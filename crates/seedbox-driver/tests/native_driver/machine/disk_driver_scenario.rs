use crate::{
    driver::{Action, Driver, Event, EventKind, InputKey, MachineId, StepOutcome, Target},
    machine::{Machine, RunContext, StopReason, Timestamp},
    pv::Response,
    rng::RngStream,
};
use seedbox_machine::{
    definition::DiskImage,
    test_support::disk_scenario::{
        at, definition, raw_image, Backend, BOOT_LIMIT, HOST_LIMIT, QUANTUM,
    },
};
use seedbox_test_time::HangGuard;
use std::{fmt::Debug, time::Duration};
/// Snapshot replaces the running machine with one restored from it, so the
/// Driver continues on the restored copy.
struct Migrating<M: Machine> {
    machine: M,
    restore: fn(&M::Snapshot) -> Result<M, M::Error>,
}
impl<M: Machine> Machine for Migrating<M> {
    type Context = M::Context;
    type Error = M::Error;
    type Snapshot = M::Snapshot;
    fn run(
        &mut self,
        context: &mut RunContext<'_, M::Context>,
        upto: Timestamp,
    ) -> Result<StopReason, Self::Error> {
        self.machine.run(context, upto)
    }
    fn snapshot(&mut self) -> Result<Self::Snapshot, Self::Error> {
        let saved = self.machine.snapshot()?;
        self.machine = (self.restore)(&saved)?;
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

/// Application stdout of `machine` across committed events.
fn stdout(events: &[Event], machine: MachineId) -> String {
    let bytes: Vec<u8> = events
        .iter()
        .filter_map(|event| match &event.kind {
            EventKind::Output(output) if output.machine == machine => match &output.payload {
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

/// Three disk machines under the Driver: both targets write, are snapshotted
/// in place while live, crash, reboot and report their durable generation.
fn driver_disks<M>(backend: &Backend<M>, workers: usize) -> Vec<Event>
where
    M: Machine<Context = ()> + Send + 'static,
    M::Error: Debug + Send + 'static,
{
    let mut factory = (backend.factory)();
    let restore = backend.restore;
    let mut driver = Driver::new(
        at(0),
        Duration::from_nanos(QUANTUM),
        workers,
        FixtureRng(9),
        || Ok(()),
        definition(
            backend,
            "oracle",
            &["raw"],
            DiskImage::zeroed(1 << 20).unwrap(),
        ),
        vec![
            definition(backend, "a", &["raw"], raw_image()),
            definition(backend, "b", &["raw"], DiskImage::zeroed(1 << 20).unwrap()),
        ],
        Vec::new(),
        crate::driver::NetworkConfig::default(),
        move |definition| {
            Ok(Migrating {
                machine: factory(definition)?,
                restore,
            })
        },
    )
    .unwrap();
    let targets: Vec<MachineId> = driver
        .with_barrier(|b| Ok(b.machines().skip(1).map(|(id, _)| id).collect()))
        .unwrap();
    let guard = HangGuard::new("native disk marker transport must return", HOST_LIMIT);
    let mut events = Vec::new();
    let mut now = at(0);
    let mut advance_until = |driver: &mut Driver<Migrating<M>>, marker: &str| {
        while !targets
            .iter()
            .all(|&target| stdout(&events, target).contains(marker))
        {
            guard.remaining().expect("disk marker infrastructure hang");
            assert!(now.as_nanos() < 3 * BOOT_LIMIT, "simulated bound");
            let outcome = driver.step(at(now.as_nanos() + QUANTUM)).unwrap();
            events.extend(driver.drain_events());
            now = match outcome {
                StepOutcome::BarrierReached { at } | StepOutcome::Blocked { at } => at,
                other => panic!("unexpected outcome {other:?}"),
            };
        }
        now
    };
    let now = advance_until(&mut driver, "DISK volatile 1\n");
    driver
        .with_barrier(|b| {
            for (index, &target) in targets.iter().enumerate() {
                b.snapshot(target)?;
                b.schedule(
                    at(now.as_nanos() + QUANTUM),
                    InputKey::new(format!("crash-{index}")).unwrap(),
                    Action::Crash(Target::Machine(target)),
                )?;
                b.schedule(
                    at(now.as_nanos() + 2 * QUANTUM),
                    InputKey::new(format!("reboot-{index}")).unwrap(),
                    Action::Reboot(Target::Machine(target)),
                )?;
            }
            Ok(())
        })
        .unwrap();
    advance_until(&mut driver, "DISK volatile 2\n");
    for (target, base) in targets.iter().zip(["77", "00"]) {
        let text = stdout(&events, *target);
        assert!(
            text.contains(&format!("DISK raw boot=1 a=01 b=00 base={base} ")),
            "{text:?}"
        );
    }
    events
}

pub(crate) fn driver_is_deterministic_across_workers<M>(backend: &Backend<M>)
where
    M: Machine<Context = ()> + Send + 'static,
    M::Error: Debug + Send + 'static,
{
    let serial = driver_disks(backend, 1);
    assert!(serial == driver_disks(backend, 4), "workers=4");
}
