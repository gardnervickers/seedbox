//! Real-time stale reads in list-append histories.
//!
//! A committed transaction W appends `v` to key `k` and is acknowledged at
//! time `a`; a committed transaction R that begins after `a` then reads `k`
//! without `v`. Under strict serializability (and any model with real-time
//! order, such as strong snapshot isolation) R must follow W, so R missed a
//! write that had already committed. In Elle's terms this is a cycle of one
//! real-time edge W -> R and one anti-dependency R -> W, `G-single-realtime`;
//! Jepsen's Galera analysis reports these as stale reads (MDEV-38999). The
//! check needs no dependency graph: a read is stale when it lacks any value
//! that an earlier-acknowledged transaction appended.
//!
//! One item per stale read (reader and key), with the missing values, the
//! writer acknowledged earliest among them, and the lag between that
//! acknowledgement and the reader's invocation.
use super::elle::{kind_of, Atom, Mop, TxnOp};
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashSet};
use wgl::OpOutcome;

/// Stale reads in `ops`, ordered by reader then key; `reader` selects
/// which committed transactions count as readers (all of them, say, or
/// every one but a final read).
pub fn stale_reads(ops: &[TxnOp], reader: impl Fn(&TxnOp) -> bool) -> Vec<Value> {
    // Per key, every acknowledged append: (acknowledged at, value, writer).
    let mut acked: BTreeMap<&Atom, Vec<(u64, i64, usize)>> = BTreeMap::new();
    for (index, op) in ops.iter().enumerate() {
        if let OpOutcome::Completed { at, ret } = &op.outcome {
            for mop in ret {
                if let Mop::Append(k, v) = mop {
                    acked.entry(k).or_default().push((*at, *v, index));
                }
            }
        }
    }
    for appends in acked.values_mut() {
        appends.sort();
    }
    let mut out = Vec::new();
    for (index, op) in ops.iter().enumerate() {
        if kind_of(op) != "ok" || !reader(op) {
            continue;
        }
        let OpOutcome::Completed { ret, .. } = &op.outcome else {
            continue;
        };
        let mut seen_keys: HashSet<&Atom> = HashSet::new();
        for mop in ret {
            let Mop::Read(k, Some(list)) = mop else {
                continue;
            };
            if !seen_keys.insert(k) {
                continue;
            }
            let Some(appends) = acked.get(k) else {
                continue;
            };
            let present: HashSet<i64> = list.iter().copied().collect();
            let missing: Vec<&(u64, i64, usize)> = appends
                .iter()
                .take_while(|(at, _, _)| *at < op.invoked_at)
                .filter(|(_, v, writer)| *writer != index && !present.contains(v))
                .collect();
            if let Some(&&(at, value, writer)) = missing.first() {
                out.push(json!({
                    "reader": op.op.id.to_json(),
                    "reader-process": op.op.process.as_ref().map(Atom::to_json),
                    "key": k.to_json(),
                    "read": list,
                    "missing": missing.iter().map(|m| m.1).collect::<Vec<_>>(),
                    "writer": ops[writer].op.id.to_json(),
                    "writer-process": ops[writer].op.process.as_ref().map(Atom::to_json),
                    "value": value,
                    "acked-at": at as f64 / 1e9,
                    "read-invoked-at": op.invoked_at as f64 / 1e9,
                    "lag": (op.invoked_at - at) as f64 / 1e9,
                }));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::checker::elle;

    fn ok(id: i64, value: Vec<Mop>, t0: u64, t1: u64) -> TxnOp {
        elle::txn(id, "ok", value, Some(Atom::Int(id)), t0, t1, None).unwrap()
    }

    #[test]
    fn a_read_after_an_acknowledged_append_must_see_it() {
        let ops = [
            ok(0, vec![Mop::Append(1.into(), 1)], 0, 10),
            // Concurrent with the append: may miss it.
            ok(1, vec![Mop::Read(1.into(), Some(vec![]))], 5, 20),
            // Begins after the acknowledgement and misses it: stale.
            ok(2, vec![Mop::Read(1.into(), Some(vec![]))], 15, 30),
            // Sees it.
            ok(3, vec![Mop::Read(1.into(), Some(vec![1]))], 15, 30),
            ok(4, vec![Mop::Append(1.into(), 2)], 31, 40),
            // Misses 2 only, reading a key twice counts once.
            ok(
                5,
                vec![
                    Mop::Read(1.into(), Some(vec![1])),
                    Mop::Read(1.into(), Some(vec![1])),
                ],
                50,
                60,
            ),
            elle::txn(6, "info", vec![Mop::Append(1.into(), 3)], None, 0, 1, None).unwrap(),
        ];
        let stale = stale_reads(&ops, |_| true);
        let found: Vec<(Value, Value)> = stale
            .iter()
            .map(|s| (s["reader"].clone(), s["missing"].clone()))
            .collect();
        assert_eq!(found, [(json!(2), json!([1])), (json!(5), json!([2]))]);
        assert_eq!(stale[1]["writer"], 4);
        assert_eq!(stale_reads(&ops, |op| op.op.id != Atom::Int(2)).len(), 1);
    }
}
