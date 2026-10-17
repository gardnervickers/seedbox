use crate::{
    driver::{
        Action, Driver, Event, EventKind, FaultOrigin, InputKey, MachineId, SettlementStatus,
        StepOutcome, Target,
    },
    machine::{ExitReason, Machine, RunContext, StopReason, Timestamp},
    rng::RngStream,
};
use seedbox_machine::{
    definition::MachineDefinition,
    pv::Response,
    test_support::kernel_terminal_tests::{definition, BOOT_LIMIT, HOST_LIMIT, QUANTUM},
};
use seedbox_test_time::HangGuard;
use std::{fmt::Debug, fs, path::PathBuf, time::Duration};
fn at(n: u64) -> Timestamp {
    Timestamp::from_nanos(n)
}
fn input(key: &str) -> PathBuf {
    std::env::var_os(key).expect(key).into()
}
struct Migrating<M, F> {
    inner: M,
    restore: F,
}
impl<M, F> Machine for Migrating<M, F>
where
    M: Machine<Context = ()>,
    F: Fn(&M::Snapshot) -> M,
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
        self.inner = (self.restore)(&saved);
        Ok(saved)
    }
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

fn key(text: &str) -> InputKey {
    InputKey::new(text).unwrap()
}

/// Driver view of a guest panic: a live idle oracle and a target whose kernel
/// panics. Returns committed events plus the target's stop at each barrier.
/// The confirmed self-crash makes the target down, so a host Reboot restarts
/// it directly after an in-place snapshot of its exited state.
fn driver_panic<M>(
    binary: Vec<u8>,
    memory: u64,
    workers: usize,
    construct: impl FnMut(&MachineDefinition) -> M + Send + 'static,
) -> (Vec<Event>, Vec<(Timestamp, StopReason)>)
where
    M: Machine<Context = ()> + Send + 'static,
    M::Error: Debug + Send + 'static,
{
    let mut construct = construct;
    let mut driver = Driver::new(
        at(0),
        Duration::from_nanos(QUANTUM),
        workers,
        FixtureRng(5),
        || Ok(()),
        definition(&binary, "idle", "oracle", memory),
        vec![definition(&binary, "panic", "target", memory)],
        Vec::new(),
        crate::driver::NetworkConfig::default(),
        move |definition| Ok(construct(definition)),
    )
    .unwrap();
    let target: MachineId = driver
        .with_barrier(|b| Ok(b.machines().nth(1).unwrap().0))
        .unwrap();
    let guard = HangGuard::new("native kernel exit transport must return", HOST_LIMIT);
    let mut events = Vec::new();
    let mut trace: Vec<(Timestamp, StopReason)> = Vec::new();
    let mut now = at(0);
    fn step<M: Machine<Context = ()> + Send + 'static>(
        driver: &mut Driver<M>,
        target: MachineId,
        guard: &HangGuard,
        events: &mut Vec<Event>,
        trace: &mut Vec<(Timestamp, StopReason)>,
        now: &mut Timestamp,
    ) -> StopReason
    where
        M::Error: Debug + Send + 'static,
    {
        guard.remaining().expect("kernel exit infrastructure hang");
        assert!(now.as_nanos() < 3 * BOOT_LIMIT, "simulated bound");
        let outcome = driver.step(at(now.as_nanos() + QUANTUM)).unwrap();
        events.extend(driver.drain_events());
        *now = match outcome {
            StepOutcome::BarrierReached { at } | StepOutcome::Blocked { at } => at,
            other => panic!("unexpected outcome {other:?}"),
        };
        let stop = driver.with_barrier(|b| b.stopped(target)).unwrap();
        trace.push((*now, stop));
        stop
    }
    macro_rules! advance {
        () => {
            step(
                &mut driver,
                target,
                &guard,
                &mut events,
                &mut trace,
                &mut now,
            )
        };
    }
    let exited = |stop| matches!(stop, StopReason::Exited { .. });
    while !exited(advance!()) {}
    let first = *trace.last().unwrap();
    assert!(matches!(
        first.1,
        StopReason::Exited {
            reason: ExitReason::Crashed,
            ..
        }
    ));
    driver
        .with_barrier(|b| {
            b.snapshot(target)?;
            b.schedule(
                at(now.as_nanos() + QUANTUM),
                key("reboot"),
                Action::Reboot(Target::Machine(target)),
            )
        })
        .unwrap();
    let applied = |events: &[Event]| {
        events.iter().any(|event| {
            matches!(&event.kind, EventKind::FaultSettled { record, status, .. }
                if record.origin == FaultOrigin::Input { key: key("reboot") }
                    && *status == SettlementStatus::Applied)
        })
    };
    while !applied(&events) {
        advance!();
    }
    while !exited(advance!()) {}
    let second = *trace.last().unwrap();
    assert!(second.0 > first.0);
    assert!(matches!(
        second.1,
        StopReason::Exited {
            reason: ExitReason::Crashed,
            ..
        }
    ));
    let console: Vec<u8> = events
        .iter()
        .filter_map(|event| match &event.kind {
            EventKind::Output(output) if output.machine == target => match &output.payload {
                Response::BootConsole { bytes } => Some(bytes.as_slice()),
                _ => None,
            },
            _ => None,
        })
        .flatten()
        .copied()
        .collect();
    let panics = String::from_utf8_lossy(&console)
        .matches("Kernel panic - not syncing: sysrq triggered crash")
        .count();
    assert_eq!(panics, 2, "each boot panicked once");
    (events, trace)
}

#[test]
#[ignore = "requires native TCG QEMU, rebuilt assets and kernel-terminal fixture"]
fn tcg_driver_guest_panic_is_deterministic_across_workers() {
    let run = |workers| {
        let qemu = input("SEEDBOX_TCG_QEMU");
        let mut factory = super::tcg::Factory::new(
            &qemu,
            &input("SEEDBOX_TCG_ASSETS"),
            super::TEST_EPOCH_SECONDS,
        )
        .unwrap();
        driver_panic(
            fs::read(input("SEEDBOX_TCG_TERMINAL_APPLICATION")).unwrap(),
            256 << 20,
            workers,
            move |definition| {
                let qemu = qemu.clone();
                Migrating {
                    inner: factory.create(definition).unwrap(),
                    restore: move |saved: &super::tcg::Snapshot| {
                        super::tcg::TcgMachine::from_snapshot(&qemu, saved).unwrap()
                    },
                }
            },
        )
    };
    assert!(run(1) == run(4), "workers=4");
}

#[cfg(seedbox_pv)]
#[test]
#[ignore = "requires native PV worker, rebuilt assets and kernel-terminal fixture"]
fn pv_driver_guest_panic_is_deterministic_across_workers() {
    let run = |workers| {
        let worker = input("SEEDBOX_PV_WORKER");
        let mut factory = super::pv::Factory::new(
            &worker,
            &input("SEEDBOX_PV_ASSETS"),
            super::TEST_EPOCH_SECONDS,
        )
        .unwrap();
        driver_panic(
            fs::read(input("SEEDBOX_PV_TERMINAL_APPLICATION")).unwrap(),
            128 << 20,
            workers,
            move |definition| {
                let worker = worker.clone();
                Migrating {
                    inner: factory.create(definition).unwrap(),
                    restore: move |saved: &super::pv::Snapshot| {
                        super::pv::PVMachine::from_snapshot(&worker, saved).unwrap()
                    },
                }
            },
        )
    };
    assert!(run(1) == run(4), "workers=4");
}
