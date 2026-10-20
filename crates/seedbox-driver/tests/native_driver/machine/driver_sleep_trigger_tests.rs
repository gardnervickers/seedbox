//! Native Driver/service evidence with private backend snapshot reconstruction.
//! Restoring machines in place keeps Driver-owned time, queues, control state and
//! RNG cursors intact; this is not a whole-simulation snapshot API.
use super::auto_key;
use crate::{
    driver::{Action, Driver, MachineId, MachineResponse, StepOutcome},
    machine::{ExitReason, Machine, RunContext, StopReason, Timestamp},
    pv::{CommandResult, Response, Stream},
    rng::RngStream,
};
use seedbox_driver::driver::Target;
use seedbox_test_time::{GuestProgress, HangGuard};
use std::{fmt::Debug, fs, path::PathBuf, time::Duration};
const QUANTUM: u64 = 5_000_000;
fn at(n: u64) -> Timestamp {
    Timestamp::from_nanos(n)
}
fn input(key: &str) -> PathBuf {
    std::env::var_os(key).expect(key).into()
}
struct FixtureRng(u8);
impl RngStream for FixtureRng {
    fn fill_bytes(&mut self, bytes: &mut [u8]) {
        for byte in bytes {
            *byte = self.0;
            self.0 = self.0.wrapping_add(1);
        }
    }
}
struct Migrating<M, F> {
    inner: M,
    restore: F,
}
impl<M, F> Machine for Migrating<M, F>
where
    M: Machine<Context = ()>,
    F: Fn(&M::Snapshot) -> Result<M, M::Error>,
{
    type Context = ();
    type Error = M::Error;
    type Snapshot = M::Snapshot;
    fn run(
        &mut self,
        context: &mut RunContext<'_, ()>,
        upto: Timestamp,
    ) -> Result<StopReason, Self::Error> {
        self.inner.run(context, upto)
    }
    fn snapshot(&mut self) -> Result<Self::Snapshot, Self::Error> {
        let saved = self.inner.snapshot()?;
        self.inner = (self.restore)(&saved)?;
        Ok(saved)
    }
}
fn output(events: &[MachineResponse], machine: MachineId, stream: Stream) -> Vec<u8> {
    events
        .iter()
        .filter_map(|event| match &event.payload {
            Response::ApplicationOutput {
                stream: actual,
                bytes,
            } if event.machine == machine && *actual == stream => Some(bytes.as_slice()),
            _ => None,
        })
        .flatten()
        .copied()
        .collect()
}
/// Bound on reply admission to the application's complete read, as observed
/// by the stdout write that immediately follows it. The program waits idle in a
/// blocking read, so this covers interrupt delivery, the init relay and the
/// fixture's one-byte reads of a ~170-byte reply. Both backends measured about
/// 0.22-0.25 ms at the default execution rate; a missed wake waits for an
/// unrelated guest timer instead (about 400 ms here before the TCG fix).
const REPLY_READ_BOUND_NS: u64 = 1_000_000;
/// Occurrence time of the output fragment holding each `RPC_REPLY ` marker. The
/// fixture writes that marker only after reading its whole reply, so these
/// times bound the read completion from above.
fn reply_read_times(events: &[MachineResponse], machine: MachineId) -> Vec<u64> {
    let mut text = Vec::new();
    let mut fragments = Vec::new();
    for event in events {
        if let Response::ApplicationOutput {
            stream: Stream::Stdout,
            bytes,
        } = &event.payload
        {
            if event.machine == machine {
                fragments.push((text.len(), event.at.as_nanos()));
                text.extend_from_slice(bytes);
            }
        }
    }
    text.windows(10)
        .enumerate()
        .filter(|(_, window)| *window == b"RPC_REPLY ")
        .map(|(offset, _)| {
            fragments
                .iter()
                .rev()
                .find(|(start, _)| *start <= offset)
                .unwrap()
                .1
        })
        .collect()
}
fn scenario<M>(
    backend: &str,
    workers: usize,
    migrate: bool,
    mut construct: impl FnMut(&crate::definition::MachineDefinition) -> M + Send + 'static,
) -> (Vec<StepOutcome>, Vec<MachineResponse>)
where
    M: Machine<Context = ()> + Send + 'static,
    M::Error: Debug + Send + 'static,
{
    let application = fs::read(input(&format!(
        "SEEDBOX_{backend}_DRIVER_CONTROL_APPLICATION"
    )))
    .unwrap();
    let mut tar = tar::Builder::new(Vec::new());
    let mut header = tar::Header::new_gnu();
    header.set_size(application.len() as u64);
    header.set_mode(0o755);
    header.set_uid(0);
    header.set_gid(0);
    header.set_mtime(0);
    header.set_cksum();
    tar.append_data(&mut header, "application", &application[..])
        .unwrap();
    let oracle = crate::definition::MachineDefinition {
        name: "oracle".into(),
        resources: crate::definition::Resources {
            memory_bytes: if backend == "PV" {
                128 << 20
            } else {
                256 << 20
            },
        },
        application: crate::definition::PreparedApplication {
            filesystem: crate::definition::FilesystemArchive::new(tar.into_inner().unwrap())
                .unwrap(),
            launch: crate::definition::Launch {
                argv: vec!["/application".into(), "sleep-trigger-oracle".into()],
                environment: Default::default(),
                working_directory: "/".into(),
                user: "0:0".into(),
            },
        },
        disk: None,
        network: None,
    };
    let mut peer = oracle.clone();
    peer.name = "peer".into();
    peer.application.launch.argv = vec!["/application".into(), "peer".into()];
    let mut driver = Driver::new(
        at(0),
        Duration::from_nanos(QUANTUM),
        workers,
        FixtureRng(19),
        || Ok(()),
        oracle,
        vec![peer],
        Vec::new(),
        crate::driver::NetworkConfig::default(),
        move |definition| Ok(construct(definition)),
    )
    .unwrap();
    let (oracle, peer) = driver
        .with_barrier(|b| {
            let ids: Vec<_> = b.machines().map(|(id, _)| id).collect();
            b.schedule(at(0), auto_key(), Action::Pause(Target::Machine(ids[0])))?;
            Ok((ids[0], ids[1]))
        })
        .unwrap();
    let guard = HangGuard::new(
        "native sleep/trigger transport must return",
        Duration::from_secs(180),
    );
    // Public native acceptance's 40s TCG boot allowance, for the same kernel
    // and static service fixture. Initial pause admission and settlement may
    // use two same-instant rounds before peer execution advances.
    let mut progress =
        GuestProgress::new("peer boot", Duration::ZERO, Duration::from_secs(40), 2).unwrap();
    let mut control_phase = false;
    let mut events = Vec::new();
    let mut outcomes = Vec::new();
    let mut resumed = false;
    let mut sleep_checkpoint = false;
    let mut marker_checkpoint = false;
    loop {
        guard
            .remaining()
            .expect("sleep/trigger infrastructure hang");
        let now = driver
            .with_barrier(|b| Ok(b.playback().position.at))
            .unwrap();
        let limit = progress
            .next(
                Duration::from_nanos(now.as_nanos()),
                Duration::from_nanos(QUANTUM),
            )
            .unwrap_or_else(|error| {
                panic!(
                    "{}: {error}; at={now:?}; outcomes={:?}",
                    progress.phase(),
                    outcomes.iter().rev().take(8).collect::<Vec<_>>()
                )
            });
        let result = driver
            .step(at(limit.as_nanos().try_into().unwrap()))
            .unwrap();
        outcomes.push(result.clone());
        events.extend(
            driver
                .drain_observations()
                .events
                .into_iter()
                .filter_map(|event| match event.kind {
                    crate::driver::EventKind::Output(output) => Some(output),
                    _ => None,
                }),
        );
        if !resumed && output(&events, peer, Stream::Stdout) == b"PEER_RUNNING\n" {
            driver
                .with_barrier(|b| {
                    b.schedule(
                        b.playback().position.at,
                        auto_key(),
                        Action::Resume(Target::Machine(oracle)),
                    )
                })
                .unwrap();
            resumed = true;
            let start = driver
                .with_barrier(|b| Ok(b.playback().position.at))
                .unwrap()
                .as_nanos();
            // Oracle was initially paused before boot. Its own boot receives
            // the same image-derived allowance, reset ONCE at semantic Resume.
            progress = GuestProgress::new(
                "oracle boot",
                Duration::from_nanos(start),
                Duration::from_nanos(start) + Duration::from_secs(40),
                2,
            )
            .unwrap();
        }
        let stdout = output(&events, oracle, Stream::Stdout);
        let text = std::str::from_utf8(&stdout).unwrap();
        if !control_phase
            && events.iter().any(|event| {
                event.machine == oracle
                    && matches!(event.payload, Response::SimulationControl { .. })
            })
        {
            let start = driver
                .with_barrier(|b| Ok(b.playback().position.at))
                .unwrap()
                .as_nanos();
            // driver-simulation-control.c: 100ms trigger match window, two
            // 20ms sleeps, four reply rounds and the Stop's 5s command deadline.
            // Same-instant edges: four RPC admission/replies, trigger arm/match,
            // and Stop admission/ACK/settlement (8 + 2 + 3 = 13).
            let work = Duration::from_millis(100 + 2 * 20)
                + Duration::from_nanos(4 * QUANTUM)
                + Duration::from_secs(5);
            progress = GuestProgress::new(
                "oracle sleep/trigger outcome",
                Duration::from_nanos(start),
                Duration::from_nanos(start) + work,
                13,
            )
            .unwrap();
            control_phase = true;
        }
        if !sleep_checkpoint && events.iter().any(|event| event.machine == oracle &&
            matches!(event.payload, Response::SimulationControl { id, .. } if id.request.get() == 2)) {
            assert!(!text.contains("\"id\":2,"), "positive Sleep must still be pending at admission");
            if migrate { driver.with_barrier(|b| b.snapshot(oracle)).unwrap(); }
            sleep_checkpoint = true;
        }
        if !marker_checkpoint && text.ends_with("nee") {
            assert!(
                !events.iter().any(|event| event.machine == peer
                    && matches!(event.payload, Response::CommandAcknowledged { .. })),
                "a marker prefix must not trigger a Stop"
            );
            if migrate {
                driver.with_barrier(|b| b.snapshot(oracle)).unwrap();
            }
            marker_checkpoint = true;
        }
        if let StepOutcome::Finished { reason, .. } = result {
            assert_eq!(reason, ExitReason::ProgramExit { code: 0 });
            break;
        }
        assert!(outcomes.len() < 20000);
    }
    assert!(resumed && sleep_checkpoint && marker_checkpoint);
    assert!(output(&events, oracle, Stream::Stderr).is_empty());
    assert!(output(&events, peer, Stream::Stderr).is_empty());
    assert_eq!(output(&events, peer, Stream::Stdout), b"PEER_RUNNING\n");
    let stdout = output(&events, oracle, Stream::Stdout);
    let text = std::str::from_utf8(&stdout).unwrap();
    assert!(text.ends_with("DONE\n"), "{text}");
    assert_eq!(text.matches("needle\n").count(), 1);
    // The trigger spans distinct guest writes separated by a real Linux timer
    // and at least one Driver barrier (the checkpoint above).
    let calls: Vec<_> = events
        .iter()
        .filter(|event| {
            event.machine == oracle && matches!(event.payload, Response::SimulationControl { .. })
        })
        .collect();
    let replies: Vec<serde_json::Value> = text
        .lines()
        .filter_map(|line| line.strip_prefix("RPC_REPLY "))
        .map(|json| serde_json::from_str(json).unwrap())
        .collect();
    let reads = reply_read_times(&events, oracle);
    assert_eq!(calls.len(), 4);
    assert_eq!(replies.len(), 4);
    assert_eq!(reads.len(), 4);
    for (index, (call, reply)) in calls.iter().zip(&replies).enumerate() {
        let Response::SimulationControl { id, payload } = &call.payload else {
            unreachable!()
        };
        assert_eq!(id.session.get(), 1);
        assert_eq!(id.request.get() as usize, index + 1);
        let request: serde_json::Value = serde_json::from_slice(payload).unwrap();
        assert_eq!(
            request["op"],
            ["arm_trigger", "sleep", "sleep", "now"][index]
        );
        assert_eq!(reply["id"], index + 1);
        assert_eq!(reply["ok"], true, "{reply}");
        assert_eq!(reply["requested_at_ns"], call.at.as_nanos());
        let admitted = reply["admitted_at_ns"].as_u64().unwrap();
        let replied = reply["replied_at_ns"].as_u64().unwrap();
        assert!(call.at.as_nanos() <= admitted && admitted <= replied);
        // Idle-wait delivery: a positive Sleep reply generated at its wake and
        // an immediate reply are both read promptly after admission.
        assert!(
            replied <= reads[index] && reads[index] - replied < REPLY_READ_BOUND_NS,
            "reply {} admitted at {replied} but read by {}",
            index + 1,
            reads[index]
        );
        if index == 1 || index == 2 {
            assert_eq!(reply["effect_at_ns"], admitted + 20_000_000);
            assert_eq!(replied, admitted + 20_000_000);
        }
    }
    let acks: Vec<_> = events
        .iter()
        .filter(|event| {
            event.machine == peer
                && matches!(
                    event.payload,
                    Response::CommandAcknowledged {
                        result: CommandResult::Completed,
                        ..
                    }
                )
        })
        .collect();
    assert_eq!(
        acks.len(),
        1,
        "trigger must Stop exactly once: {text}; calls={calls:?}"
    );
    let marker_end = events.iter().find(|event| event.machine == oracle &&
        matches!(&event.payload, Response::ApplicationOutput { bytes, .. } if bytes.windows(3).any(|w| w == b"dle"))).unwrap().at;
    assert!(acks[0].at >= marker_end);
    driver
        .with_barrier(|b| {
            assert!(!matches!(b.stopped(peer)?, StopReason::Exited { .. }));
            Ok(())
        })
        .unwrap();
    (outcomes, events)
}
fn tcg(workers: usize, migrate: bool) -> (Vec<StepOutcome>, Vec<MachineResponse>) {
    let qemu = input("SEEDBOX_TCG_QEMU");
    let mut factory = super::tcg::Factory::new(
        &qemu,
        &input("SEEDBOX_TCG_ASSETS"),
        crate::machine::TEST_EPOCH_SECONDS,
    )
    .unwrap();
    scenario("TCG", workers, migrate, move |definition| {
        let qemu = qemu.clone();
        Migrating {
            inner: factory.create(definition).unwrap(),
            restore: move |saved: &super::tcg::Snapshot| {
                super::tcg::TcgMachine::from_snapshot(&qemu, saved)
            },
        }
    })
}
#[test]
#[ignore = "requires native TCG kernel/helper and driver-control application"]
fn tcg_driver_guest_sleep_and_trigger_match_workers_and_snapshot() {
    let serial = tcg(1, false);
    for workers in [1, 2, 4] {
        if workers != 1 {
            assert_eq!(serial, tcg(workers, false), "workers={workers}");
        }
        assert_eq!(serial, tcg(workers, true), "restored workers={workers}");
    }
}
#[cfg(seedbox_pv)]
fn pv(workers: usize, migrate: bool) -> (Vec<StepOutcome>, Vec<MachineResponse>) {
    let worker = input("SEEDBOX_PV_WORKER");
    let mut factory = super::pv::Factory::new(
        &worker,
        &input("SEEDBOX_PV_ASSETS"),
        crate::machine::TEST_EPOCH_SECONDS,
    )
    .unwrap();
    scenario("PV", workers, migrate, move |definition| {
        let worker = worker.clone();
        Migrating {
            inner: factory.create(definition).unwrap(),
            restore: move |saved: &super::pv::Snapshot| {
                super::pv::PVMachine::from_snapshot(&worker, saved).map_err(std::io::Error::other)
            },
        }
    })
}
#[cfg(seedbox_pv)]
#[test]
#[ignore = "requires native PV hypervisor/kernel/helper and driver-control application"]
fn pv_driver_guest_sleep_and_trigger_match_workers_and_snapshot() {
    let serial = pv(1, false);
    for workers in [1, 2, 4] {
        if workers != 1 {
            assert_eq!(serial, pv(workers, false), "workers={workers}");
        }
        assert_eq!(serial, pv(workers, true), "restored workers={workers}");
    }
}
