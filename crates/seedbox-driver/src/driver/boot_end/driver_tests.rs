//! Private Driver identity/staging/codec invariants using a host-only Machine.
//! RAM below is real mock-owned bytes, not a claim of native acceptance.
use super::*;
use crate::driver::fixture;
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, io};

#[derive(Clone, Copy, Debug)]
enum Mode {
    Busy,
    NaturalExit,
    ProgramExit(i32),
    IgnoreCrash,
    DeferredCrash,
    Bad(u64, bool),
    Duplicate,
}
#[derive(Clone, Debug)]
struct Mock {
    mode: Mode,
    boot: u64,
    ram: Option<Vec<u8>>,
    ended: bool,
    reason: ExitReason,
    first_crash: bool,
    pending: Option<Response>,
}
impl Mock {
    fn new(mode: Mode) -> Self {
        Self {
            mode,
            boot: 0,
            ram: None,
            ended: false,
            reason: ExitReason::Crashed,
            first_crash: true,
            pending: None,
        }
    }
    fn retire(&mut self, disposition: EvidenceDisposition) -> Response {
        let retired_state_digest = self.ram.as_ref().map(|ram| hex(&Sha256::digest(ram)));
        let response = Response::BootEnded {
            ended_boot: self.boot,
            disposition,
            native_booted: self.ram.is_some(),
            disk_lost_sectors: 0,
            surviving_disk: None,
            retired_state_digest,
        };
        self.ram = None;
        self.ended = true;
        self.reason = match disposition {
            EvidenceDisposition::Crashed => ExitReason::Crashed,
            EvidenceDisposition::Shutdown => ExitReason::Shutdown,
            EvidenceDisposition::Exited { code } => ExitReason::ProgramExit { code },
            EvidenceDisposition::Live => panic!("fixture cannot retire a live disposition"),
        };
        response
    }
    fn emit(context: &mut RunContext<'_, ()>, response: Response) {
        context
            .outbox
            .emit(Output {
                at: Timestamp::from_nanos(context.clock.now()),
                payload: response,
            })
            .unwrap();
    }
}
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|v| format!("{v:02x}")).collect()
}
impl Machine for Mock {
    type Context = ();
    type Error = io::Error;
    type Snapshot = Self;
    fn run(
        &mut self,
        context: &mut RunContext<'_, ()>,
        upto: Timestamp,
    ) -> Result<StopReason, io::Error> {
        let now = Timestamp::from_nanos(context.clock.now());
        if matches!(self.mode, Mode::IgnoreCrash)
            && context
                .inbox
                .front()
                .is_some_and(|i| matches!(i.payload, Request::Crash))
        {
            let report = self.retire(EvidenceDisposition::Crashed);
            Self::emit(context, report);
            return Ok(StopReason::BoundaryReached); // deliberately did not consume the request
        }
        while let Some(input) = context.inbox.front() {
            match input.payload {
                Request::Crash => {
                    context.inbox.take();
                    if !self.ended {
                        let report = self.retire(EvidenceDisposition::Crashed);
                        Self::emit(context, report);
                    }
                    return Ok(StopReason::Exited {
                        reached: now,
                        reason: ExitReason::Crashed,
                    });
                }
                Request::Reboot => {
                    context.inbox.take();
                    if let Some(report) = self.pending.take() {
                        Self::emit(context, report);
                    }
                    if matches!(self.mode, Mode::Duplicate) {
                        Self::emit(
                            context,
                            Response::BootEnded {
                                ended_boot: self.boot,
                                disposition: EvidenceDisposition::Crashed,
                                native_booted: false,
                                disk_lost_sectors: 0,
                                surviving_disk: None,
                                retired_state_digest: None,
                            },
                        );
                    }
                    self.boot += 1;
                    self.ended = false;
                    self.ram = None;
                    Self::emit(
                        context,
                        Response::BootConsole {
                            bytes: vec![self.boot as u8],
                        },
                    );
                }
                _ => {
                    context.inbox.take();
                }
            }
        }
        if self.ended {
            return Ok(StopReason::Exited {
                reached: now,
                reason: self.reason,
            });
        }
        if self.ram.is_none() {
            self.ram = Some(vec![self.boot as u8; 17]);
            Self::emit(context, Response::Ready);
        }
        if upto == now {
            return Ok(StopReason::BoundaryReached);
        }
        match self.mode {
            Mode::NaturalExit => {
                let report = self.retire(EvidenceDisposition::Shutdown);
                Self::emit(context, report);
                Ok(StopReason::Exited {
                    reached: now,
                    reason: ExitReason::Shutdown,
                })
            }
            Mode::ProgramExit(code) => {
                let report = self.retire(EvidenceDisposition::Exited { code });
                Self::emit(context, report);
                Ok(StopReason::Exited {
                    reached: now,
                    reason: ExitReason::ProgramExit { code },
                })
            }
            Mode::DeferredCrash if self.first_crash => {
                self.first_crash = false;
                self.pending = Some(self.retire(EvidenceDisposition::Crashed));
                Ok(StopReason::Exited {
                    reached: now,
                    reason: ExitReason::Crashed,
                })
            }
            Mode::Bad(boot, exited) => {
                let mut report = self.retire(EvidenceDisposition::Crashed);
                if let Response::BootEnded { ended_boot, .. } = &mut report {
                    *ended_boot = boot;
                }
                Self::emit(context, report);
                if exited {
                    Ok(StopReason::Exited {
                        reached: now,
                        reason: ExitReason::Crashed,
                    })
                } else {
                    self.ended = false;
                    Ok(StopReason::BoundaryReached)
                }
            }
            _ => Ok(StopReason::BoundaryReached),
        }
    }
    fn snapshot(&mut self) -> Result<Self, io::Error> {
        Ok(self.clone())
    }
}
fn make(mode: Mode, workers: usize, members: usize) -> Driver<Mock> {
    let mut machines = std::collections::VecDeque::from([Mock::new(Mode::Busy)]);
    machines.extend((0..members).map(|_| Mock::new(mode)));
    Driver::new_seeded(
        Timestamp::from_nanos(0),
        Duration::from_nanos(10),
        workers,
        42,
        || Ok(()),
        fixture::definition(0),
        (1..=members).map(fixture::definition).collect(),
        Vec::new(),
        NetworkConfig::default(),
        move |_| Ok(machines.pop_front().unwrap()),
    )
    .unwrap()
}
fn step(d: &mut Driver<Mock>, at: u64) {
    while d.now().as_nanos() < at {
        d.step(Timestamp::from_nanos(at)).unwrap();
    }
}
fn schedule(d: &mut Driver<Mock>, at: u64, key: &str, action: Action) {
    d.with_barrier(|b| {
        b.schedule(
            Timestamp::from_nanos(at),
            InputKey::new(key).unwrap(),
            action,
        )
    })
    .unwrap();
}
fn outputs(d: &mut Driver<Mock>) -> Vec<MachineResponse> {
    fixture::outputs(&d.drain_events())
}

#[test]
fn an_offered_but_unconsumed_crash_is_not_end_proof() {
    let mut d = make(Mode::IgnoreCrash, 1, 1);
    d.drain_events();
    schedule(
        &mut d,
        0,
        "ignored-crash",
        Action::Crash(Target::Machine(MachineId(1))),
    );
    assert!(matches!(
        d.step(Timestamp::from_nanos(10)),
        Err(Error::InvalidBootEnd { .. })
    ));
    assert_eq!(d.execution.machines[1].last_ended_published, None);
    assert!(!outputs(&mut d)
        .iter()
        .any(|o| matches!(o.payload, Response::BootEnded { .. })));
}

#[test]
fn natural_exit_and_program_exit_payloads_survive_the_closed_codec() {
    for mode in [Mode::NaturalExit, Mode::ProgramExit(17)] {
        let mut d = make(mode, 1, 1);
        step(&mut d, 10);
        let c = roundtrip(d.checkpoint().unwrap());
        let expected = d.drain_events();
        let mut restored = Driver::<Mock>::from_checkpoint(
            &c,
            1,
            || Ok(()),
            |s| Ok(s.clone()),
            |_| Err(io::Error::other("unexpected spawn")),
        )
        .unwrap();
        assert_eq!(restored.drain_events(), expected);
        assert_eq!(restored.execution.machines[1].last_ended_published, Some(0));
    }
}

#[test]
fn natural_exit_reports_initial_zero_and_preserves_ordinary_stamping() {
    let mut d = make(Mode::NaturalExit, 1, 1);
    step(&mut d, 10);
    let out = outputs(&mut d);
    assert!(out.iter().any(|o| matches!(
        o.payload,
        Response::BootEnded {
            ended_boot: 0,
            native_booted: true,
            disposition: EvidenceDisposition::Shutdown,
            ..
        }
    ) && o.boot == 0));
    assert!(out
        .iter()
        .filter(|o| o.machine == MachineId(1))
        .all(|o| o.boot == 0));
    assert_eq!(d.execution.machines[1].last_ended_published, Some(0));
}
#[test]
fn crash_before_initial_native_start_has_no_fabricated_ram() {
    let mut d = make(Mode::Busy, 1, 1);
    schedule(
        &mut d,
        0,
        "prestart-crash",
        Action::Crash(Target::Machine(MachineId(1))),
    );
    step(&mut d, 10);
    assert!(outputs(&mut d).iter().any(|o| matches!(
        o.payload,
        Response::BootEnded {
            ended_boot: 0,
            native_booted: false,
            retired_state_digest: None,
            disposition: EvidenceDisposition::Crashed,
            ..
        }
    )));
    assert_eq!(d.execution.machines[1].last_ended_published, Some(0));
}
#[test]
fn reboot_payload_names_old_boot_but_every_carrier_keeps_prospective_boot() {
    let mut d = make(Mode::DeferredCrash, 1, 1);
    step(&mut d, 10);
    assert_eq!(d.execution.machines[1].last_ended_published, None);
    d.drain_events();
    schedule(
        &mut d,
        10,
        "reboot",
        Action::Reboot(Target::Machine(MachineId(1))),
    );
    step(&mut d, 20);
    let out = outputs(&mut d);
    let out: Vec<_> = out.iter().filter(|o| o.machine == MachineId(1)).collect();
    assert!(out
        .iter()
        .any(|o| matches!(o.payload, Response::BootEnded { ended_boot: 0, .. })));
    assert!(out
        .iter()
        .any(|o| matches!(o.payload, Response::Ready | Response::BootConsole { .. })));
    assert!(
        out.iter().all(|o| o.boot == 1),
        "generic carrier stamping must not be rewritten"
    );
    assert_eq!(d.execution.machines[1].last_ended_published, Some(0));
}
#[test]
fn future_arbitrary_noend_and_past_identities_poison_without_publication() {
    for mode in [
        Mode::Bad(1, true),
        Mode::Bad(u64::MAX, true),
        Mode::Bad(0, false),
    ] {
        let mut d = make(mode, 1, 1);
        d.drain_events();
        assert!(matches!(
            d.step(Timestamp::from_nanos(10)),
            Err(Error::InvalidBootEnd { .. })
        ));
        assert!(outputs(&mut d).is_empty());
        assert_eq!(d.execution.machines[1].last_ended_published, None);
        assert!(matches!(d.with_barrier(|_| Ok(())), Err(Error::Failed)));
    }
    let mut d = make(Mode::DeferredCrash, 1, 1);
    step(&mut d, 10);
    schedule(
        &mut d,
        10,
        "reboot",
        Action::Reboot(Target::Machine(MachineId(1))),
    );
    step(&mut d, 20);
    d.execution.machines[1].machine.as_mut().unwrap().mode = Mode::Bad(0, true);
    d.drain_events();
    assert!(matches!(
        d.step(Timestamp::from_nanos(30)),
        Err(Error::InvalidBootEnd { .. })
    ));
    assert!(outputs(&mut d).is_empty());
}
#[test]
fn exit_then_reboot_cannot_publish_the_same_ended_boot_again() {
    let mut d = make(Mode::Busy, 1, 1);
    schedule(
        &mut d,
        0,
        "crash",
        Action::Crash(Target::Machine(MachineId(1))),
    );
    step(&mut d, 10);
    assert_eq!(d.execution.machines[1].last_ended_published, Some(0));
    d.execution.machines[1].machine.as_mut().unwrap().mode = Mode::Duplicate;
    d.drain_events();
    schedule(
        &mut d,
        10,
        "reboot",
        Action::Reboot(Target::Machine(MachineId(1))),
    );
    assert!(matches!(
        d.step(Timestamp::from_nanos(20)),
        Err(Error::InvalidBootEnd { .. })
    ));
    assert!(!outputs(&mut d)
        .iter()
        .any(|o| matches!(o.payload, Response::BootEnded { .. })));
    assert!(matches!(d.checkpoint(), Err(Error::Failed)));
}
fn limits() -> DecodeLimits {
    DecodeLimits {
        encoded_bytes: 1 << 20,
        allocation_bytes: 8 << 20,
        collection_items: 1 << 16,
        total_items: 1 << 18,
        string_bytes: 1 << 16,
        blob_bytes: 1 << 20,
        depth: 128,
        archive_entries: 1000,
        archive_path_bytes: 1 << 20,
        archive_expanded_bytes: 1 << 20,
    }
}
fn roundtrip(c: Checkpoint<Mock>) -> Checkpoint<Mock> {
    let mut bodies = BTreeMap::new();
    let refs = c
        .try_map_snapshots(|snapshot| {
            let bytes = format!("{snapshot:?}").into_bytes(); // explicit host-fixture encoding
            let digest: [u8; 32] = Sha256::digest(&bytes).into();
            bodies.insert(digest, (bytes.len() as u64, snapshot));
            Ok::<_, io::Error>(SnapshotReference::new(digest, bytes.len() as u64))
        })
        .unwrap();
    let mut bytes = Vec::new();
    refs.write_to(&mut bytes).unwrap();
    let refs = Checkpoint::read_from(&mut &bytes[..], limits()).unwrap();
    refs.try_map_snapshots(|reference| {
        let (length, snapshot) = &bodies[reference.digest()];
        assert_eq!(*length, reference.encoded_length());
        assert_eq!(
            &<[u8; 32]>::from(Sha256::digest(format!("{snapshot:?}").as_bytes())),
            reference.digest()
        );
        Ok::<_, io::Error>(snapshot.clone())
    })
    .unwrap()
}
#[test]
fn cursor_codec_and_mock_continuation_match_one_and_four_workers() {
    let mut source = make(Mode::DeferredCrash, 4, 3);
    step(&mut source, 10);
    for id in 1..=3 {
        schedule(
            &mut source,
            10,
            &format!("reboot-{id}"),
            Action::Reboot(Target::Machine(MachineId(id))),
        );
    }
    step(&mut source, 20);
    let c = roundtrip(source.checkpoint().unwrap());
    assert!(source
        .execution
        .machines
        .iter()
        .skip(1)
        .all(|e| e.last_ended_published == Some(0)));
    let prefix = source.drain_events();
    let continue_driver = |d: &mut Driver<Mock>| {
        for id in 1..=3 {
            schedule(
                d,
                30,
                &format!("crash-{id}"),
                Action::Crash(Target::Machine(MachineId(id))),
            );
        }
        step(d, 40);
        d.drain_events()
    };
    let expected = continue_driver(&mut source);
    for workers in [1, 4] {
        let mut restored = Driver::<Mock>::from_checkpoint(
            &c,
            workers,
            || Ok(()),
            |s| Ok(s.clone()),
            |_| Err(io::Error::other("unexpected spawn")),
        )
        .unwrap();
        assert!(restored
            .execution
            .machines
            .iter()
            .skip(1)
            .all(|e| e.last_ended_published == Some(0)));
        assert_eq!(restored.drain_events(), prefix);
        assert_eq!(continue_driver(&mut restored), expected);
        assert!(restored
            .execution
            .machines
            .iter()
            .skip(1)
            .all(|e| e.last_ended_published == Some(1)));
    }
}
#[test]
fn duplicate_after_closed_restore_poisoned_without_new_publication() {
    let mut source = make(Mode::Busy, 1, 1);
    schedule(
        &mut source,
        0,
        "crash",
        Action::Crash(Target::Machine(MachineId(1))),
    );
    step(&mut source, 10);
    source.drain_events();
    let c = roundtrip(source.checkpoint().unwrap());
    let mut restored = Driver::<Mock>::from_checkpoint(
        &c,
        1,
        || Ok(()),
        |s| {
            let mut m = s.clone();
            if m.ended {
                m.mode = Mode::Duplicate;
            }
            Ok(m)
        },
        |_| Err(io::Error::other("unexpected spawn")),
    )
    .unwrap();
    schedule(
        &mut restored,
        10,
        "reboot",
        Action::Reboot(Target::Machine(MachineId(1))),
    );
    assert!(matches!(
        restored.step(Timestamp::from_nanos(20)),
        Err(Error::InvalidBootEnd { .. })
    ));
    assert!(!outputs(&mut restored)
        .iter()
        .any(|o| matches!(o.payload, Response::BootEnded { .. })));
}
