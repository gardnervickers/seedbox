//! Checks over what one committed transaction read, for workloads that are
//! not list-append: Jepsen's MySQL non-repeatable-read and monotonic atomic
//! view (MAV) tests.
//!
//! * [`repeatable`]: every read of the same row in one transaction returns
//!   the same value. A transaction that writes some other column of the row
//!   between two reads still must not see a concurrent writer's change of the
//!   column it reads. Disagreements are `changed` (two values), `appeared`
//!   (absent, then present) or `vanished` (present, then absent).
//! * [`monotonic`]: counters that writers advance together, read in order in
//!   one transaction, never go backwards: once a transaction has observed a
//!   writer's effect on one counter, a later read of another counter must
//!   observe that writer as well. A decrease is a fractured (non-monotonic)
//!   view. Jepsen's MAV check, `(<= a1 b2 a2)`, is this order over three reads.
use serde_json::{json, Value};

/// One committed transaction's reads, in order: (row, value), `Null` when
/// the row was absent.
#[derive(Clone, Debug, PartialEq)]
pub struct Reads<V> {
    pub id: Value,
    pub reads: Vec<(Value, V)>,
}

/// Rows whose reads disagree within a transaction: one item per
/// transaction and row, with every value it read.
pub fn repeatable(txns: &[Reads<Value>]) -> Vec<Value> {
    let mut out = Vec::new();
    for txn in txns {
        let mut rows: Vec<&Value> = Vec::new();
        for (row, _) in &txn.reads {
            if !rows.contains(&row) {
                rows.push(row);
            }
        }
        for row in rows {
            let values: Vec<&Value> = txn
                .reads
                .iter()
                .filter(|(r, _)| r == row)
                .map(|(_, v)| v)
                .collect();
            let Some(pair) = values.windows(2).find(|pair| pair[0] != pair[1]) else {
                continue;
            };
            let kind = match (pair[0].is_null(), pair[1].is_null()) {
                (true, false) => "appeared",
                (false, true) => "vanished",
                _ => "changed",
            };
            out.push(json!({"txn": txn.id, "row": row, "type": kind, "reads": values}));
        }
    }
    out
}

/// Transactions whose ordered counter reads decrease: the first decrease
/// of each.
pub fn monotonic(txns: &[Reads<i64>]) -> Vec<Value> {
    let mut out = Vec::new();
    for txn in txns {
        let mut highest: Option<(usize, i64)> = None;
        for (position, (row, value)) in txn.reads.iter().enumerate() {
            if let Some((at, seen)) = highest {
                if *value < seen {
                    let (seen_row, _) = &txn.reads[at];
                    out.push(json!({
                        "type": "non-monotonic",
                        "txn": txn.id,
                        "reads": txn.reads.iter().map(|(r, v)| json!([r, v])).collect::<Vec<_>>(),
                        "saw": [seen_row, seen],
                        "then": [row, value],
                    }));
                    break;
                }
            }
            if highest.is_none_or(|(_, seen)| *value > seen) {
                highest = Some((position, *value));
            }
        }
    }
    out
}

// Counts of [`repeatable`] or [`monotonic`] items by type.
pub fn kinds(items: &[Value]) -> std::collections::BTreeMap<String, usize> {
    let mut counts = std::collections::BTreeMap::new();
    for item in items {
        let kind = item["type"].as_str().unwrap_or("?").to_string();
        *counts.entry(kind).or_default() += 1;
    }
    counts
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reads<V>(id: i64, reads: Vec<(i64, V)>) -> Reads<V> {
        Reads {
            id: id.into(),
            reads: reads.into_iter().map(|(r, v)| (r.into(), v)).collect(),
        }
    }

    #[test]
    fn repeated_reads_that_agree_are_clean() {
        let txns = [
            reads(1, vec![(0, json!("moss")), (0, json!("moss"))]),
            reads(2, vec![(0, Value::Null), (0, Value::Null)]),
            reads(3, vec![(0, json!("moss")), (1, json!("leaf"))]),
        ];
        assert!(repeatable(&txns).is_empty());
    }

    #[test]
    fn jepsens_examples_are_classified() {
        // "pebble" then "moss" after the transaction's own gender update.
        let changed = reads(1, vec![(0, json!("pebble")), (0, json!("moss"))]);
        // nil, then "moss" after a concurrent re-insert.
        let appeared = reads(2, vec![(0, Value::Null), (0, json!("moss"))]);
        let vanished = reads(3, vec![(0, json!("moss")), (0, Value::Null)]);
        let found = repeatable(&[changed, appeared, vanished]);
        let kinds: Vec<&str> = found.iter().map(|f| f["type"].as_str().unwrap()).collect();
        assert_eq!(kinds, ["changed", "appeared", "vanished"]);
        assert_eq!(found[0]["reads"], json!(["pebble", "moss"]));
        assert_eq!(super::kinds(&found)["changed"], 1);
    }

    #[test]
    fn a_monotonic_view_may_rise_but_not_fall() {
        let txns = [
            reads(1, vec![(0, 0), (1, 0), (0, 0)]),
            reads(2, vec![(0, 3), (1, 4), (0, 4)]),
        ];
        assert!(monotonic(&txns).is_empty());
        // Jepsen's example: row 0 reads 0, row 1 reads 1, then row 0 reads 0.
        let fractured = reads(3, vec![(0, 0), (1, 1), (0, 0)]);
        let found = monotonic(&[fractured]);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0]["saw"], json!([1, 1]));
        assert_eq!(found[0]["then"], json!([0, 0]));
        // Agrees with Jepsen's (<= a1 b2 a2) on the first read too.
        assert_eq!(
            monotonic(&[reads(4, vec![(0, 2), (1, 1), (0, 2)])]).len(),
            1
        );
    }
}
