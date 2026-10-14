//! Hand-built list-append histories, ported from the original Python suite.
use super::*;
use crate::kit::Rng;

fn ok(id: i64, mops: Vec<Mop>) -> TxnOp {
    txn(id, "ok", mops, None, 0, 0, None).unwrap()
}
fn other(id: i64, kind: &str, mops: Vec<Mop>) -> TxnOp {
    txn(id, kind, mops, None, 0, 0, None).unwrap()
}
fn a(k: &str, v: i64) -> Mop {
    Mop::Append(k.into(), v)
}
fn r(k: &str, value: &[i64]) -> Mop {
    Mop::Read(k.into(), Some(value.to_vec()))
}
fn ri(k: i64, value: &[i64]) -> Mop {
    Mop::Read(Atom::Int(k), Some(value.to_vec()))
}
fn ai(k: i64, v: i64) -> Mop {
    Mop::Append(Atom::Int(k), v)
}
fn unknown(k: &str) -> Mop {
    Mop::Read(k.into(), None)
}
fn types(result: &Checked) -> Vec<&str> {
    result.counts.keys().map(String::as_str).collect()
}
fn serializable(ops: &[TxnOp]) -> Checked {
    check(ops, "serializable").unwrap()
}

#[test]
fn serial_history_is_valid() {
    let result = serializable(&[
        ok(1, vec![a("x", 1), r("y", &[])]),
        ok(2, vec![r("x", &[1]), a("y", 2)]),
        ok(3, vec![a("x", 3), r("x", &[1, 3])]),
        ok(4, vec![r("x", &[1, 3]), r("y", &[2])]),
    ]);
    assert!(result.valid, "{:?}", result.counts);
    assert!(result.counts.is_empty());
    assert_eq!(
        result.edges,
        BTreeMap::from([("ww", 1), ("wr", 4), ("rw", 2)])
    );
}

#[test]
fn concurrent_but_serializable_reads() {
    assert!(
        serializable(&[
            ok(1, vec![a("x", 1)]),
            ok(2, vec![r("x", &[])]),
            ok(3, vec![r("x", &[1])])
        ])
        .valid
    );
}

#[test]
fn aborted_and_unobserved_indeterminate_writes_are_ignored() {
    let result = serializable(&[
        ok(1, vec![a("x", 1)]),
        other(2, "fail", vec![a("x", 2)]),
        other(3, "info", vec![a("x", 3), unknown("y")]),
        ok(4, vec![r("x", &[1])]),
    ]);
    assert!(result.valid, "{:?}", result.counts);
}

fn only(result: &Checked, kinds: &[&str]) {
    assert_eq!(types(result), kinds, "{:?}", result.anomalies);
    assert!(!result.valid);
}

#[test]
fn g0_write_cycle() {
    let result = serializable(&[
        ok(1, vec![a("x", 1), a("y", 1)]),
        ok(2, vec![a("x", 2), a("y", 2)]),
        ok(3, vec![r("x", &[1, 2]), r("y", &[2, 1])]),
    ]);
    assert!(result.counts.contains_key("G0"));
    let cycle = &result.anomalies["G0"][0];
    let mut members: Vec<i64> = cycle["cycle"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_i64().unwrap())
        .collect();
    members.sort();
    assert_eq!(members, [1, 2]);
    assert!(cycle["steps"]
        .as_array()
        .unwrap()
        .iter()
        .all(|s| s["type"] == "ww"));
}

#[test]
fn g1c_circular_information_flow() {
    only(
        &serializable(&[
            ok(1, vec![a("x", 1), r("y", &[2])]),
            ok(2, vec![a("y", 2), r("x", &[1])]),
        ]),
        &["G1c"],
    );
}

#[test]
fn g_single_read_skew() {
    let result = serializable(&[
        ok(1, vec![r("x", &[]), r("y", &[1])]),
        ok(2, vec![a("x", 1), a("y", 1)]),
        ok(3, vec![r("x", &[1])]),
    ]);
    only(&result, &["G-single"]);
    let explanation = result.anomalies["G-single"][0]["explanation"]
        .as_str()
        .unwrap();
    assert!(explanation.contains("did not observe"));
    assert!(explanation.contains("a contradiction!"));
}

#[test]
fn g2_item_write_skew() {
    let history = [
        ok(1, vec![r("x", &[]), a("y", 1)]),
        ok(2, vec![r("y", &[]), a("x", 1)]),
        ok(3, vec![r("x", &[1]), r("y", &[1])]),
    ];
    let result = serializable(&history);
    only(&result, &["G2-item"]);
    let steps: Vec<&str> = result.anomalies["G2-item"][0]["steps"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["type"].as_str().unwrap())
        .collect();
    assert_eq!(steps, ["rw", "rw"]);
    assert!(check(&history, "snapshot-isolation").unwrap().valid);
}

#[test]
fn g2_item_from_unobserved_appends() {
    only(
        &serializable(&[
            ok(1, vec![r("x", &[]), a("y", 1)]),
            ok(2, vec![r("y", &[]), a("x", 1)]),
        ]),
        &["G2-item"],
    );
}

#[test]
fn read_only_anomaly_postgres_shape() {
    let result = serializable(&[
        ok(1, vec![ri(1670, &[]), ai(1671, 1)]),
        ok(2, vec![ai(1670, 1)]),
        ok(3, vec![ri(1670, &[1]), ri(1671, &[])]),
        ok(4, vec![ri(1670, &[1]), ri(1671, &[1])]),
    ]);
    only(&result, &["G2-item"]);
    let cycle = &result.anomalies["G2-item"][0];
    let mut members: Vec<i64> = cycle["cycle"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_i64().unwrap())
        .collect();
    members.sort();
    assert_eq!(members, [1, 2, 3]);
    let mut kinds: Vec<&str> = cycle["steps"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["type"].as_str().unwrap())
        .collect();
    kinds.sort();
    assert_eq!(kinds, ["rw", "rw", "wr"]);
}

#[test]
fn observed_indeterminate_write_counts_as_committed() {
    let result = serializable(&[
        ok(1, vec![r("x", &[]), a("y", 1)]),
        other(2, "info", vec![unknown("y"), a("x", 1)]),
        ok(3, vec![r("x", &[1]), r("y", &[1])]),
    ]);
    assert!(result.valid, "{:?}", result.counts);
    assert_eq!(result.edges, BTreeMap::from([("rw", 1), ("wr", 2)]));
}

#[test]
fn indeterminate_write_closes_a_cycle_when_observed() {
    let result = serializable(&[
        ok(1, vec![a("x", 1), r("y", &[2])]),
        other(2, "info", vec![a("y", 2), a("z", 2)]),
        ok(3, vec![r("x", &[1]), r("z", &[])]),
        ok(4, vec![a("z", 4)]),
        ok(5, vec![r("z", &[4])]),
    ]);
    assert!(
        result.counts.contains_key("G-single"),
        "{:?}",
        result.counts
    );
}

#[test]
fn g1a_aborted_read() {
    let result = serializable(&[other(1, "fail", vec![a("x", 1)]), ok(2, vec![r("x", &[1])])]);
    assert_eq!(types(&result), ["G1a"]);
    assert!(!result.valid);
}

#[test]
fn g1b_intermediate_read() {
    let result = serializable(&[ok(1, vec![a("x", 1), a("x", 2)]), ok(2, vec![r("x", &[1])])]);
    assert!(result.counts.contains_key("G1b"));
}

#[test]
fn own_intermediate_read_is_fine() {
    let result = serializable(&[
        ok(1, vec![a("x", 1), r("x", &[1]), a("x", 2)]),
        ok(2, vec![r("x", &[1, 2])]),
    ]);
    assert!(result.valid, "{:?}", result.counts);
}

#[test]
fn internal_inconsistency() {
    let result = serializable(&[ok(1, vec![r("x", &[]), a("x", 1), r("x", &[])])]);
    assert_eq!(types(&result), ["internal"]);
    let result = serializable(&[ok(1, vec![a("x", 1), r("x", &[5])])]);
    assert!(result.counts.contains_key("internal"));
}

#[test]
fn incompatible_order() {
    let result = serializable(&[
        ok(1, vec![a("x", 1)]),
        ok(2, vec![a("x", 2)]),
        ok(3, vec![r("x", &[1, 2])]),
        ok(4, vec![r("x", &[2, 1])]),
    ]);
    assert!(result.counts.contains_key("incompatible-order"));
}

#[test]
fn duplicates_and_garbage() {
    let result = serializable(&[ok(1, vec![a("x", 1)]), ok(2, vec![r("x", &[1, 1, 9])])]);
    assert!(result.counts.contains_key("duplicate-elements"));
    assert!(result.counts.contains_key("garbage"));
}

#[test]
fn duplicate_append_is_a_generator_error() {
    assert!(check(
        &[ok(1, vec![a("x", 1)]), ok(2, vec![a("x", 1)])],
        "serializable"
    )
    .is_err());
}

#[test]
fn explanations_name_transactions_in_cycle_order() {
    let result = serializable(&[
        ok(1, vec![r("x", &[]), a("y", 1)]),
        ok(2, vec![r("y", &[]), a("x", 1)]),
    ]);
    assert_eq!(
        result.anomalies["G2-item"][0]["explanation"],
        "Let:\n  T1 = {id 1, process None, ok, value [['r', 'x', []], ['append', 'y', 1]]}\n  \
         T2 = {id 2, process None, ok, value [['r', 'y', []], ['append', 'x', 1]]}\nThen:\n  \
         - T1 < T2, because T1 did not observe T2's append of 1 to x.\n  \
         - However, T2 < T1, because T2 did not observe T1's append of 1 to y: a contradiction!"
    );
}

#[test]
fn identifiers_sort_by_python_repr() {
    // "10" sorts before "9", and a quoted name before either.
    let mut ids = [Atom::Int(9), Atom::Int(10), Atom::from("final-read")];
    ids.sort_by_key(Atom::repr);
    assert_eq!(ids, [Atom::from("final-read"), Atom::Int(10), Atom::Int(9)]);
}

/// An in-memory list-append database: `serial` runs each transaction alone;
/// otherwise transactions interleave with snapshot reads and
/// first-committer-wins.
fn simulate(seed: u64, serial: bool) -> Vec<TxnOp> {
    let (clients, txns, keys) = (6, 300, 4);
    let mut rng = Rng::new(seed);
    let mut db: BTreeMap<i64, Vec<i64>> = BTreeMap::new();
    let mut version: BTreeMap<i64, u64> = BTreeMap::new();
    struct Active {
        ops: Vec<Mop>,
        position: usize,
        snapshot: BTreeMap<i64, Vec<i64>>,
        writes: BTreeMap<i64, Vec<i64>>,
        start: BTreeMap<i64, u64>,
    }
    let mut active: BTreeMap<u64, Active> = BTreeMap::new();
    let (mut out, mut counter, mut done, mut clock) = (Vec::new(), 0, 0, 0i64);
    while done < txns || !active.is_empty() {
        let client = rng.below(clients);
        #[allow(clippy::map_entry)]
        if !active.contains_key(&client) {
            if done >= txns {
                continue;
            }
            done += 1;
            let mut ops = Vec::new();
            for _ in 0..rng.range(1, 4) {
                let k = rng.below(keys) as i64;
                if rng.chance(0.5) {
                    ops.push(Mop::Read(Atom::Int(k), None));
                } else {
                    counter += 1;
                    ops.push(Mop::Append(Atom::Int(k), counter));
                }
            }
            active.insert(
                client,
                Active {
                    ops,
                    position: 0,
                    snapshot: db.clone(),
                    writes: BTreeMap::new(),
                    start: version.clone(),
                },
            );
        }
        let state = active.get_mut(&client).unwrap();
        let steps = if serial { state.ops.len() } else { 1 };
        for _ in 0..steps {
            if state.position < state.ops.len() {
                match state.ops[state.position].clone() {
                    Mop::Read(Atom::Int(k), _) => {
                        let mut seen = state.snapshot.get(&k).cloned().unwrap_or_default();
                        seen.extend(state.writes.get(&k).cloned().unwrap_or_default());
                        state.ops[state.position] = Mop::Read(Atom::Int(k), Some(seen));
                    }
                    Mop::Append(Atom::Int(k), v) => state.writes.entry(k).or_default().push(v),
                    _ => unreachable!(),
                }
                state.position += 1;
            }
        }
        if state.position < state.ops.len() || (!serial && rng.chance(0.5)) {
            continue;
        }
        let state = active.remove(&client).unwrap();
        clock += 1;
        if state
            .writes
            .keys()
            .any(|k| version.get(k) != state.start.get(k))
        {
            out.push(other(clock, "fail", state.ops));
            continue;
        }
        for (k, vs) in &state.writes {
            db.entry(*k).or_default().extend(vs);
            *version.entry(*k).or_default() += 1;
        }
        out.push(ok(clock, state.ops));
    }
    let last = db
        .iter()
        .map(|(k, v)| Mop::Read(Atom::Int(*k), Some(v.clone())))
        .collect();
    out.push(ok(clock + 1, last));
    out
}

#[test]
fn serial_executor_is_always_valid() {
    for seed in 0..20 {
        let result = serializable(&simulate(seed, true));
        assert!(result.valid, "seed {seed}: {:?}", result.counts);
    }
}

#[test]
fn snapshot_executor_shows_only_g2_item() {
    let mut g2 = 0;
    for seed in 0..20 {
        let result = check(&simulate(seed, false), "snapshot-isolation").unwrap();
        assert!(result.valid, "seed {seed}: {:?}", result.counts);
        g2 += result.counts.get("G2-item").copied().unwrap_or(0);
    }
    assert!(g2 > 0);
}

#[test]
fn h_lines_round_trip() {
    let lines: Vec<Value> = [
        json!({"i": 0, "p": 0, "type": "ok", "t0": 0, "t1": 1, "value": [["r", 1, []], ["append", 2, 1]], "error": null}),
        json!({"i": 1, "p": 1, "type": "ok", "t0": 0, "t1": 1, "value": [["r", 2, []], ["append", 1, 1]], "error": null}),
        json!({"i": 2, "p": 2, "type": "fail", "t0": 0, "t1": 1, "value": [["append", 1, 2]], "error": "40001"}),
    ]
    .into();
    let ops = from_h_lines(&lines).unwrap();
    assert_eq!(
        ops.iter().map(kind_of).collect::<Vec<_>>(),
        ["ok", "ok", "fail"]
    );
    assert_eq!(
        serializable(&ops).counts,
        BTreeMap::from([("G2-item".to_string(), 1)])
    );
    assert_eq!(
        from_h_lines(&ops.iter().map(h_line).collect::<Vec<_>>()).unwrap(),
        ops
    );
}
