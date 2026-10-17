//! Router arithmetic/stream/codec checks; Driver admission is tested separately.
use super::test_observer::ObservedNetwork as Network;
use super::*;
use crate::driver::network_test_frames as frames;
use crate::persistence::{CodecError, DecodeLimits, Decoder, Encoder, Wire};
use frames::{plain, tcp};
fn config() -> NetworkConfig {
    NetworkConfig::new(Duration::from_nanos(10), Duration::ZERO, 0).unwrap()
}
fn policy(latency: u64) -> NetworkPolicy {
    NetworkPolicy {
        latency_ns: latency,
        jitter_ns: 0,
        loss_ppm: 0,
        bandwidth_bps: None,
        queue_bound_ns: None,
    }
}
fn net(setup: NetworkSetup) -> Network {
    let mut n = Network::from_setup(setup);
    for i in 0..3 {
        n.admit(
            MachineId(i),
            &format!("m{i}"),
            7 + i as u64,
            Some(MacAddress::new(frames::mac(i)).unwrap()),
        );
    }
    n
}
fn fresh() -> Network {
    net(config().into())
}
fn serialized<T: Wire>(value: &T) -> Vec<u8> {
    let mut e = Encoder { output: Vec::new() };
    value.put(&mut e).unwrap();
    e.output
}
fn rngs(n: &Network) -> Vec<Vec<u8>> {
    let p = &n.pairs[&(MachineId(0), MachineId(1))];
    std::iter::once(&p.loss)
        .chain(std::iter::once(&p.jitter))
        .chain(p.faults.iter())
        .map(serialized)
        .collect()
}
fn limits() -> DecodeLimits {
    DecodeLimits {
        encoded_bytes: 16 << 20,
        allocation_bytes: 32 << 20,
        collection_items: 1 << 16,
        total_items: 1 << 20,
        string_bytes: 1 << 16,
        blob_bytes: 1 << 20,
        depth: 128,
        archive_entries: 1000,
        archive_path_bytes: 1 << 16,
        archive_expanded_bytes: 1 << 20,
    }
}
fn decode(n: &Network, now: u64) -> Network {
    let bytes = serialized(n);
    let mut d = Decoder::new(&bytes[..], bytes.len() as u64, limits());
    let restored = Network::get(&mut d).unwrap();
    assert_eq!(d.remaining, 0);
    restored.check_persist(3, now, &[]).unwrap();
    assert_eq!(serialized(&restored), bytes);
    restored
}
#[test]
fn named_baselines_global_bandwidth_and_policy_validation_are_directed() {
    let setup = NetworkSetup::new(
        config(),
        Some(1_000_000_000),
        Some(480),
        vec![LinkPolicy {
            from: "m0".into(),
            to: "m1".into(),
            policy: policy(50),
        }],
    )
    .unwrap();
    let mut n = net(setup);
    n.route(MachineId(0), 0, &plain(0, 1, 1)).unwrap();
    n.route(MachineId(1), 0, &plain(1, 0, 2)).unwrap();
    assert_eq!(
        n.in_flight()
            .iter()
            .map(|x| (x.0, x.1, x.2))
            .collect::<Vec<_>>(),
        [
            (50, MachineId(0), MachineId(1)),
            (490, MachineId(1), MachineId(0))
        ]
    );
    let link = LinkPolicy {
        from: "m0".into(),
        to: "m1".into(),
        policy: policy(9),
    };
    assert!(NetworkSetup::new(config(), None, None, vec![link]).is_err());
    let link = LinkPolicy {
        from: "m0".into(),
        to: "m1".into(),
        policy: policy(10),
    };
    assert!(NetworkSetup::new(config(), None, None, vec![link.clone(), link]).is_err());
    assert!(NetworkSetup::new(config(), Some(0), None, vec![]).is_err());
    assert!(NetworkSetup::new(config(), None, Some(1), vec![]).is_err());
}
#[test]
fn layered_heals_in_either_order_reveal_baseline_without_editing_flights() {
    for older_first in [true, false] {
        let setup = NetworkSetup::new(
            config(),
            None,
            None,
            vec![LinkPolicy {
                from: "m0".into(),
                to: "m1".into(),
                policy: policy(20),
            }],
        )
        .unwrap();
        let mut n = net(setup);
        n.shape(
            4,
            "low".into(),
            &[MachineId(0)],
            &[MachineId(1)],
            policy(30),
            NetworkFaults::default(),
        );
        n.shape(
            9,
            "high".into(),
            &[MachineId(0)],
            &[MachineId(1)],
            policy(40),
            NetworkFaults::default(),
        );
        n.route(MachineId(0), 0, &plain(0, 1, 1)).unwrap();
        let before = n.in_flight();
        assert!(n
            .unshape(if older_first { "low" } else { "high" })
            .is_some());
        assert_eq!(n.in_flight(), before);
        n.route(MachineId(0), 100, &plain(0, 1, 2)).unwrap();
        assert_eq!(
            n.in_flight().last().unwrap().0,
            if older_first { 140 } else { 130 }
        );
        assert!(n
            .unshape(if older_first { "high" } else { "low" })
            .is_some());
        n.route(MachineId(0), 200, &plain(0, 1, 3)).unwrap();
        assert_eq!(n.in_flight().last().unwrap().0, 220);
    }
}
#[test]
fn bandwidth_ceil_tail_drop_and_overflow_never_reserve_clocks() {
    let mut p = policy(10);
    p.bandwidth_bps = Some(1_000_000_001);
    p.queue_bound_ns = Some(480);
    let mut n = fresh();
    n.shape(
        1,
        "q".into(),
        &[MachineId(0)],
        &[MachineId(1)],
        p,
        NetworkFaults::default(),
    );
    n.route(MachineId(0), 0, &plain(0, 1, 1)).unwrap();
    assert_eq!(n.next_arrival(), Some(490));
    let pair = (MachineId(0), MachineId(1));
    assert_eq!(n.pairs[&pair].last_depart, Some(480));
    n.route(MachineId(0), 0, &plain(0, 1, 2)).unwrap();
    assert_eq!(n.in_flight().len(), 1);
    assert_eq!(n.pairs[&pair].last_depart, Some(480));
    assert_eq!(n.pairs[&pair].last, Some(490));
    n.route(MachineId(0), 480, &plain(0, 1, 3)).unwrap();
    assert_eq!(n.in_flight().len(), 2);
    assert_eq!(n.in_flight().last().unwrap().0, 970);
    let stats = n.unshape("q").unwrap().queue.unwrap();
    assert_eq!(
        stats,
        QueueCounts {
            matched: 3,
            queue_dropped: 1,
            delayed_max_ns: 960
        }
    );
    let mut n = fresh();
    p.bandwidth_bps = Some(1);
    p.queue_bound_ns = None;
    n.shape(
        1,
        "overflow".into(),
        &[MachineId(0)],
        &[MachineId(1)],
        p,
        NetworkFaults::default(),
    );
    n.route(MachineId(0), u64::MAX - 1, &plain(0, 1, 1))
        .unwrap();
    assert!(n.is_idle());
    assert_eq!(n.pairs[&pair].last, None);
    assert_eq!(n.pairs[&pair].last_depart, None);
    assert_eq!(n.counts[&pair].queue_dropped, 1);
    assert_eq!(
        n.unshape("overflow").unwrap().queue.unwrap().delayed_max_ns,
        u64::MAX
    );
}
#[test]
fn selection_masking_classification_and_effective_draws_follow_route_order() {
    let mut n = fresh();
    n.partition("cut".into(), &[MachineId(0)], &[MachineId(1)]);
    let syn = tcp(0, 1, (1234, 80), 7, 0, 2);
    n.route(MachineId(0), 0, &syn).unwrap();
    let c = n.connections("m0", "m1", 0).pop().unwrap();
    assert_eq!(c.incarnation, 1);
    assert!(n.pairs.is_empty());
    n.heal("cut");
    n.select(1, "flow".into(), c.clone()).unwrap();
    let mut p = policy(10);
    p.jitter_ns = 5;
    p.loss_ppm = 500_000;
    p.bandwidth_bps = Some(1);
    let faults = NetworkFaults {
        burst_loss_ppm: 1_000_000,
        burst_length: 3,
        reorder_ppm: 1_000_000,
        reorder_delay_ns: 9,
        duplicate_ppm: 1_000_000,
        ..NetworkFaults::default()
    };
    n.shape(
        2,
        "faults".into(),
        &[MachineId(0)],
        &[MachineId(1)],
        p,
        faults,
    );
    let ack = tcp(0, 1, (1234, 80), 8, 1, 0x10);
    n.route(MachineId(0), 1, &ack).unwrap();
    let pair = &n.pairs[&(MachineId(0), MachineId(1))];
    let seed = pair_seed(7, "m1");
    for (actual, domain) in [
        (&pair.loss, RngDomain::NetworkLoss),
        (&pair.jitter, RngDomain::NetworkJitter),
    ] {
        let mut expected = SeededRng::derived(seed, domain);
        expected.next_u64();
        assert_eq!(serialized(actual), serialized(&expected));
    }
    for (actual, domain) in pair.faults.iter().zip(parity::FAULT_DOMAINS) {
        assert_eq!(
            serialized(actual),
            serialized(&SeededRng::derived(seed, domain))
        );
    }
    assert!(n.is_idle());
    assert_eq!(n.layers[&2].queue, QueueCounts::default());
    assert_eq!(n.layers[&2].counts, NetworkFaultCounts::default());
    let cursor = rngs(&n);
    n.partition("cut2".into(), &[MachineId(0)], &[MachineId(1)]);
    n.route(MachineId(0), 2, &ack).unwrap();
    assert_eq!(rngs(&n), cursor);
    assert_eq!(
        n.selection_counts("flow").unwrap().selection.unwrap(),
        SelectionCounts {
            matched: 2,
            dropped: 1
        }
    );
    let rst = tcp(0, 1, (1234, 80), 8, 1, 0x14);
    n.route(MachineId(0), 3, &rst).unwrap();
    assert!(!n.selection_available(&c));
    assert!(n.connections.selections.contains_key("flow"));
}
#[test]
fn bursts_have_exact_length_and_heal_discards_only_their_layer_state() {
    let f = NetworkFaults {
        burst_loss_ppm: 1_000_000,
        burst_length: 3,
        ..NetworkFaults::default()
    };
    let mut n = fresh();
    n.shape(
        1,
        "burst".into(),
        &[MachineId(0)],
        &[MachineId(1)],
        policy(10),
        f,
    );
    for i in 0..5 {
        n.route(MachineId(0), i, &plain(0, 1, i as u8)).unwrap();
    }
    assert!(n.is_idle());
    assert_eq!(n.layers[&1].burst[&(MachineId(0), MachineId(1))], 1);
    let actual = &n.pairs[&(MachineId(0), MachineId(1))].faults[2];
    let mut expected = SeededRng::derived(pair_seed(7, "m1"), RngDomain::NetworkBurst);
    expected.next_u64();
    expected.next_u64();
    assert_eq!(serialized(actual), serialized(&expected));
    let count = n.unshape("burst").unwrap().faults.unwrap();
    assert_eq!((count.matched, count.burst_dropped), (5, 5));
    n.shape(
        2,
        "fresh".into(),
        &[MachineId(0)],
        &[MachineId(1)],
        policy(10),
        f,
    );
    assert!(n.layers[&2].burst.is_empty());
    n.route(MachineId(0), 6, &plain(0, 1, 6)).unwrap();
    assert_eq!(n.layers[&2].burst[&(MachineId(0), MachineId(1))], 2);
}
#[test]
fn reorder_spike_reset_duplicate_and_eligible_reset_draws_are_exact() {
    let f = NetworkFaults {
        reorder_ppm: 1_000_000,
        reorder_delay_ns: 1,
        latency_spike_ppm: 1_000_000,
        latency_spike_ns: 7,
        tcp_reset_ppm: 1_000_000,
        duplicate_ppm: 1_000_000,
        ..NetworkFaults::default()
    };
    let mut n = fresh();
    n.shape(
        1,
        "all".into(),
        &[MachineId(0)],
        &[MachineId(1)],
        policy(10),
        f,
    );
    let syn = tcp(0, 1, (1234, 80), 7, 0, 2);
    n.route(MachineId(0), 0, &syn).unwrap();
    let pair = (MachineId(0), MachineId(1));
    let pristine = SeededRng::derived(pair_seed(7, "m1"), RngDomain::NetworkReset);
    assert_eq!(serialized(&n.pairs[&pair].faults[4]), serialized(&pristine));
    let ack = tcp(0, 1, (1234, 80), 8, 1, 0x10);
    n.route(MachineId(0), 0, &ack).unwrap();
    let flights = n.in_flight();
    assert_eq!(
        flights.iter().map(|x| x.0).collect::<Vec<_>>(),
        [18, 19, 19, 20]
    );
    let expected = seedbox_engine_core::connections::reset_frame(&ack).unwrap();
    assert_eq!(flights[2].3, expected);
    assert_eq!(flights[3].3, expected);
    let mut expected_rng = pristine;
    expected_rng.next_u64();
    assert_eq!(
        serialized(&n.pairs[&pair].faults[4]),
        serialized(&expected_rng)
    );
    let counts = n.unshape("all").unwrap().faults.unwrap();
    assert_eq!(
        (
            counts.matched,
            counts.reordered,
            counts.spiked,
            counts.tcp_reset,
            counts.duplicated,
            counts.delayed_max_ns
        ),
        (2, 2, 2, 1, 2, 8)
    );
    // Added reorder delay is outside base FIFO: a later unshaped frame overtakes.
    let mut n = fresh();
    n.shape(
        1,
        "hold".into(),
        &[MachineId(0)],
        &[MachineId(1)],
        policy(10),
        NetworkFaults {
            reorder_ppm: 1_000_000,
            reorder_delay_ns: 1,
            latency_spike_ppm: 1_000_000,
            latency_spike_ns: 100,
            ..NetworkFaults::default()
        },
    );
    n.route(MachineId(0), 0, &plain(0, 1, 1)).unwrap();
    n.unshape("hold");
    n.route(MachineId(0), 0, &plain(0, 1, 2)).unwrap();
    assert_eq!(
        n.in_flight().iter().map(|x| x.0).collect::<Vec<_>>(),
        [11, 111]
    );
}
#[test]
fn extra_delay_saturates_and_overflowing_duplicate_is_skipped() {
    let mut n = fresh();
    n.shape(
        1,
        "tail".into(),
        &[MachineId(0)],
        &[MachineId(1)],
        policy(10),
        NetworkFaults {
            duplicate_ppm: 1_000_000,
            latency_spike_ppm: 1_000_000,
            latency_spike_ns: 100,
            ..NetworkFaults::default()
        },
    );
    n.route(MachineId(0), u64::MAX - 10, &plain(0, 1, 1))
        .unwrap();
    assert_eq!(n.in_flight().len(), 1);
    assert_eq!(n.next_arrival(), Some(u64::MAX));
    assert_eq!(n.layers[&1].counts.duplicated, 0);
}
#[test]
fn complete_policy_tracker_rng_counters_roundtrip_with_fresh_observer() {
    let mut n = net(
        NetworkSetup::new(config(), Some(1_000_000_000), None, vec![])
            .unwrap()
            .with_frame_capacity(2),
    );
    n.route(MachineId(0), 0, &tcp(0, 1, (1234, 80), 7, 0, 2))
        .unwrap();
    let c = n.connections("m0", "m1", 0).pop().unwrap();
    n.select(2, "flow".into(), c).unwrap();
    n.shape(
        3,
        "burst".into(),
        &[MachineId(1)],
        &[MachineId(0)],
        policy(10),
        NetworkFaults {
            burst_loss_ppm: 1_000_000,
            burst_length: 3,
            ..NetworkFaults::default()
        },
    );
    n.route(MachineId(1), 0, &plain(1, 0, 3)).unwrap();
    n.route(MachineId(0), 0, &tcp(0, 1, (1234, 80), 8, 1, 0x10))
        .unwrap();
    assert_eq!(n.observations.frames.len(), 2);
    assert_eq!(n.observations.omitted, 1);
    let mut restored = decode(&n, 0);
    let original = n.drain_frames();
    assert_eq!(original.facts.len(), 2);
    assert_eq!(original.omitted, 1);
    assert!(restored.drain_frames().facts.is_empty());
    assert_eq!(restored.observations.capacity, 0);
    for i in 1..6 {
        for net in [&mut n, &mut restored] {
            net.route(MachineId(1), i, &plain(1, 0, i as u8)).unwrap();
        }
    }
    assert_eq!(serialized(&n), serialized(&restored));
    let mut bad = restored.clone();
    bad.pairs
        .get_mut(&(MachineId(0), MachineId(1)))
        .unwrap()
        .faults[0] = SeededRng::derived(999, RngDomain::NetworkReorder);
    assert!(matches!(
        bad.check_persist(3, 6, &[]),
        Err(CodecError::Invalid("RNG owner/domain/seed"))
    ));
}
#[test]
fn malformed_layers_reject_but_observer_fields_never_enter_continuation() {
    let mut n = net(NetworkSetup::from(config()).with_frame_capacity(1));
    n.shape(
        1,
        "layer".into(),
        &[MachineId(0)],
        &[MachineId(1)],
        policy(10),
        NetworkFaults::default(),
    );
    n.route(MachineId(0), 0, &plain(0, 1, 1)).unwrap();
    for case in 0..3 {
        let mut bad = n.clone();
        match case {
            0 => bad.layers.get_mut(&1).unwrap().queue.matched = 1,
            1 => bad.layers.get_mut(&1).unwrap().counts.matched = 1,
            _ => {
                bad.layers
                    .get_mut(&1)
                    .unwrap()
                    .burst
                    .insert((MachineId(0), MachineId(1)), 0);
            }
        }
        assert!(bad.check_persist(3, 0, &[]).is_err(), "case {case}");
    }
    let expected = serialized(&n);
    // Even deliberately nonsensical observer copies cannot reach the encoder.
    n.observations.frames[0].due = Some(Timestamp::from_nanos(0));
    n.observations.frames[0].position.boundary_index = 1;
    // Old codec negative case: retained facts with capacity zero are not a
    // continuing-state relation at all; even malformed private copies stay out.
    n.observations.capacity = 0;
    assert_eq!(serialized(&n), expected);
    n.observations.capacity = usize::MAX;
    n.observations.omitted = u64::MAX;
    assert_eq!(serialized(&n), expected);
    assert!(n.check_persist(3, 0, &[]).is_ok());
}

#[test]
fn validation_index_items_are_reserved_before_validation_allocations() {
    let mut bytes = 1u64.to_le_bytes().to_vec();
    bytes.extend(3u64.to_le_bytes());
    let mut l = limits();
    l.total_items = 2;
    let mut d = Decoder::new(&bytes[..], bytes.len() as u64, l);
    assert_eq!(Vec::<u64>::get(&mut d).unwrap(), [3]);
    assert!(matches!(
        d.validation_scratch(0),
        Err(CodecError::Limit("aggregate items"))
    ));
}
struct Prefix<'a>(&'a [u8]);
impl std::io::Read for Prefix<'_> {
    fn read(&mut self, b: &mut [u8]) -> std::io::Result<usize> {
        assert!(!self.0.is_empty(), "forbidden element read");
        let n = b.len().min(self.0.len());
        b[..n].copy_from_slice(&self.0[..n]);
        self.0 = &self.0[n..];
        Ok(n)
    }
}
#[test]
fn tracker_capacity_and_conversion_items_reject_before_element_reads() {
    use seedbox_engine_core::connections::ConnectionState;
    for count in [1025, u64::MAX] {
        let bytes = count.to_le_bytes();
        let mut d = Decoder::new(Prefix(&bytes), 1_000_000, limits());
        assert!(matches!(
            ConnectionState::get(&mut d),
            Err(CodecError::Invalid("connection capacity"))
        ));
    }
    let bytes = 1u64.to_le_bytes();
    let mut l = limits();
    l.total_items = 1;
    let mut d = Decoder::new(Prefix(&bytes), 1_000_000, l);
    assert!(matches!(
        ConnectionState::get(&mut d),
        Err(CodecError::Limit("aggregate items"))
    ));
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&0u64.to_le_bytes());
    bytes.extend_from_slice(&0u64.to_le_bytes());
    bytes.extend_from_slice(&0u64.to_le_bytes());
    bytes.push(0);
    bytes.extend_from_slice(&1025u64.to_le_bytes());
    let mut d = Decoder::new(Prefix(&bytes), 1_000_000, limits());
    assert!(matches!(
        ConnectionState::get(&mut d),
        Err(CodecError::Invalid("connection capacity"))
    ));
    let n = fresh();
    let bytes = serialized(&n);
    let mut l = limits();
    l.total_items = 3;
    let mut d = Decoder::new(Prefix(&bytes[..bytes.len() - 16]), bytes.len() as u64, l);
    assert!(matches!(
        Network::get(&mut d),
        Err(CodecError::Limit("aggregate items"))
    ));
}
