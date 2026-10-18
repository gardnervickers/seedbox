//! Export a list-append history as a Jepsen/Elle EDN history, one op per
//! line, so Jepsen's Elle (elle-cli) can check the same history offline.
//!
//! Each transaction becomes an `:invoke` op at its invocation (reads `nil`)
//! and a completion op, sorted by time with `:index` in that order and
//! `:time` in nanoseconds:
//!
//! ```text
//! {:index 0, :time 2265724000, :type :invoke, :process 1, :f :txn, :value [[:append 1 1] [:r 0 nil]]}
//! {:index 5, :time 2303639000, :type :ok, :process 1, :f :txn, :value [[:append 1 1] [:r 0 []]]}
//! ```
//!
//! `fail` and `info` completions carry the invocation's micro-operations, as
//! Jepsen's do. A non-integer process (the final read) gets the next free
//! integer. Keys are integers.
use super::elle::{kind_of, Atom, Mop, TxnOp};
use std::collections::HashMap;
use wgl::OpOutcome;

fn key(atom: &Atom) -> String {
    match atom {
        Atom::Int(n) => n.to_string(),
        Atom::Str(s) => format!(":{s}"),
    }
}

fn micro(mops: &[Mop], with_reads: bool) -> String {
    let parts: Vec<String> = mops
        .iter()
        .map(|mop| match mop {
            Mop::Append(k, v) => format!("[:append {} {v}]", key(k)),
            Mop::Read(k, Some(values)) if with_reads => format!(
                "[:r {} [{}]]",
                key(k),
                values
                    .iter()
                    .map(i64::to_string)
                    .collect::<Vec<_>>()
                    .join(" ")
            ),
            Mop::Read(k, _) => format!("[:r {} nil]", key(k)),
        })
        .collect();
    format!("[{}]", parts.join(" "))
}

/// The EDN lines for a history.
pub fn lines(ops: &[TxnOp]) -> Vec<String> {
    let spare = ops
        .iter()
        .filter_map(|op| match &op.op.process {
            Some(Atom::Int(p)) => Some(*p),
            _ => None,
        })
        .max()
        .unwrap_or(-1)
        + 1;
    let mut sorted: Vec<&TxnOp> = ops.iter().collect();
    let order = |op: &TxnOp| match &op.op.id {
        Atom::Int(n) => *n,
        Atom::Str(_) => i64::MAX,
    };
    sorted.sort_by(|a, b| {
        a.invoked_at
            .cmp(&b.invoked_at)
            .then(order(a).cmp(&order(b)))
    });
    let mut sequence: HashMap<i64, u64> = HashMap::new();
    // (time, per-process sequence, id, kind, process, value)
    let mut events: Vec<(u64, u64, i64, &str, i64, String)> = Vec::new();
    for op in sorted {
        let process = match &op.op.process {
            Some(Atom::Int(p)) => *p,
            _ => spare,
        };
        // Ties in time keep each process's own order: its invocation, then its completion.
        let n = *sequence.get(&process).unwrap_or(&0);
        sequence.insert(process, n + 2);
        let id = order(op);
        events.push((
            op.invoked_at,
            n,
            id,
            "invoke",
            process,
            micro(&op.op.mops, false),
        ));
        let (at, value, reads) = match &op.outcome {
            OpOutcome::Completed { at, ret } => (*at, ret.as_slice(), true),
            OpOutcome::Failed { at } => (*at, op.op.mops.as_slice(), false),
            OpOutcome::Pending => (
                op.op.info_at.unwrap_or(op.invoked_at),
                op.op.mops.as_slice(),
                false,
            ),
        };
        events.push((at, n + 1, id, kind_of(op), process, micro(value, reads)));
    }
    events.sort();
    events
        .into_iter()
        .enumerate()
        .map(|(index, (time, _, _, kind, process, value))| {
            format!("{{:index {index}, :time {time}, :type :{kind}, :process {process}, :f :txn, :value {value}}}")
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::super::elle::{from_h_lines, Mop};
    use super::*;
    use serde_json::json;

    #[test]
    fn invocations_completions_and_ties() {
        let ops = from_h_lines(&[
            json!({"i": 0, "p": 3, "type": "ok", "t0": 1.0, "t1": 2.0, "value": [["append", 1, 1], ["r", 2, [4, 5]]]}),
            json!({"i": 1, "p": 3, "type": "fail", "t0": 2.0, "t1": 2.0, "value": [["r", 1, null], ["append", 2, 6]]}),
            json!({"i": 2, "p": 0, "type": "info", "t0": 1.5, "t1": 9.0, "value": [["r", 7, null]]}),
            json!({"i": 3, "p": "final", "type": "ok", "t0": 10.0, "t1": 10.0, "value": [["r", 1, []]]}),
        ])
        .unwrap();
        assert!(
            matches!(ops[0].outcome, OpOutcome::Completed { ref ret, .. } if ret[1] == Mop::Read(Atom::Int(2), Some(vec![4, 5])))
        );
        assert_eq!(
            lines(&ops),
            [
                "{:index 0, :time 1000000000, :type :invoke, :process 3, :f :txn, :value [[:append 1 1] [:r 2 nil]]}",
                "{:index 1, :time 1500000000, :type :invoke, :process 0, :f :txn, :value [[:r 7 nil]]}",
                "{:index 2, :time 2000000000, :type :ok, :process 3, :f :txn, :value [[:append 1 1] [:r 2 [4 5]]]}",
                "{:index 3, :time 2000000000, :type :invoke, :process 3, :f :txn, :value [[:r 1 nil] [:append 2 6]]}",
                "{:index 4, :time 2000000000, :type :fail, :process 3, :f :txn, :value [[:r 1 nil] [:append 2 6]]}",
                "{:index 5, :time 9000000000, :type :info, :process 0, :f :txn, :value [[:r 7 nil]]}",
                "{:index 6, :time 10000000000, :type :invoke, :process 4, :f :txn, :value [[:r 1 nil]]}",
                "{:index 7, :time 10000000000, :type :ok, :process 4, :f :txn, :value [[:r 1 []]]}",
            ]
        );
    }
}
