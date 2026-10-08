use links::Outcome;
use seedbox_control::{Action, Plan};
use seedbox_engine_core::{connections, faults, links};
use sha2::{Digest, Sha256};

fn frame(port: u16, reverse: bool, seq: u32, ack: u32, flags: u8) -> Vec<u8> {
    let mut f = vec![0; 55];
    f[12..14].copy_from_slice(&[8, 0]);
    f[14] = 0x45;
    f[16..18].copy_from_slice(&41u16.to_be_bytes());
    f[23] = 6;
    let (a, b, sport, dport) = if reverse {
        (2, 1, 80, port)
    } else {
        (1, 2, port, 80)
    };
    f[26..30].copy_from_slice(&[10, 0, 0, a]);
    f[30..34].copy_from_slice(&[10, 0, 0, b]);
    f[34..36].copy_from_slice(&sport.to_be_bytes());
    f[36..38].copy_from_slice(&dport.to_be_bytes());
    f[38..42].copy_from_slice(&seq.to_be_bytes());
    f[42..46].copy_from_slice(&ack.to_be_bytes());
    f[46] = 0x50;
    f[47] = flags;
    f
}
fn fixture() -> faults::Faults {
    faults::Faults::new(vec![], vec!["client".into(), "server".into()]).unwrap()
}
fn apply(f: &mut faults::Faults, action: Action, at: u64) {
    f.append(Plan {
        id: 0,
        at_ns: at,
        deadline_ns: at + 100,
        action,
    })
    .unwrap();
    f.apply_due(at);
    assert!(f.error.is_none(), "{:?}", f.error);
}
fn hash(f: &faults::Faults) -> Vec<u8> {
    let mut h = Sha256::new();
    f.network_witness(&mut h);
    h.finalize().to_vec()
}

#[test]
fn two_simultaneous_flows_direction_retransmissions_overlap_and_tuple_reuse() {
    let mut f = fixture();
    for port in [4000, 4001] {
        assert!(matches!(
            f.route(0, 1, 0, &frame(port, false, 7, 0, 2), false),
            Outcome::Deliver(_)
        ));
        f.route(1, 0, 1, &frame(port, true, 50, 8, 0x12), false);
    }
    let selected = f.connections("server", "client", 0)[0].clone();
    assert_eq!(selected.dst_port, 4000);
    let queued = f.route(1, 0, 2, &frame(4000, true, 51, 8, 0x18), false);
    apply(
        &mut f,
        Action::DropConnection {
            handle: "one".into(),
            connection: selected.clone(),
        },
        3,
    );
    apply(
        &mut f,
        Action::DropConnection {
            handle: "two".into(),
            connection: selected.clone(),
        },
        3,
    );
    assert_eq!(queued, Outcome::Deliver(4)); // previously scheduled arrival survives
    for _ in 0..2 {
        assert_eq!(
            f.route(1, 0, 4, &frame(4000, true, 50, 8, 0x12), false),
            Outcome::Lost
        );
        assert_eq!(
            f.route(1, 0, 4, &frame(4000, true, 51, 8, 0x18), false),
            Outcome::Lost
        );
        assert!(matches!(
            f.route(1, 0, 4, &frame(4001, true, 51, 8, 0x18), false),
            Outcome::Deliver(_)
        ));
        assert!(matches!(
            f.route(0, 1, 4, &frame(4000, false, 7, 0, 2), false),
            Outcome::Deliver(_)
        ));
    }
    assert_eq!(f.connections("server", "client", 0)[0], selected);
    apply(
        &mut f,
        Action::Heal {
            handle: "one".into(),
        },
        5,
    );
    assert_eq!(f.log[2]["selection_counts"]["dropped"], 4);
    assert_eq!(
        f.route(1, 0, 6, &frame(4000, true, 51, 8, 0x18), false),
        Outcome::Lost
    );
    // FIN is half-close; both FINs end admission, retransmitted data still belongs
    // to the old incarnation until a fresh opening SYN is observed.
    f.route(0, 1, 7, &frame(4000, false, 8, 51, 0x11), false);
    assert!(f
        .admission_error(&Action::DropConnection {
            handle: "x".into(),
            connection: selected.clone()
        })
        .is_none());
    f.route(1, 0, 8, &frame(4000, true, 52, 9, 0x11), false);
    assert!(f
        .admission_error(&Action::DropConnection {
            handle: "x".into(),
            connection: selected.clone()
        })
        .is_some());
    f.route(0, 1, 9, &frame(4000, false, 100, 0, 2), false);
    let replacement = f
        .connections("server", "client", selected.incarnation)
        .into_iter()
        .find(|c| c.dst_port == 4000)
        .unwrap();
    assert_ne!(replacement.incarnation, selected.incarnation);
    assert!(matches!(
        f.route(1, 0, 10, &frame(4000, true, 200, 101, 0x12), false),
        Outcome::Deliver(_)
    ));
    assert!(matches!(
        f.route(1, 0, 11, &frame(4000, true, 201, 101, 0x18), false),
        Outcome::Deliver(_)
    ));
    apply(
        &mut f,
        Action::Heal {
            handle: "two".into(),
        },
        12,
    );
    assert!(f.active_handles().is_empty());
}

#[test]
fn rst_changed_syn_simultaneous_open_and_process_restart_do_not_reset_nic() {
    let mut f = fixture();
    f.route(0, 1, 0, &frame(4000, false, u32::MAX, 0, 2), false);
    let first = f.connections("client", "server", 0)[0].clone();
    // Valid wraparound SYN acknowledgement and simultaneous open retain identity.
    f.route(1, 0, 1, &frame(4000, true, 10, 0, 2), false);
    f.route(1, 0, 2, &frame(4000, true, 10, 0, 0x12), false);
    assert_eq!(f.connections("client", "server", 0)[0], first);
    apply(
        &mut f,
        Action::Stop {
            machine: "client".into(),
        },
        3,
    );
    // Scheduling a process operation leaves TCP classification untouched.
    assert_eq!(f.connections("client", "server", 0)[0], first);
    f.route(0, 1, 4, &frame(4000, false, 5, 0, 2), false);
    let second = f.connections("client", "server", 0)[0].clone();
    assert_ne!(first.incarnation, second.incarnation);
    f.route(0, 1, 5, &frame(4000, false, 6, 0, 4), false);
    assert!(f.connections("client", "server", 0).is_empty());
    f.route(0, 1, 6, &frame(4000, false, 5, 0, 2), false);
    assert_ne!(
        f.connections("client", "server", 0)[0].incarnation,
        second.incarnation
    );
}

#[test]
fn unsupported_malformed_and_fragmented_traffic_bypass_selector_but_not_links() {
    let mut f = fixture();
    let syn = frame(4000, false, 1, 0, 2);
    f.route(0, 1, 0, &syn, false);
    let c = f.connections("client", "server", 0)[0].clone();
    apply(
        &mut f,
        Action::DropConnection {
            handle: "s".into(),
            connection: c,
        },
        1,
    );
    let mut cases = vec![];
    for (offset, value) in [
        (12, 0x86),
        (13, 6),
        (14, 0x44),
        (14, 0x65),
        (20, 0x20),
        (21, 1),
        (23, 17),
        (46, 0x40),
        (46, 0xf0),
        (0, 0xff),
        (16, 1),
    ] {
        let mut p = syn.clone();
        p[offset] = value;
        cases.push(p);
    }
    cases.extend((0..54).map(|len| syn[..len].to_vec()));
    for packet in cases {
        assert!(matches!(
            f.route(0, 1, 2, &packet, false),
            Outcome::Deliver(_)
        ));
        assert_eq!(f.route(0, 1, 3, &packet, true), Outcome::Cut);
    }
    assert_eq!(f.connections.selections["s"].dropped, 0);
    // A SYN-ACK with no matching acknowledgement does not join the incarnation.
    assert!(matches!(
        f.route(1, 0, 4, &frame(4000, true, 5, 99, 0x12), false),
        Outcome::Deliver(_)
    ));
    let mut midstream = fixture();
    midstream.route(0, 1, 0, &frame(4000, false, 2, 0, 0x18), false);
    assert!(midstream.connections("client", "server", 0).is_empty());
}

#[test]
fn passive_tracking_preserves_legacy_bytes_and_partition_rng_and_snapshot_state() {
    let mut f = fixture();
    let mut reference = fixture();
    for n in 0..10 {
        assert_eq!(
            f.route(0, 1, n, &frame(4000, false, 1, 0, 2), false),
            reference.links.decide(0, 1, n, 1500, false)
        );
        assert_eq!(hash(&f), hash(&reference));
        assert_eq!(
            serde_json::to_vec(&f.log).unwrap(),
            serde_json::to_vec(&reference.log).unwrap()
        );
    }
    let c = f.connections("client", "server", 0)[0].clone();
    apply(
        &mut f,
        Action::DropConnection {
            handle: "s".into(),
            connection: c.clone(),
        },
        10,
    );
    let snapshot = (f.connections.clone(), f.links.clone());
    let before = hash(&f);
    let mut link_hash = Sha256::new();
    f.links.witness(&mut link_hash);
    assert_eq!(
        f.route(0, 1, 11, &frame(4000, false, 2, 0, 0x18), true),
        Outcome::Cut
    );
    assert_eq!(f.connections.selections["s"].dropped, 0);
    let mut after_cut = Sha256::new();
    f.links.witness(&mut after_cut);
    assert_eq!(link_hash.finalize(), after_cut.finalize());
    let mut expected_links = f.links.clone();
    expected_links.selected_drop(0, 1);
    assert_eq!(
        f.route(0, 1, 12, &frame(4000, false, 2, 0, 0x18), false),
        Outcome::Lost
    );
    let mut after = Sha256::new();
    f.links.witness(&mut after);
    let mut expected = Sha256::new();
    expected_links.witness(&mut expected);
    assert_eq!(expected.finalize(), after.finalize());
    assert_ne!(before, hash(&f));
    (f.connections, f.links) = snapshot;
    assert_eq!(before, hash(&f));
    assert_eq!(f.connections("client", "server", 0), vec![c]);
    apply(&mut f, Action::Heal { handle: "s".into() }, 13);
    assert!(f.connections.used);
    assert_ne!(hash(&f), hash(&reference));
}

#[test]
fn bounded_eviction_pages_and_stale_selection_never_reattach() {
    let mut f = fixture();
    f.route(0, 1, 0, &frame(4000, false, 1, 0, 2), false);
    let c = f.connections("client", "server", 0)[0].clone();
    apply(
        &mut f,
        Action::DropConnection {
            handle: "s".into(),
            connection: c.clone(),
        },
        0,
    );
    for port in 4001..=4000 + connections::CAPACITY as u16 {
        f.route(0, 1, 1, &frame(port, false, 1, 0, 2), false);
    }
    assert!(f
        .admission_error(&Action::DropConnection {
            handle: "new".into(),
            connection: c.clone()
        })
        .is_none());
    // The oldest unselected open session is evicted instead of the selected one.
    assert!(!f
        .connections("client", "server", 0)
        .iter()
        .any(|c| c.src_port == 4001));
    let mut cursor = 0;
    let mut count = 0;
    loop {
        let page = f.connections("client", "server", cursor);
        if page.is_empty() {
            break;
        }
        assert!(page.len() <= 32);
        cursor = page.last().unwrap().incarnation;
        count += page.len();
    }
    assert_eq!(count, connections::CAPACITY);
    assert_eq!(
        f.route(0, 1, 2, &frame(4000, false, 2, 0, 0x18), false),
        Outcome::Lost
    );
    // A new ISN replaces the incarnation; the old selection cannot reattach.
    assert!(matches!(
        f.route(0, 1, 3, &frame(4000, false, 100, 0, 2), false),
        Outcome::Deliver(_)
    ));
    assert!(f
        .admission_error(&Action::DropConnection {
            handle: "stale".into(),
            connection: c
        })
        .is_some());
    assert!(matches!(
        f.route(0, 1, 4, &frame(4000, false, 101, 0, 0x18), false),
        Outcome::Deliver(_)
    ));
    assert_eq!(f.connections.selections["s"].dropped, 1);
}

#[test]
fn options_are_bounded_and_discovery_cannot_change_witness_or_selection() {
    let mut f = fixture();
    let base = frame(4000, false, 7, 0, 2);
    let mut options = base.clone();
    options.splice(34..34, [1, 1, 1, 1]); // IPv4 options
    options[14] = 0x46;
    options.splice(58..58, [1, 1, 1, 1]); // TCP options, before the payload
    options[50] = 0x60;
    options[16..18].copy_from_slice(&49u16.to_be_bytes());
    f.route(0, 1, 0, &options, false);
    let c = f.connections("client", "server", 0)[0].clone();
    apply(
        &mut f,
        Action::DropConnection {
            handle: "s".into(),
            connection: c.clone(),
        },
        1,
    );
    let before = hash(&f);
    for _ in 0..100 {
        assert_eq!(f.connections("client", "server", 0), vec![c.clone()]);
    }
    assert_eq!(hash(&f), before);
    assert_eq!(f.route(0, 1, 2, &options, false), Outcome::Lost);
    assert_eq!(f.route(0, 1, 3, &base, false), Outcome::Lost);
    assert_eq!(f.connections("client", "server", 0), vec![c]);
}

#[test]
fn no_selectors_preserve_bytes_with_link_shapes_cuts_and_random_loss() {
    let mut f = fixture();
    let mut reference = fixture();
    for state in [&mut f, &mut reference] {
        apply(
            state,
            Action::Shape {
                handle: "shape".into(),
                from: vec!["client".into()],
                to: vec!["server".into()],
                latency_ns: 10,
                jitter_ns: 9,
                loss_ppm: 500_000,
                bandwidth_bps: None,
                queue_bound_ns: None,
            },
            0,
        );
        apply(
            state,
            Action::Partition {
                handle: "cut".into(),
                left: vec!["client".into()],
                right: vec!["server".into()],
            },
            0,
        );
    }
    for n in 0..128 {
        if n == 16 {
            for state in [&mut f, &mut reference] {
                apply(
                    state,
                    Action::Heal {
                        handle: "cut".into(),
                    },
                    n,
                );
            }
        }
        let packet = frame(4000 + n as u16, false, 1, 0, 2);
        assert_eq!(
            f.route(0, 1, n, &packet, false),
            reference
                .links
                .decide(0, 1, n, 1500, reference.blocked(0, 1))
        );
        assert_eq!(hash(&f), hash(&reference));
        assert_eq!(
            serde_json::to_vec(&f.log).unwrap(),
            serde_json::to_vec(&reference.log).unwrap()
        );
    }
}

#[test]
fn selected_open_connection_survives_1024_short_connections() {
    let mut f = fixture();
    f.route(0, 1, 0, &frame(4000, false, 1, 0, 2), false);
    let c = f.connections("client", "server", 0)[0].clone();
    apply(
        &mut f,
        Action::DropConnection {
            handle: "s".into(),
            connection: c.clone(),
        },
        0,
    );
    // Also retain an older, unselected open connection: closed sessions go first.
    f.route(0, 1, 0, &frame(3999, false, 1, 0, 2), false);
    for port in 4001..=4000 + connections::CAPACITY as u16 {
        f.route(0, 1, 1, &frame(port, false, 1, 0, 2), false);
        f.route(0, 1, 2, &frame(port, false, 2, 0, 4), false);
    }
    assert_eq!(
        f.route(0, 1, 3, &frame(4000, false, 2, 0, 0x18), false),
        Outcome::Lost
    );
    let live = f.connections("client", "server", 0);
    assert_eq!(live.len(), 2);
    assert_eq!(live[0], c);
    assert_eq!(live[1].src_port, 3999);
    // The two oldest closed sessions were evicted, in incarnation order.
    for port in [4001, 4002] {
        assert!(f
            .connections
            .observe(0, 1, &frame(port, false, 3, 0, 0x18))
            .is_none());
    }
    assert!(f
        .connections
        .observe(0, 1, &frame(4003, false, 3, 0, 0x18))
        .is_some());
}

#[test]
fn full_selected_table_skips_new_tuples_and_protects_closed_selections() {
    let mut f = fixture();
    let mut cursor = 0;
    for port in 4000..4000 + connections::CAPACITY as u16 {
        f.route(0, 1, 0, &frame(port, false, 1, 0, 2), false);
        let c = f.connections("client", "server", cursor)[0].clone();
        cursor = c.incarnation;
        apply(
            &mut f,
            Action::DropConnection {
                handle: port.to_string(),
                connection: c,
            },
            0,
        );
    }
    let next_syn = frame(6000, false, 1, 0, 2);
    assert!(f.connections.observe(0, 1, &next_syn).is_none());
    assert_eq!(
        f.route(0, 1, 1, &frame(4000, false, 2, 0, 4), false),
        Outcome::Lost
    );
    // Even a closed incarnation remains protected until its handle is healed.
    assert!(f.connections.observe(0, 1, &next_syn).is_none());
    assert_eq!(
        f.route(0, 1, 2, &frame(4000, false, 2, 0, 0x18), false),
        Outcome::Lost
    );
    apply(
        &mut f,
        Action::Heal {
            handle: "4000".into(),
        },
        3,
    );
    assert!(f.connections.observe(0, 1, &next_syn).is_some());
    let c = f.connections("client", "server", cursor)[0].clone();
    assert_eq!(c.src_port, 6000);
    assert_eq!(c.incarnation, cursor + 1);
    assert_eq!(
        f.route(0, 1, 4, &frame(4001, false, 2, 0, 0x18), false),
        Outcome::Lost
    );
}

#[test]
fn selected_drop_preserves_other_flow_jitter_and_does_not_advance_fifo() {
    let mut plain = fixture();
    let mut selected = fixture();
    for state in [&mut plain, &mut selected] {
        apply(
            state,
            Action::Shape {
                handle: "jitter".into(),
                from: vec!["server".into()],
                to: vec!["client".into()],
                latency_ns: 1000,
                jitter_ns: 999,
                loss_ppm: 0,
                bandwidth_bps: None,
                queue_bound_ns: None,
            },
            0,
        );
        for port in [4000, 4001] {
            state.route(0, 1, 0, &frame(port, false, 7, 0, 2), false);
            state.route(1, 0, 1, &frame(port, true, 50, 8, 0x12), false);
        }
    }
    let c = selected.connections("server", "client", 0)[0].clone();
    // Overlapping selectors must still consume only one draw from each stream.
    for handle in ["one", "two"] {
        apply(
            &mut selected,
            Action::DropConnection {
                handle: handle.into(),
                connection: c.clone(),
            },
            2,
        );
    }
    let last = selected.links.last.clone();
    let packet = frame(4000, true, 51, 8, 0x18);
    assert!(matches!(
        plain.route(1, 0, 3000, &packet, false),
        Outcome::Deliver(_)
    ));
    assert_eq!(selected.route(1, 0, 3000, &packet, false), Outcome::Lost);
    assert_eq!(selected.links.last, last);
    let other = frame(4001, true, 51, 8, 0x18);
    // Spaced sends keep FIFO from masking a different jitter sample.
    let arrival = plain.route(1, 0, 6000, &other, false);
    assert!(matches!(arrival, Outcome::Deliver(_)));
    assert_eq!(selected.route(1, 0, 6000, &other, false), arrival);
    // With FIFO equal again, the full link witness also checks loss stream position.
    let mut a = Sha256::new();
    let mut b = Sha256::new();
    plain.links.witness(&mut a);
    selected.links.witness(&mut b);
    assert_eq!(a.finalize(), b.finalize());
    for row in &selected.log[1..] {
        assert_eq!(
            row["selection_counts"],
            serde_json::json!({"matched": 1, "dropped": 1})
        );
    }
}
