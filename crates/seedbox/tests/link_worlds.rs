use seedbox_engine_core::{faults, links};
// Exercise the host fixture's JSON through the actual portable VMM modules.

use links::{Links, Outcome, Policy};
use serde_json::Value;

fn fixture(text: &str) -> faults::Faults {
    let world: Value = serde_json::from_str(text).unwrap();
    let names = ["a", "b", "c"]
        .into_iter()
        .map(str::to_owned)
        .collect::<Vec<_>>();
    let links = Links::from_config(1, &names, &world["network"]).unwrap();
    faults::Faults::with_links(world["faults"].as_array().unwrap().clone(), names, links).unwrap()
}

#[test]
fn full_world_window_45_reveals_lossmid_after_cut_heal_and_bounds_nominal_arrivals() {
    let mut f = fixture(include_str!("../../../harness/worlds/shaped.json"));
    let mut received = 0;
    let mut last = 0;
    // 400 broadcasts per 100 ms window; other directions exercise the same
    // live layers, while maintaining independent per-pair streams.
    for send in (0..5_000_000_000).step_by(250_000) {
        f.apply_due(send);
        for src in 0..3 {
            for dst in links::candidates(src, [0xff; 6], &[[0; 6]; 3]) {
                let cut = f.blocked(src, dst);
                let wi = send / 100_000_000;
                if (20..50).contains(&wi) {
                    let latency_ms = match (src, dst, wi) {
                        (0, 1, _) => 3,
                        (0, 2, 25..=29) | (1, 2, 25..=29) | (1, 2, 35..=39) => 5,
                        (0, 2, 30..=39) | (1, 2, 30..=34) => 8,
                        _ => 1,
                    };
                    let lossmid = src == 2 && dst == 0 && (40..47).contains(&wi);
                    let lossall = src == 2 && dst == 1 && (40..47).contains(&wi);
                    assert_eq!(
                        f.links.policy((src, dst)),
                        Policy {
                            latency_ns: latency_ms * 1_000_000,
                            jitter_ns: if lossmid { 500_000 } else { 0 },
                            loss_ppm: if lossmid {
                                500_000
                            } else if lossall {
                                1_000_000
                            } else {
                                0
                            },
                            bandwidth_bps: None,
                            queue_bound_ns: None
                        }
                    );
                    assert_eq!(
                        cut,
                        matches!((src, dst), (0, 2) | (2, 0)) && (43..45).contains(&wi)
                    );
                }
                let outcome = f.links.decide(src, dst, send, 1500, cut);
                if src != 2 || dst != 0 {
                    continue;
                }
                if (4_300_000_000..4_500_000_000).contains(&send) {
                    assert_eq!(outcome, Outcome::Cut);
                }
                if (4_500_000_000..4_600_000_000).contains(&send) {
                    assert!(!cut);
                    assert_eq!(
                        f.links.policy((2, 0)),
                        Policy {
                            latency_ns: 1_000_000,
                            jitter_ns: 500_000,
                            loss_ppm: 500_000,
                            bandwidth_bps: None,
                            queue_bound_ns: None
                        }
                    );
                    match outcome {
                        Outcome::Deliver(at) => {
                            assert!((1_000_000..=1_500_000).contains(&(at - send)));
                            assert!(at > last);
                            received += 1;
                        }
                        Outcome::Lost => {}
                        Outcome::QueueDropped => panic!("unlimited fixture must not tail-drop"),
                        Outcome::Cut => panic!("healing cut must reveal the live lossmid layer"),
                    }
                }
                if let Outcome::Deliver(at) = outcome {
                    last = at;
                }
            }
        }
    }
    // Same four-sigma count criterion as the guest checker, without claiming
    // exact guest counts (ARP frames and actual sends determine stream positions).
    assert!(
        (159..=241).contains(&received),
        "received {received} of 400"
    );
    assert!(f.error.is_none());
    assert_eq!(f.log.len(), 16);
    for (i, log) in f.log.iter().enumerate() {
        assert_eq!(log["id"], i + 1);
        assert_eq!(log["status"], "applied");
        assert_eq!(log["applied_at_ns"], log["requested_at_ns"]);
    }
    assert!(f.active_handles().is_empty());
}

#[test]
fn short_world_applies_the_same_ordered_faults_and_exposes_at_least_300_lossmid_sends() {
    let mut f = fixture(include_str!(
        "../../../harness/worlds/shaped-tol-short.json"
    ));
    let mut exposed = 0;
    // World cmdline selects link-short.init's 1 ms gap; probe end is 1.1 s.
    // These are scheduled candidates, not measured guest sends or receipts.
    for send in (0..1_100_000_000).step_by(1_000_000) {
        f.apply_due(send);
        if !f.blocked(2, 0) && f.links.policy((2, 0)).loss_ppm == 500_000 {
            assert_eq!(
                f.links.policy((2, 0)),
                Policy {
                    latency_ns: 10_000_000,
                    jitter_ns: 500_000,
                    loss_ppm: 500_000,
                    bandwidth_bps: None,
                    queue_bound_ns: None
                }
            );
            exposed += 1;
        }
    }
    assert_eq!(exposed, 340);
    assert!(exposed >= 300);
    assert!(f.error.is_none());
    assert_eq!(f.log.len(), 16);
    for (i, log) in f.log.iter().enumerate() {
        assert_eq!(log["id"], i + 1);
        assert_eq!(log["status"], "applied");
        assert_eq!(log["applied_at_ns"], log["requested_at_ns"]);
    }
}
