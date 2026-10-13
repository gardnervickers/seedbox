use seedbox_machine::{
    driver::{Action, Driver, MachineId},
    machine::{Machine, RunContext, StopReason, Timestamp},
    pv::{Output, Request, Response, SimulationControlId},
    rng::RngStream,
};
use std::{
    num::NonZeroU32,
    sync::{Arc, Mutex},
    time::Duration,
};
#[path = "support/driver_definition.rs"]
mod driver_definition;
fn at(n: u64) -> Timestamp {
    Timestamp::from_nanos(n)
}
#[derive(Clone)]
struct Caller {
    label: usize,
    sent: bool,
    crashed: bool,
    last_factor: u32,
    operation: &'static str,
    replies: Arc<Mutex<Vec<(usize, u64, Vec<u8>)>>>,
}
impl Machine for Caller {
    type Context = ();
    type Error = std::io::Error;
    type Snapshot = Self;
    fn run(
        &mut self,
        c: &mut RunContext<'_, ()>,
        upto: Timestamp,
    ) -> Result<StopReason, Self::Error> {
        let now = c.clock.now();
        self.last_factor = c.execution.factor_permille();
        while let Some(input) = c.inbox.take() {
            match input.payload {
                Request::Crash => self.crashed = true,
                Request::Reboot => self.crashed = false,
                Request::SimulationReply { id, payload } if !self.crashed => {
                    assert_eq!(id, identity());
                    self.replies
                        .lock()
                        .unwrap()
                        .push((self.label, now, payload));
                }
                _ => {}
            }
        }
        if self.crashed {
            return Ok(StopReason::Exited {
                reached: at(now),
                reason: seedbox_machine::machine::ExitReason::Crashed,
            });
        }
        if !self.sent && upto.as_nanos() > now {
            self.sent = true;
            c.outbox
                .emit(Output {
                    at: Timestamp::from_nanos(now),
                    payload: Response::SimulationControl {
                        id: identity(),
                        payload: self.operation.as_bytes().to_vec(),
                    },
                })
                .unwrap();
        }
        if !self.sent {
            return Ok(StopReason::BoundaryReached);
        }
        Ok(StopReason::Blocked {
            reached: upto,
            deadline: None,
        })
    }
    fn snapshot(&mut self) -> Result<Self, Self::Error> {
        Ok(self.clone())
    }
}
fn identity() -> SimulationControlId {
    SimulationControlId {
        session: NonZeroU32::new(1).unwrap(),
        request: NonZeroU32::new(1).unwrap(),
    }
}
struct Rng;
impl RngStream for Rng {
    fn fill_bytes(&mut self, bytes: &mut [u8]) {
        bytes.fill(0)
    }
}
fn setup(
    workers: usize,
    operation: &'static str,
) -> (
    Driver<Caller>,
    Arc<Mutex<Vec<(usize, u64, Vec<u8>)>>>,
    Vec<MachineId>,
) {
    let replies = Arc::new(Mutex::new(vec![]));
    let captured = replies.clone();
    let mut next = 0;
    let mut driver = Driver::new(
        at(0),
        Duration::from_nanos(10),
        workers,
        Rng,
        || Ok(()),
        driver_definition::definition("same"),
        vec![driver_definition::definition("same")],
        move |_| {
            let label = next;
            next += 1;
            Ok(Caller {
                crashed: false,
                last_factor: 1000,
                label,
                sent: false,
                operation,
                replies: captured.clone(),
            })
        },
    )
    .unwrap();
    let ids = driver
        .with_barrier(|b| Ok(b.machines().map(|(id, _)| id).collect()))
        .unwrap();
    (driver, replies, ids)
}
#[test]
fn duplicate_display_names_route_identical_rpc_ids_to_exact_sources() {
    for workers in [1, 2] {
        let (mut driver, replies, _) = setup(workers, r#"{"version":1,"id":1,"op":"now"}"#);
        driver.step(at(10)).unwrap();
        let got = replies.lock().unwrap();
        assert_eq!(got.len(), 2);
        for label in [0, 1] {
            let reply = got.iter().find(|r| r.0 == label).unwrap();
            assert_eq!(reply.1, 10);
            let value: serde_json::Value = serde_json::from_slice(&reply.2).unwrap();
            assert_eq!(value["ok"], true);
            assert_eq!(value["result"]["now_ns"], 10);
        }
        drop(got);
        driver
            .with_barrier(|b| {
                let outputs = b.drain_responses();
                assert_eq!(
                    outputs
                        .iter()
                        .filter(|o| matches!(o.payload, Response::SimulationControl { .. }))
                        .count(),
                    2
                );
                Ok(())
            })
            .unwrap();
    }
}
#[test]
fn definitionless_add_and_zero_sleep_finish_due_processing_without_guest_progress() {
    let (mut driver, replies, ids) =
        setup(1, r#"{"version":1,"id":1,"op":"sleep","duration_ns":0}"#);
    let extra = driver
        .with_barrier(|b| {
            for id in &ids {
                b.schedule(at(0), Action::Pause(*id))?;
            }
            b.add(|| {
                Ok(Caller {
                    crashed: false,
                    last_factor: 1000,
                    label: 2,
                    sent: false,
                    operation: r#"{"version":1,"id":1,"op":"now"}"#,
                    replies: replies.clone(),
                })
            })
        })
        .unwrap();
    driver.step(at(10)).unwrap();
    let got = replies.lock().unwrap();
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].0, 2);
    drop(got);
    driver
        .with_barrier(|b| {
            assert!(matches!(
                b.stopped(extra)?,
                StopReason::Blocked { deadline: None, .. }
            ));
            b.schedule(at(10), Action::Resume(ids[1]))
        })
        .unwrap();
    driver.step(at(20)).unwrap();
    let got = replies.lock().unwrap();
    assert_eq!(got.len(), 2);
    let value: serde_json::Value = serde_json::from_slice(&got[1].2).unwrap();
    assert_eq!(value["ok"], true);
    assert_eq!(got[1].1, 20);
}

fn queued_pair(
    first: &'static str,
    second: &'static str,
    crash_peer: bool,
) -> (
    Driver<Caller>,
    Arc<Mutex<Vec<(usize, u64, Vec<u8>)>>>,
    Vec<MachineId>,
) {
    let replies = Arc::new(Mutex::new(vec![]));
    let observed = replies.clone();
    let mut next = 0;
    let mut driver = Driver::new(
        at(0),
        Duration::from_nanos(10),
        2,
        Rng,
        || Ok(()),
        driver_definition::definition("oracle"),
        vec![
            driver_definition::definition("caller-a"),
            driver_definition::definition("caller-b"),
            driver_definition::definition("peer"),
        ],
        move |_| {
            let label = next;
            next += 1;
            Ok(Caller {
                crashed: false,
                last_factor: 1000,
                label,
                sent: label == 0 || label == 3,
                operation: if label == 1 { first } else { second },
                replies: observed.clone(),
            })
        },
    )
    .unwrap();
    let ids = driver
        .with_barrier(|b| {
            let ids: Vec<_> = b.machines().map(|(id, _)| id).collect();
            b.schedule(at(0), Action::Pause(ids[0]))?;
            if crash_peer {
                b.schedule(at(0), Action::Crash(ids[3]))?;
            }
            Ok(ids)
        })
        .unwrap();
    (driver, replies, ids)
}
#[test]
fn same_commit_pause_revalidates_state_before_second_effect() {
    let pause = r#"{"version":1,"id":1,"op":"pause","target":{"machine":"peer"}}"#;
    let (mut driver, replies, ids) = queued_pair(pause, pause, false);
    driver.step(at(10)).unwrap();
    let got = replies.lock().unwrap();
    assert_eq!(got.len(), 2);
    let first: serde_json::Value =
        serde_json::from_slice(&got.iter().find(|r| r.0 == 1).unwrap().2).unwrap();
    let second: serde_json::Value =
        serde_json::from_slice(&got.iter().find(|r| r.0 == 2).unwrap().2).unwrap();
    assert_eq!(first["ok"], true);
    assert_eq!(second["ok"], false);
    assert_eq!(second["error"]["code"], "invalid_state");
    driver
        .with_barrier(|b| {
            assert!(b.paused(ids[3])?);
            Ok(())
        })
        .unwrap();
}
#[test]
fn reboot_before_queued_throttle_settles_old_boot_instead_of_retargeting() {
    let reboot = r#"{"version":1,"id":1,"op":"reboot","target":{"machine":"peer"}}"#;
    let throttle = r#"{"version":1,"id":1,"op":"throttle","target":{"machine":"peer"},"factor_permille":2000}"#;
    let (mut driver, replies, ids) = queued_pair(reboot, throttle, true);
    driver.step(at(10)).unwrap();
    let got = replies.lock().unwrap();
    assert_eq!(got.len(), 2);
    let first: serde_json::Value =
        serde_json::from_slice(&got.iter().find(|r| r.0 == 1).unwrap().2).unwrap();
    let second: serde_json::Value =
        serde_json::from_slice(&got.iter().find(|r| r.0 == 2).unwrap().2).unwrap();
    assert_eq!(first["ok"], true);
    assert_eq!(second["ok"], false);
    assert_eq!(second["error"]["code"], "target_unavailable");
    drop(got);
    driver
        .with_barrier(|b| {
            assert_eq!(b.machine(ids[3])?.last_factor, 1000);
            assert!(matches!(b.stopped(ids[3])?, StopReason::Blocked { .. }));
            Ok(())
        })
        .unwrap();
}
#[test]
fn unmatched_trigger_expiry_quiesces_without_a_repeated_zero_time_marker() {
    let arm = r#"{"version":1,"id":1,"op":"arm_trigger","source":{"machine":"peer"},"marker":[120],"match_timeout_ns":1,"fault":{"action":"stop","target":{"machine":"peer"}}}"#;
    let (mut driver, replies, _) = queued_pair(arm, r#"{"version":1,"id":1,"op":"now"}"#, false);
    driver.step(at(10)).unwrap();
    let got = replies.lock().unwrap();
    let reply: serde_json::Value =
        serde_json::from_slice(&got.iter().find(|r| r.0 == 1).unwrap().2).unwrap();
    assert_eq!(reply["ok"], true, "{reply}");
    drop(got);
    driver.step(at(11)).unwrap();
    assert!(matches!(
        driver.step(at(20)).unwrap(),
        seedbox_machine::driver::StepOutcome::Blocked { .. }
    ));
}
