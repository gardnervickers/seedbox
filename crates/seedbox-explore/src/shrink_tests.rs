use super::*;
use serde_json::json;

fn input(key: &str, at_ns: u64, action: serde_json::Value) -> TimedInput {
    serde_json::from_value(json!({"key": key, "at_ns": at_ns, "action": action})).unwrap()
}

fn program() -> Vec<TimedInput> {
    let crash = |m: &str| json!({"action":"crash","machine":m});
    let reboot = |m: &str| json!({"action":"reboot","machine":m});
    vec![
        input("prefix", 1, crash("db")),
        input("gen-0", 10, crash("db")),
        input("gen-1", 20, crash("db")),
        input("anchor", 25, json!({"action":"anchor"})),
        input(
            "reseed",
            25,
            json!({"action":"reseed","seed":3,"identity":"child"}),
        ),
        input(
            "cut",
            26,
            json!({"action":"partition","handle":"cut","left":["a"],"right":["b"]}),
        ),
        input("gen-0-end", 30, reboot("db")),
        input("gen-1-end", 40, reboot("db")),
        input("heal", 45, json!({"action":"heal","handle":"cut"})),
        input("x", 50, crash("db")),
        input("y", 60, crash("db")),
    ]
}

fn shrink(fails: impl Fn(&BTreeSet<&str>) -> bool, budget: u32) -> (Result, u32) {
    let mut shrinker = Shrinker::new(candidates(&program(), 1), budget);
    while let Some(keep) = shrinker.next_probe() {
        let kept: BTreeSet<&str> = keep.iter().map(String::as_str).collect();
        assert!(kept.contains("prefix"), "the fixed prefix stays");
        // Dependents never outlive what they need.
        assert!(!kept.contains("reseed") || kept.contains("anchor"));
        assert!(!kept.contains("heal") || kept.contains("cut"));
        assert_eq!(kept.contains("gen-1"), kept.contains("gen-1-end"));
        shrinker.report(fails(&kept)).unwrap();
    }
    (shrinker.result().unwrap().clone(), shrinker.probes())
}

#[test]
fn groups_teardown_pairs_and_tracks_dependencies() {
    let c = candidates(&program(), 1);
    assert_eq!(c.fixed, ["prefix"]);
    assert!(c
        .atoms
        .contains(&vec!["gen-1".to_string(), "gen-1-end".to_string()]));
    assert_eq!(c.requires["reseed"], "anchor");
    assert_eq!(c.requires["heal"], "cut");
}

#[test]
fn ddmin_finds_a_one_minimal_program_respecting_dependencies() {
    let (result, probes) = shrink(
        |k| k.contains("gen-1") && k.contains("reseed") && k.contains("x"),
        200,
    );
    assert_eq!(
        result,
        Result::Minimal {
            keep: ["prefix", "gen-1", "gen-1-end", "anchor", "reseed", "x"]
                .map(String::from)
                .to_vec()
        }
    );
    assert!(probes < 60, "{probes} probes");
}

#[test]
fn an_unreproduced_failure_and_an_exhausted_budget_are_reported() {
    assert_eq!(shrink(|_| false, 10).0, Result::NotReproduced);
    let (result, probes) = shrink(|k| k.contains("x") && k.contains("y"), 3);
    assert!(matches!(result, Result::Reduced { .. }));
    assert_eq!(probes, 3);
    // Everything removable can go when the failure needs none of it.
    assert_eq!(
        shrink(|_| true, 100).0,
        Result::Minimal {
            keep: vec!["prefix".into()]
        }
    );
}
