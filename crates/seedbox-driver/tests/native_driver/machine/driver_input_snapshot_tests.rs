//! Native host-input evidence with private in-place machine snapshot
//! restoration. Driver-owned state (the pending input, its live key and the
//! admitted fault awaiting acknowledgment) stays in the Driver; only machines
//! are captured and rebuilt, so continuation must be indistinguishable.
use super::guest_service_snapshot_tests::prepared;
use crate::{
    driver::{
        Action, Driver, Event, EventKind, FaultOrigin, InputKey, MachineId, SettlementStatus,
        StepOutcome, Target,
    },
    machine::{Machine, RunContext, StopReason, Timestamp},
    pv::Response,
    rng::RngStream,
};
use std::{
    fmt::Debug,
    fs,
    path::PathBuf,
    time::{Duration, Instant},
};
const QUANTUM: u64 = 5_000_000;
fn at(n: u64) -> Timestamp {
    Timestamp::from_nanos(n)
}
fn input(key: &str) -> PathBuf {
    std::env::var_os(key).expect(key).into()
}
fn key(text: &str) -> InputKey {
    InputKey::new(text).unwrap()
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
/// Snapshot replaces the running machine with one restored from that snapshot.
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
fn settled(events: &[Event], name: &str) -> Option<SettlementStatus> {
    events.iter().find_map(|event| match &event.kind {
        EventKind::FaultSettled { record, status, .. }
            if record.origin == FaultOrigin::Input { key: key(name) } =>
        {
            Some(*status)
        }
        _ => None,
    })
}
fn admitted(events: &[Event], name: &str) -> bool {
    events.iter().any(|event| {
        matches!(&event.kind,
        EventKind::FaultAdmitted(record) if record.origin == FaultOrigin::Input { key: key(name) })
    })
}
fn acknowledged(events: &[Event], machine: MachineId, command: u32) -> bool {
    events.iter().any(|event| matches!(&event.kind,
        EventKind::Output(output) if output.machine == machine
            && matches!(output.payload, Response::CommandAcknowledged { id, .. } if id.get() == command)))
}
fn scenario<M>(
    memory: u64,
    initrd: Vec<u8>,
    workers: usize,
    migrate: bool,
    mut construct: impl FnMut(&crate::definition::MachineDefinition) -> M + Send + 'static,
) -> Vec<Event>
where
    M: Machine<Context = ()> + Send + 'static,
    M::Error: Debug + Send + 'static,
{
    let definition = prepared::definition(initrd, memory);
    let mut oracle = definition.clone();
    oracle.name = "oracle".into();
    let mut target = definition;
    target.name = "target".into();
    let mut driver = Driver::new(
        at(0),
        Duration::from_nanos(QUANTUM),
        workers,
        FixtureRng(23),
        || Ok(()),
        oracle,
        vec![target],
        Vec::new(),
        crate::driver::NetworkConfig::default(),
        move |definition| Ok(construct(definition)),
    )
    .unwrap();
    let (_oracle, target) = driver
        .with_barrier(|b| {
            let ids: Vec<_> = b.machines().map(|(id, _)| id).collect();
            b.schedule(
                at(0),
                key("oracle-pause"),
                Action::Pause(Target::Machine(ids[0])),
            )?;
            Ok((ids[0], ids[1]))
        })
        .unwrap();
    let started = Instant::now();
    let mut events: Vec<Event> = Vec::new();
    let mut now = at(0);
    let step = |driver: &mut Driver<M>, events: &mut Vec<Event>, upto: Timestamp| {
        assert!(
            started.elapsed() < Duration::from_secs(180),
            "native deadline"
        );
        let outcome = driver.step(upto).unwrap();
        events.extend(driver.drain_events());
        match outcome {
            StepOutcome::BarrierReached { at } | StepOutcome::Blocked { at } => at,
            other => panic!("unexpected outcome {other:?}"),
        }
    };
    while !events.iter().any(|event| {
        matches!(&event.kind,
        EventKind::Output(output) if output.machine == target && output.payload == Response::Ready)
    }) {
        now = step(&mut driver, &mut events, at(now.as_nanos() + QUANTUM));
    }
    let deadline = Duration::from_secs(5);
    let freeze_at = at(now.as_nanos() + 3 * QUANTUM / 2);
    let stop_at = at(now.as_nanos() + 20 * QUANTUM);
    driver
        .with_barrier(|b| {
            b.schedule(
                freeze_at,
                key("freeze"),
                Action::PauseProcess {
                    target: Target::Name("target".into()),
                    deadline,
                },
            )?;
            b.schedule(
                stop_at,
                key("stop"),
                Action::Stop {
                    target: Target::Machine(target),
                    deadline,
                },
            )
        })
        .unwrap();
    // Checkpoint with both inputs pending: nothing is reserved yet.
    if migrate {
        driver
            .with_barrier(|b| b.snapshot(target).map(drop))
            .unwrap();
    }
    while now < freeze_at {
        now = step(&mut driver, &mut events, freeze_at);
    }
    // At the admission barrier: the freeze was dispatched, but the guest has
    // not acknowledged it; the stop is still pending.
    assert!(admitted(&events, "freeze"));
    assert_eq!(settled(&events, "freeze"), None);
    assert!(!admitted(&events, "stop"));
    if migrate {
        driver
            .with_barrier(|b| b.snapshot(target).map(drop))
            .unwrap();
    }
    while settled(&events, "stop").is_none() {
        now = step(&mut driver, &mut events, at(now.as_nanos() + QUANTUM));
    }
    assert_eq!(settled(&events, "freeze"), Some(SettlementStatus::Applied));
    assert_eq!(settled(&events, "stop"), Some(SettlementStatus::Applied));
    assert!(acknowledged(&events, target, 1) && acknowledged(&events, target, 2));
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(&event.kind, EventKind::Output(output)
                if output.machine == target
                    && matches!(output.payload, Response::CommandAcknowledged { .. })))
            .count(),
        2,
        "each command is acknowledged exactly once"
    );
    events
        .into_iter()
        .filter(|event| {
            !matches!(&event.kind, EventKind::Output(output)
                if matches!(output.payload, Response::BootConsole { .. }))
        })
        .collect()
}
fn tcg(workers: usize, migrate: bool) -> Vec<Event> {
    let qemu = input("SEEDBOX_TCG_QEMU");
    let mut factory = super::tcg::Factory::new(
        &qemu,
        &input("SEEDBOX_TCG_ASSETS"),
        crate::machine::TEST_EPOCH_SECONDS,
    )
    .unwrap();
    scenario(
        256 << 20,
        fs::read(input("SEEDBOX_TCG_SERVICE_INITRD")).unwrap(),
        workers,
        migrate,
        move |definition| {
            let qemu = qemu.clone();
            Migrating {
                inner: factory.create(definition).unwrap(),
                restore: move |saved: &super::tcg::Snapshot| {
                    super::tcg::TcgMachine::from_snapshot(&qemu, saved)
                },
            }
        },
    )
}
#[test]
#[ignore = "requires native TCG kernel/helper and service initramfs"]
fn tcg_host_inputs_continue_after_snapshot_with_pending_input_and_unacknowledged_command() {
    let serial = tcg(1, false);
    assert_eq!(serial, tcg(4, false), "workers=4");
    assert_eq!(serial, tcg(1, true), "restored workers=1");
    assert_eq!(serial, tcg(4, true), "restored workers=4");
}
#[cfg(seedbox_pv)]
fn pv(workers: usize, migrate: bool) -> Vec<Event> {
    let worker = input("SEEDBOX_PV_WORKER");
    let mut factory = super::pv::Factory::new(
        &worker,
        &input("SEEDBOX_PV_ASSETS"),
        crate::machine::TEST_EPOCH_SECONDS,
    )
    .unwrap();
    scenario(
        128 << 20,
        fs::read(input("SEEDBOX_PV_SERVICE_INITRD")).unwrap(),
        workers,
        migrate,
        move |definition| {
            let worker = worker.clone();
            Migrating {
                inner: factory.create(definition).unwrap(),
                restore: move |saved: &super::pv::Snapshot| {
                    super::pv::PVMachine::from_snapshot(&worker, saved)
                        .map_err(std::io::Error::other)
                },
            }
        },
    )
}
#[cfg(seedbox_pv)]
#[test]
#[ignore = "requires native PV hypervisor/kernel/helper and service initramfs"]
fn pv_host_inputs_continue_after_snapshot_with_pending_input_and_unacknowledged_command() {
    let serial = pv(1, false);
    assert_eq!(serial, pv(4, false), "workers=4");
    assert_eq!(serial, pv(1, true), "restored workers=1");
    assert_eq!(serial, pv(4, true), "restored workers=4");
}
