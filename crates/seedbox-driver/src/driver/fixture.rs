//! Scheduling-only fixtures; constructor behavior is tested through Driver::new.
use super::*;
use crate::definition::{FilesystemArchive, Launch, PreparedApplication, Resources};
pub(crate) fn definition(index: usize) -> MachineDefinition {
    MachineDefinition {
        name: format!("fixture-{index}"),
        resources: Resources {
            memory_bytes: 128 << 20,
        },
        application: PreparedApplication {
            filesystem: FilesystemArchive::new(tar::Builder::new(Vec::new()).into_inner().unwrap())
                .unwrap(),
            launch: Launch {
                argv: vec!["/fixture".into()],
                environment: Default::default(),
                working_directory: "/".into(),
                user: "0:0".into(),
            },
        },
        disk: None,
        network: None,
    }
}
pub(crate) fn driver<M>(
    start: Timestamp,
    quantum: Duration,
    workers: usize,
    rng: impl RngStream + Send + 'static,
    context: impl Fn() -> Result<M::Context, M::Error> + Send + Sync + 'static,
    make: impl FnOnce() -> Result<Vec<M>, M::Error>,
) -> Result<Driver<M>, Error<M::Error>>
where
    M: Machine + Send + 'static,
    M::Context: 'static,
    M::Error: Send + 'static,
{
    let machines = make().map_err(Error::Construction)?;
    assert!(!machines.is_empty(), "fixture requires an oracle");
    let others = (1..machines.len()).map(definition).collect();
    let mut machines = machines.into_iter();
    Driver::new(
        start,
        quantum,
        workers,
        rng,
        context,
        definition(0),
        others,
        Vec::new(),
        crate::driver::NetworkConfig::default(),
        move |_| Ok(machines.next().expect("one fixture per definition")),
    )
}
/// Committed outputs among drained events, in publication order.
pub(crate) fn outputs(events: &[Event]) -> Vec<MachineResponse> {
    events
        .iter()
        .filter_map(|event| match &event.kind {
            EventKind::Output(output) => Some(output.clone()),
            _ => None,
        })
        .collect()
}
/// Settlements among drained events: record, status, outcome time and commit time.
pub(crate) fn settlements(
    events: &[Event],
) -> Vec<(FaultRecord, SettlementStatus, Option<Timestamp>, Timestamp)> {
    events
        .iter()
        .filter_map(|event| match &event.kind {
            EventKind::FaultSettled {
                record,
                status,
                outcome_at,
                ..
            } => Some((record.clone(), *status, *outcome_at, event.at)),
            _ => None,
        })
        .collect()
}
/// A key unique within this test process, so it is never live twice.
pub(crate) fn auto_key() -> InputKey {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    InputKey::new(format!("auto-{}", NEXT.fetch_add(1, Ordering::Relaxed))).unwrap()
}
impl<M> Driver<M>
where
    M: Machine + Send + 'static,
    M::Context: 'static,
    M::Error: Send + 'static,
{
    /// Scheduling shortcut for private tests: one barrier, one input.
    pub(crate) fn schedule(
        &mut self,
        at: Timestamp,
        key: InputKey,
        action: Action,
    ) -> Result<(), Error<M::Error>> {
        self.with_barrier(|barrier| barrier.schedule(at, key, action))
    }
}

/// Owned-machine fixtures shared by Driver private invariant suites.
pub(super) mod owned;

impl<M: Machine> Driver<M> {
    /// Take published events only, leaving bounded frame facts for a later
    /// `drain_observations`. Production hosts drain both together.
    pub(crate) fn drain_events(&mut self) -> Vec<Event> {
        std::mem::take(&mut self.observations.published)
    }
}
