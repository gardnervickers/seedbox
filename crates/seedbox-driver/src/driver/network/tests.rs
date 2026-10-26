use super::*;

fn mac(n: u8) -> MacAddress {
    MacAddress::new([2, 0, 0, 0, 0, n]).unwrap()
}
fn frame(to: [u8; 6], tag: u8) -> Vec<u8> {
    let mut bytes = to.to_vec();
    bytes.extend_from_slice(&[2, 0, 0, 0, 0, 0, 0x88, 0xb5, tag]);
    bytes
}
fn interface(n: u8) -> NetworkInterface {
    NetworkInterface::new(mac(n), std::net::Ipv4Addr::new(10, 0, 0, n), 16).unwrap()
}
fn network(config: NetworkConfig, attached: &[bool]) -> Network {
    let mut network = Network::new(config);
    for (index, &on) in attached.iter().enumerate() {
        network.admit(
            MachineId(index),
            &format!("m{index}"),
            index as u64 + 7,
            on.then(|| interface(index as u8 + 1)).as_ref(),
        );
    }
    network
}
fn ms(n: u64) -> Duration {
    Duration::from_millis(n)
}

#[test]
fn configuration_bounds_match_legacy_link_policy() {
    use InvalidNetworkConfig::*;
    assert_eq!(NetworkConfig::new(ms(0), ms(0), 0), Err(ZeroLatency));
    assert_eq!(
        NetworkConfig::new(ms(1), ms(1), 0),
        Err(JitterNotBelowLatency)
    );
    let hour = Duration::from_secs(3600);
    assert!(NetworkConfig::new(hour, Duration::ZERO, 0).is_ok());
    assert_eq!(
        NetworkConfig::new(hour, Duration::from_nanos(1), 0),
        Err(DelayTooLong)
    );
    assert_eq!(
        NetworkConfig::new(Duration::MAX, Duration::ZERO, 0),
        Err(DelayTooLong)
    );
    assert_eq!(
        NetworkConfig::new(ms(1), ms(0), 1_000_001),
        Err(LossAboveCertain)
    );
    let config = NetworkConfig::new(ms(2), ms(1), 1_000_000).unwrap();
    assert_eq!(
        (config.latency(), config.jitter(), config.loss_ppm()),
        (ms(2), ms(1), 1_000_000)
    );
    assert_eq!(
        NetworkConfig::default(),
        NetworkConfig::new(ms(1), ms(0), 0).unwrap()
    );
}

#[test]
fn broadcast_unicast_and_unroutable_candidates_follow_legacy_rules() {
    let mut net = network(NetworkConfig::default(), &[true, false, true, true]);
    net.route(MachineId(0), 0, &frame([0xff; 6], 1)).unwrap();
    net.route(MachineId(0), 0, &frame(mac(4).octets(), 2))
        .unwrap();
    // Unattached owner, self, unknown and other group addresses reach nobody.
    for to in [
        mac(2).octets(),
        mac(1).octets(),
        [2, 9, 9, 9, 9, 9],
        [1, 0, 0x5e, 0, 0, 1],
    ] {
        net.route(MachineId(0), 0, &frame(to, 3)).unwrap();
    }
    let flights: Vec<_> = net
        .in_flight()
        .into_iter()
        .map(|(at, from, to, bytes)| (at, from, to, bytes[14]))
        .collect();
    // Same-arrival order: sender, sender sequence, receiver; strict pair FIFO.
    assert_eq!(
        flights,
        [
            (1_000_000, MachineId(0), MachineId(2), 1),
            (1_000_000, MachineId(0), MachineId(3), 1),
            (1_000_001, MachineId(0), MachineId(3), 2),
        ]
    );
}

#[test]
fn delivery_is_exact_and_due_frames_leave_in_key_order() {
    let mut net = network(NetworkConfig::default(), &[true, true, true]);
    net.route(MachineId(2), 5, &frame(mac(1).octets(), 1))
        .unwrap();
    net.route(MachineId(1), 5, &frame(mac(1).octets(), 2))
        .unwrap();
    assert_eq!(net.next_arrival(), Some(1_000_005));
    assert!(net.take_due(1_000_004).is_empty());
    let due = net.take_due(1_000_005);
    assert_eq!(
        due.iter().map(|d| (d.to, d.bytes[14])).collect::<Vec<_>>(),
        [(MachineId(0), 2), (MachineId(0), 1)]
    );
    assert!(net.is_idle());
}

#[test]
fn disabled_policies_draw_nothing_and_enabled_streams_are_independent() {
    let mut quiet = network(NetworkConfig::default(), &[true, true]);
    quiet
        .route(MachineId(0), 0, &frame(mac(2).octets(), 0))
        .unwrap();
    let untouched = network(NetworkConfig::default(), &[true, true]);
    // The pair exists, but neither stream advanced from a fresh derivation.
    let fresh = {
        let mut probe = untouched.clone();
        probe.pairs.insert(
            (MachineId(0), MachineId(1)),
            Pair {
                loss: SeededRng::derived(pair_seed(7, "m1"), RngDomain::NetworkLoss),
                jitter: SeededRng::derived(pair_seed(7, "m1"), RngDomain::NetworkJitter),
                last: None,
                ..Pair::new(pair_seed(7, "m1"))
            },
        );
        probe.pair_cursor(MachineId(0), MachineId(1))
    };
    assert_eq!(quiet.pair_cursor(MachineId(0), MachineId(1)), fresh);

    let config = NetworkConfig::new(ms(1), Duration::from_nanos(999), 500_000).unwrap();
    let mut a = network(config, &[true, true, true]);
    let mut b = network(config, &[true, true, true]);
    // Traffic on another pair never shifts this pair's draws.
    for tag in 0..50 {
        b.route(MachineId(0), 0, &frame(mac(3).octets(), tag))
            .unwrap();
    }
    for tag in 0..50 {
        a.route(MachineId(0), u64::from(tag), &frame(mac(2).octets(), tag))
            .unwrap();
        b.route(MachineId(0), u64::from(tag), &frame(mac(2).octets(), tag))
            .unwrap();
    }
    let pair = |n: &Network| {
        n.in_flight()
            .into_iter()
            .filter(|f| f.2 == MachineId(1))
            .collect::<Vec<_>>()
    };
    assert_eq!(pair(&a), pair(&b));
    let delivered = pair(&a);
    // Seeded loss drops some but not all; jitter stays within 0..=999.
    assert!(!delivered.is_empty() && delivered.len() < 50);
    for (arrival, ..) in &delivered {
        assert!((1_000_000..=1_001_100).contains(arrival));
    }
    assert!(delivered.windows(2).all(|w| w[0].0 < w[1].0));
    // Loss and jitter streams of one pair never share bytes.
    let seed = pair_seed(7, "m1");
    let mut loss = SeededRng::derived(seed, RngDomain::NetworkLoss);
    let mut jitter = SeededRng::derived(seed, RngDomain::NetworkJitter);
    assert_ne!(loss.next_u64(), jitter.next_u64());
    assert_ne!(pair_seed(7, "m1"), pair_seed(7, "m2"));
    assert_ne!(pair_seed(7, "m1"), pair_seed(8, "m1"));
}

#[test]
fn certain_loss_drops_every_candidate() {
    let config = NetworkConfig::new(ms(1), ms(0), 1_000_000).unwrap();
    let mut net = network(config, &[true, true]);
    for tag in 0..20 {
        net.route(MachineId(0), 0, &frame([0xff; 6], tag)).unwrap();
    }
    assert!(net.is_idle());
}

#[test]
fn unrepresentable_arrivals_fail_without_scheduling() {
    let mut net = network(NetworkConfig::default(), &[true, true]);
    assert_eq!(
        net.route(MachineId(0), u64::MAX - 999_999, &frame(mac(2).octets(), 0)),
        Err(RouteError::ArrivalOverflow)
    );
    assert!(net.is_idle());
    // The pair FIFO successor of the last representable nanosecond overflows too.
    let mut net = network(NetworkConfig::default(), &[true, true]);
    net.route(
        MachineId(0),
        u64::MAX - 1_000_000,
        &frame(mac(2).octets(), 0),
    )
    .unwrap();
    assert_eq!(net.next_arrival(), Some(u64::MAX));
    assert_eq!(
        net.route(
            MachineId(0),
            u64::MAX - 1_000_000,
            &frame(mac(2).octets(), 1)
        ),
        Err(RouteError::ArrivalOverflow)
    );
}

#[test]
fn an_exhausted_sender_sequence_fails_without_scheduling() {
    let mut net = network(NetworkConfig::default(), &[true, true, true]);
    net.set_sequence(MachineId(0), u64::MAX - 1);
    // The last representable sequence orders one more candidate.
    net.route(MachineId(0), 0, &frame(mac(2).octets(), 0))
        .unwrap();
    assert_eq!(
        net.route(MachineId(0), 0, &frame(mac(3).octets(), 1)),
        Err(RouteError::SequenceOverflow)
    );
    assert_eq!(net.in_flight().len(), 1);
    // Other senders keep their own sequences.
    net.route(MachineId(1), 0, &frame(mac(3).octets(), 2))
        .unwrap();
}

/// The next loss and jitter draws of a fresh pair stream after `draws` each.
fn fresh(from_seed: u64, to: &str, draws: [usize; 2]) -> [u64; 2] {
    let seed = pair_seed(from_seed, to);
    let mut loss = SeededRng::derived(seed, RngDomain::NetworkLoss);
    let mut jitter = SeededRng::derived(seed, RngDomain::NetworkJitter);
    for _ in 0..draws[0] {
        loss.next_u64();
    }
    for _ in 0..draws[1] {
        jitter.next_u64();
    }
    [loss.next_u64(), jitter.next_u64()]
}

#[test]
fn each_policy_draws_only_its_own_stream_and_lost_frames_still_draw_jitter() {
    let route = |config, frames| {
        let mut net = network(config, &[true, true]);
        for tag in 0..frames {
            net.route(MachineId(0), 0, &frame(mac(2).octets(), tag))
                .unwrap();
        }
        (
            net.pair_cursor(MachineId(0), MachineId(1)).unwrap(),
            net.in_flight().len(),
        )
    };
    let loss_only = NetworkConfig::new(ms(1), ms(0), 500_000).unwrap();
    assert_eq!(route(loss_only, 8).0, fresh(7, "m1", [8, 0]));
    let jitter_only = NetworkConfig::new(ms(1), Duration::from_nanos(10), 0).unwrap();
    assert_eq!(route(jitter_only, 8), (fresh(7, "m1", [0, 8]), 8));
    // Every candidate is lost, yet each consumes its jitter draw.
    let all_lost = NetworkConfig::new(ms(1), Duration::from_nanos(10), 1_000_000).unwrap();
    assert_eq!(route(all_lost, 8), (fresh(7, "m1", [8, 8]), 0));
}
