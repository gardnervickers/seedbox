//! Native test construction through public Driver and Machine boundaries.
use seedbox_driver::driver::{Driver, Error, InputKey};
use seedbox_machine::test_support::factory_definition::definition;
use seedbox_machine::{
    machine::{Machine, Timestamp},
    rng::RngStream,
};
use std::time::Duration;
pub fn driver<M>(
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
        seedbox_driver::driver::NetworkConfig::default(),
        move |_| Ok(machines.next().expect("one fixture per definition")),
    )
}
/// A key unique within this test process, so it is never live twice.
pub fn auto_key() -> InputKey {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    InputKey::new(format!("auto-{}", NEXT.fetch_add(1, Ordering::Relaxed))).unwrap()
}
