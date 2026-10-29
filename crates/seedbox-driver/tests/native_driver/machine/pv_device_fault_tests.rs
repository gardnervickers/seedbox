//! Disk and scheduler faults and a held block request on real Linux under
//! native PV, through the Driver: each fault takes effect in the guest, heals
//! settle with the device's counts, and the run is identical across worker
//! counts, machine migration through snapshots and a Driver checkpoint.
use crate::{
    driver::{
        faults, Action, Cause, Driver, Event, EventKind, FaultAction, FaultHandle, FaultOrigin,
        HealTarget, InputKey, MachineId, StepOutcome, Target, Transition,
    },
    machine::{Machine, RunContext, StopReason, Timestamp},
    pv::{self as contract, Response},
};
use seedbox_driver::driver::TriggerResult;
use seedbox_machine::{
    definition::DiskImage,
    test_support::disk_scenario::{at, definition, Backend, HOST_LIMIT, QUANTUM},
};
use seedbox_machine_pv::machine::pv::{Factory, PVMachine, Snapshot};
use seedbox_test_time::HangGuard;
use std::{path::PathBuf, time::Duration};

const ROUNDS: usize = 8;
const LATENCY_NS: u64 = 20_000_000;
/// Simulated bound for every machine to report.
const LIMIT_NS: u64 = 120_000_000_000;

fn input(key: &str) -> PathBuf {
    std::env::var_os(key).expect(key).into()
}
fn backend() -> Backend<PVMachine> {
    Backend {
        factory: || {
            let mut factory = Factory::new(
                &input("SEEDBOX_PV_WORKER"),
                &input("SEEDBOX_PV_ASSETS"),
                crate::machine::TEST_EPOCH_SECONDS,
            )
            .unwrap();
            Box::new(move |definition| factory.create(definition))
        },
        restore: restore_pv,
        store: |_| None,
        probe: input("SEEDBOX_PV_DISK_PROBE"),
        image: input("SEEDBOX_PV_DISK_IMAGE"),
        memory_bytes: 128 << 20,
    }
}
fn restore_pv(snapshot: &Snapshot) -> std::io::Result<PVMachine> {
    PVMachine::from_snapshot(&input("SEEDBOX_PV_WORKER"), snapshot).map_err(std::io::Error::other)
}

/// A PV machine whose snapshot can also replace it with a copy restored from
/// that snapshot, so the Driver continues on the restored machine. The
/// oracle's stands in for a guest oracle: on its first run it calls
/// `arm_trigger`, and it drops the reply its guest never asked for.
struct Migrating {
    machine: PVMachine,
    migrate: bool,
    call: Option<Vec<u8>>,
}
/// The oracle's call: crash the held machine on its third FLUSH.
fn arm_held() -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
        "version": 1, "id": 1, "op": "arm_trigger",
        "source": {"machine": "held"},
        "block_request": {"op": "flush", "nth": 3},
        "match_timeout_ns": LIMIT_NS / 2,
        "fault": {"action": "crash", "target": {"machine": "held"}},
    }))
    .unwrap()
}
fn wrap(machine: PVMachine, migrate: bool, name: &str) -> Migrating {
    Migrating {
        machine,
        migrate,
        call: (name == "oracle").then(arm_held),
    }
}
/// The engine these machines run, which provides held block requests.
fn engine() -> faults::Engine {
    faults::Engine {
        backend: seedbox_spec::backend::CpuBackend::Pv,
        architecture: if cfg!(target_arch = "aarch64") {
            seedbox_spec::definition::Architecture::Aarch64
        } else {
            seedbox_spec::definition::Architecture::X86_64
        },
    }
}
impl Machine for Migrating {
    type Context = ();
    type Error = std::io::Error;
    type Snapshot = Snapshot;
    fn run(
        &mut self,
        context: &mut RunContext<'_, ()>,
        upto: Timestamp,
    ) -> Result<StopReason, Self::Error> {
        while context
            .inbox
            .front()
            .is_some_and(|input| matches!(input.payload, contract::Request::SimulationReply { .. }))
        {
            context.inbox.take();
        }
        if let Some(payload) = self.call.take() {
            let id = contract::SimulationControlId {
                session: std::num::NonZeroU32::new(1).unwrap(),
                request: std::num::NonZeroU32::new(1).unwrap(),
            };
            context
                .outbox
                .emit(contract::Output {
                    at: Timestamp::from_nanos(context.clock.now()),
                    payload: Response::SimulationControl { id, payload },
                })
                .map_err(|_| std::io::Error::other("arm call outside the run"))?;
        }
        self.machine.run(context, upto)
    }
    fn snapshot(&mut self) -> Result<Snapshot, Self::Error> {
        let saved = self.machine.snapshot()?;
        if self.migrate {
            self.machine = restore_pv(&saved)?;
        }
        Ok(saved)
    }
    fn take_fault_counts(&mut self, id: u64) -> Result<Option<contract::FaultCounts>, Self::Error> {
        self.machine.take_fault_counts(id)
    }
}

/// Machines, their probe arguments and the fault installed before boot.
fn layout() -> Vec<(
    &'static str,
    &'static [&'static str],
    Option<contract::DeviceFault>,
)> {
    use contract::{DeviceFault, DiskEffect, DiskOp, DiskScope};
    let rounds: &'static [&'static str] = &["faults", "8"];
    vec![
        // The oracle is the unfaulted baseline.
        ("oracle", rounds, None),
        (
            "eio",
            rounds,
            Some(DeviceFault::DiskError {
                op: DiskOp::Write,
                ppm: 500_000,
                effect: DiskEffect::Eio,
                scope: DiskScope::All,
            }),
        ),
        (
            "corrupt",
            rounds,
            Some(DeviceFault::DiskError {
                op: DiskOp::Write,
                ppm: 1_000_000,
                effect: DiskEffect::Corrupt,
                scope: DiskScope::All,
            }),
        ),
        (
            "slow",
            rounds,
            Some(DeviceFault::DiskLatency {
                op: DiskOp::Flush,
                latency_ns: LATENCY_NS,
                jitter_ns: 1_000_000,
            }),
        ),
        (
            "race",
            &["threads", "20000"],
            Some(DeviceFault::Preempt { ppm: 100_000 }),
        ),
        ("calm", &["threads", "20000"], None),
        // Rounds 50 ms of guest time apart leave barriers before the hold.
        ("held", &["faults", "8", "50"], None),
    ]
}
const DONE: [(&str, &str); 6] = [
    ("oracle", "DISK faults done"),
    ("eio", "DISK faults done"),
    ("corrupt", "DISK faults done"),
    ("slow", "DISK faults done"),
    ("race", "DISK threads switches="),
    ("calm", "DISK threads switches="),
];

fn driver(backend: &Backend<PVMachine>, workers: usize, migrate: bool) -> Driver<Migrating> {
    let mut definitions: Vec<_> = layout()
        .into_iter()
        .map(|(name, arguments, _)| {
            definition(
                backend,
                name,
                arguments,
                DiskImage::zeroed(1 << 20).unwrap(),
            )
        })
        .collect();
    let oracle = definitions.remove(0);
    let mut factory = (backend.factory)();
    let mut driver = Driver::new_seeded(
        at(0),
        Duration::from_nanos(QUANTUM),
        workers,
        23,
        || Ok(()),
        oracle,
        definitions,
        Vec::new(),
        crate::driver::NetworkConfig::default(),
        move |definition| Ok(wrap(factory(definition)?, migrate, &definition.name)),
    )
    .unwrap();
    driver.set_engine(engine());
    let ids = driver
        .with_barrier(|b| Ok(b.machines().map(|(id, _)| id).collect::<Vec<_>>()))
        .unwrap();
    assert_eq!(MACHINES.get_or_init(|| ids.clone()), &ids);
    driver
}
fn key(text: &str) -> InputKey {
    InputKey::new(text).unwrap()
}
/// Install every fault at time zero. The oracle arms the held machine's
/// trigger as it starts: its third FLUSH is held and the machine crashes
/// there.
fn schedule(driver: &mut Driver<Migrating>) {
    driver
        .with_barrier(|b| {
            for (name, _, fault) in layout() {
                if let Some(fault) = fault {
                    b.schedule_with_handle(
                        at(0),
                        key(name),
                        Action::DeviceFault {
                            target: Target::Name(name.into()),
                            fault,
                        },
                        FaultHandle::named(name.to_owned()).unwrap(),
                    )?;
                }
            }
            Ok(())
        })
        .unwrap();
}
/// Machines join in definition order: the oracle, then the others.
fn machine(name: &str) -> MachineId {
    let index = layout().iter().position(|(n, _, _)| *n == name).unwrap();
    machines()[index]
}
/// Identities in insertion order, the same for every Driver of this layout.
static MACHINES: std::sync::OnceLock<Vec<MachineId>> = std::sync::OnceLock::new();
fn machines() -> &'static [MachineId] {
    MACHINES.get().expect("a scenario Driver was built")
}
/// Application stdout of one machine.
fn stdout(events: &[Event], name: &str) -> String {
    let id = machine(name);
    let bytes: Vec<u8> = events
        .iter()
        .filter_map(|event| match &event.kind {
            EventKind::Output(output) if output.machine == id => match &output.payload {
                Response::ApplicationOutput { bytes, .. } => Some(bytes.as_slice()),
                _ => None,
            },
            _ => None,
        })
        .flatten()
        .copied()
        .collect();
    String::from_utf8_lossy(&bytes).into_owned()
}
/// Step one quantum at a time until `done` holds, snapshotting every machine
/// but the held one each `snapshot_every` steps.
fn advance(
    driver: &mut Driver<Migrating>,
    events: &mut Vec<Event>,
    snapshot_every: Option<u64>,
    done: impl Fn(&[Event]) -> bool,
) {
    let guard = HangGuard::new("native device fault scenario must return", HOST_LIMIT);
    let mut steps = 0u64;
    while !done(events) {
        guard.remaining().expect("device fault scenario hang");
        let now = driver
            .with_barrier(|b| Ok(b.playback().position.at))
            .unwrap();
        assert!(now.as_nanos() < LIMIT_NS, "simulated bound");
        let outcome = driver.step(at(now.as_nanos() + QUANTUM)).unwrap();
        assert!(
            matches!(
                outcome,
                StepOutcome::BarrierReached { .. } | StepOutcome::Blocked { .. }
            ),
            "{outcome:?}"
        );
        events.extend(driver.drain_observations().events);
        steps += 1;
        if snapshot_every.is_some_and(|every| steps.is_multiple_of(every)) {
            driver
                .with_barrier(|b| {
                    for &id in machines() {
                        if id != machine("held") {
                            b.snapshot(id)?;
                        }
                    }
                    Ok(())
                })
                .unwrap();
        }
    }
}
/// Rounds the held machine reported after the oracle armed its trigger,
/// once it has.
fn rounds_after_arming(events: &[Event]) -> Option<usize> {
    let armed = events
        .iter()
        .find(|e| matches!(e.kind, EventKind::TriggerArmed { trigger: 1, .. }))?
        .at;
    let held = machine("held");
    Some(
        events
            .iter()
            .filter(|e| e.at >= armed)
            .filter_map(|e| match &e.kind {
                EventKind::Output(output) if output.machine == held => match &output.payload {
                    Response::ApplicationOutput { bytes, .. } => Some(
                        String::from_utf8_lossy(bytes)
                            .matches("DISK round=")
                            .count(),
                    ),
                    _ => None,
                },
                _ => None,
            })
            .sum(),
    )
}
fn finished(events: &[Event]) -> bool {
    DONE.iter()
        .all(|(name, marker)| stdout(events, name).contains(marker))
        && crashed(events)
}
fn crashed(events: &[Event]) -> bool {
    events.iter().any(|e| {
        matches!(
            e.kind,
            EventKind::Transition {
                change: Transition::Crashed,
                ..
            }
        )
    })
}
/// Heal every fault one quantum later and let the heals settle.
fn heal(driver: &mut Driver<Migrating>, events: &mut Vec<Event>) {
    let now = driver
        .with_barrier(|b| Ok(b.playback().position.at))
        .unwrap();
    driver
        .with_barrier(|b| {
            for (name, _, fault) in layout() {
                if fault.is_some() {
                    b.schedule(
                        at(now.as_nanos() + QUANTUM),
                        key(&format!("heal-{name}")),
                        Action::Heal(HealTarget::Handle(
                            FaultHandle::named(name.to_owned()).unwrap(),
                        )),
                    )?;
                }
            }
            Ok(())
        })
        .unwrap();
    let healed = |events: &[Event]| {
        events
            .iter()
            .filter(|e| {
                matches!(&e.kind, EventKind::FaultSettled { record, .. } if record.action == FaultAction::Heal)
            })
            .count()
            == 4
    };
    advance(driver, events, None, healed);
}
fn run(backend: &Backend<PVMachine>, workers: usize, snapshot_every: Option<u64>) -> Vec<Event> {
    let mut driver = driver(backend, workers, snapshot_every.is_some());
    schedule(&mut driver);
    let mut events = Vec::new();
    advance(&mut driver, &mut events, snapshot_every, finished);
    heal(&mut driver, &mut events);
    events
}

/// The guest-visible effect of each fault, and the counts its heal reports.
fn check(events: &[Event]) {
    let out = |name| stdout(events, name);
    let rounds = |name| out(name).matches("DISK round=").count();
    let number = |text: &str, field: &str| -> u64 {
        let start = text
            .find(field)
            .unwrap_or_else(|| panic!("{field} in {text}"))
            + field.len();
        text[start..]
            .split(|c: char| !c.is_ascii_digit())
            .next()
            .unwrap()
            .parse()
            .unwrap()
    };
    for name in ["oracle", "eio", "corrupt", "slow"] {
        assert_eq!(rounds(name), ROUNDS, "{name}: {}", out(name));
    }
    let failed = out("eio").matches("write=eio").count();
    assert!((1..ROUNDS).contains(&failed), "{}", out("eio"));
    assert_eq!(out("corrupt").matches("read=corrupt").count(), ROUNDS);
    assert!(
        number(&out("slow"), "min_flush_ns=") >= LATENCY_NS,
        "{}",
        out("slow")
    );
    assert!(
        number(&out("oracle"), "max_flush_ns=") < LATENCY_NS,
        "{}",
        out("oracle")
    );
    let (race, calm) = (
        number(&out("race"), "switches="),
        number(&out("calm"), "switches="),
    );
    // Only the ARM PV kernel yields; the x86 kernel ignores the preempt words
    // (x86_64 PV refuses preempt at admission, which this test bypasses).
    if cfg!(target_arch = "aarch64") {
        assert!(race > 100 && race > 10 * calm, "race={race} calm={calm}");
    }
    eprintln!(
        "observed: eio rounds {failed}/{ROUNDS}, slow min flush {} ns, baseline max flush {} ns, switches race {race} calm {calm}",
        number(&out("slow"), "min_flush_ns="),
        number(&out("oracle"), "max_flush_ns="),
    );
    // The held machine's third FLUSH after the oracle armed its trigger never
    // completed: it crashed there, so only two rounds report after arming.
    assert_eq!(rounds_after_arming(events), Some(2), "{}", out("held"));
    assert!(!out("held").contains("DISK faults done"), "{}", out("held"));
    let fault = events
        .iter()
        .find_map(|e| match &e.kind {
            EventKind::TriggerResolved {
                trigger: 1,
                result: TriggerResult::Fired,
                fault,
            } => *fault,
            _ => None,
        })
        .expect("the held machine's trigger fired");
    assert!(events.iter().any(|e| matches!(
        &e.kind,
        EventKind::Transition { change: Transition::Crashed, cause: Cause::Fault(id), .. } if *id == fault
    )));
    assert!(events.iter().any(|e| matches!(
        &e.kind,
        EventKind::FaultAdmitted(record) if record.origin == (FaultOrigin::Trigger { trigger: 1 })
    )));
    // Heals settle with what each disk fault matched and did.
    let counts: Vec<_> = events
        .iter()
        .filter_map(|e| match &e.kind {
            EventKind::FaultSettled { record, counts, .. }
                if record.action == FaultAction::Heal =>
            {
                Some((record.handle.clone().unwrap().to_string(), *counts))
            }
            _ => None,
        })
        .collect();
    let of = |name: &str| {
        counts
            .iter()
            .find(|(handle, _)| handle == name)
            .and_then(|(_, counts)| *counts)
            .unwrap_or_else(|| panic!("{name} heal counts in {counts:?}"))
    };
    assert_eq!(
        of("eio"),
        contract::FaultCounts {
            matched: ROUNDS as u64,
            affected: failed as u64
        }
    );
    assert_eq!(
        of("corrupt"),
        contract::FaultCounts {
            matched: ROUNDS as u64,
            affected: ROUNDS as u64
        }
    );
    let slow = of("slow");
    assert!(
        slow.matched >= ROUNDS as u64 && slow.affected == slow.matched,
        "{slow:?}"
    );
    // Preempt counts nothing, so its heal settles without counts.
    assert!(
        counts
            .iter()
            .any(|(handle, c)| handle == "race" && c.is_none()),
        "{counts:?}"
    );
    eprintln!("heal counts: {counts:?}");
}

#[test]
#[ignore = "requires native PV worker, rebuilt assets and disk fixtures"]
fn pv_device_faults_take_effect_and_runs_match_across_workers_and_migration() {
    let backend = backend();
    let serial = run(&backend, 1, None);
    check(&serial);
    assert!(serial == run(&backend, 4, None), "workers=4");
    // Snapshot-and-restore every machine but the held one each 7 quanta,
    // while latency completions, preempt streams and holds are live.
    assert!(serial == run(&backend, 2, Some(7)), "migration");
}

#[test]
#[ignore = "requires native PV worker, rebuilt assets and disk fixtures"]
fn pv_device_faults_and_holds_continue_from_a_driver_checkpoint() {
    let backend = backend();
    let mut source = driver(&backend, 1, false);
    schedule(&mut source);
    let mut before = Vec::new();
    // Capture once the held machine's trigger has counted a FLUSH, while
    // the slow machine still has delayed ones ahead.
    advance(&mut source, &mut before, None, |events| {
        rounds_after_arming(events).is_some_and(|rounds| rounds >= 1)
    });
    assert!(!stdout(&before, "slow").contains("DISK faults done"));
    assert!(!crashed(&before), "capture precedes the held request");
    let checkpoint = source.checkpoint().unwrap();
    let continuation = |driver: &mut Driver<Migrating>| {
        let mut events = before.clone();
        let start = events.len();
        advance(driver, &mut events, None, finished);
        heal(driver, &mut events);
        check(&events);
        events[start..]
            .iter()
            .map(|e| (e.position, e.at, e.kind.clone()))
            .collect::<Vec<_>>()
    };
    let expected = continuation(&mut source);
    drop(source);
    let factory_backend = backend;
    let mut factory = (factory_backend.factory)();
    let mut restored = Driver::from_checkpoint(
        &checkpoint,
        2,
        || Ok(()),
        |snapshot| {
            Ok(Migrating {
                machine: restore_pv(snapshot)?,
                migrate: false,
                call: None,
            })
        },
        move |definition| Ok(wrap(factory(definition)?, false, &definition.name)),
    )
    .unwrap();
    restored.set_engine(engine());
    assert!(
        continuation(&mut restored) == expected,
        "checkpoint continuation"
    );
}
