//! Network-enabled owned mock construction shared by private invariant suites.
use super::*;
use crate::driver::network_test_frames as frames;
pub(in crate::driver) fn policy(latency: u64) -> NetworkPolicy {
    NetworkPolicy {
        latency_ns: latency,
        jitter_ns: 0,
        loss_ppm: 0,
        bandwidth_bps: None,
        queue_bound_ns: None,
    }
}
pub(in crate::driver) fn setup(capacity: usize) -> NetworkSetup {
    NetworkSetup::from(NetworkConfig::new(Duration::from_nanos(10), Duration::ZERO, 0).unwrap())
        .with_frame_capacity(capacity)
}
pub(in crate::driver) fn net(
    specs: Vec<State>,
    setup: NetworkSetup,
    workers: usize,
) -> Driver<Mock> {
    let mut defs: Vec<_> = (0..specs.len())
        .map(|i| {
            let mut d = fixture::definition(i);
            d.name = specs[i].name.clone();
            d.network = Some(
                crate::definition::NetworkInterface::new(
                    crate::definition::MacAddress::new(frames::mac(i)).unwrap(),
                    [10, 0, 0, (i + 1) as u8].into(),
                    24,
                )
                .unwrap(),
            );
            d
        })
        .collect();
    let oracle = defs.remove(0);
    let all: BTreeMap<_, _> = specs.into_iter().map(|s| (s.name.clone(), s)).collect();
    Driver::new_seeded_with_network(
        at(0),
        Duration::from_nanos(10),
        workers,
        51,
        || Ok(Context(None)),
        oracle,
        defs,
        vec![],
        setup,
        move |d| Ok(Mock::new(all[&d.name].clone())),
    )
    .unwrap()
}
pub(in crate::driver) fn initial() -> Vec<State> {
    (0..3)
        .map(|i| spec(i, true, vec![(0, Response::Ready)]))
        .collect()
}
