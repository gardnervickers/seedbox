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
        move |_| Ok(machines.next().expect("one fixture per definition")),
    )
}
