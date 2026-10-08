use network_counters::NetworkCounters;
use seedbox_engine_core::network_counters;

#[test]
fn inspection_excludes_future_and_equal_time_events_regardless_of_early_dma() {
    let mut exact = NetworkCounters::default();
    let mut early = NetworkCounters::default();
    // Same actual NIC acceptance results, but early execution has already DMA'd
    // the boundary/future packets. An exact run has not delivered those yet.
    for (at, outcome) in [(90, 0), (99, 2), (100, 0), (103, 0)] {
        early.record(at, 0, 1, outcome);
    }
    for (at, outcome) in [(90, 0), (99, 2)] {
        exact.record(at, 0, 1, outcome);
    }
    early.record(98, 1, 0, 1);
    exact.record(98, 1, 0, 1); // send-time partition drop
    exact.advance(100);
    early.advance(100);
    assert_eq!(exact.counts(), early.counts());
    assert_eq!(exact.counts()[&(0, 1)], [1, 0, 1, 0]);
    assert_eq!(exact.counts()[&(1, 0)], [0, 1, 0, 0]);
    for (at, outcome) in [(100, 0), (103, 0)] {
        exact.record(at, 0, 1, outcome);
    }
    exact.advance(104);
    early.advance(104);
    assert_eq!(exact.counts(), early.counts());
    assert_eq!(exact.counts()[&(0, 1)], [3, 0, 1, 0]);
}

#[test]
fn loss_drops_use_the_send_time_frontier_and_survive_snapshot() {
    let mut counts = NetworkCounters::default();
    counts.record(10, 0, 1, 3);
    counts.record(20, 0, 1, 3);
    counts.advance(20);
    assert_eq!(counts.counts()[&(0, 1)], [0, 0, 0, 1]);
    let mut restored = counts.clone();
    restored.advance(21);
    assert_eq!(restored.counts()[&(0, 1)], [0, 0, 0, 2]);
}
