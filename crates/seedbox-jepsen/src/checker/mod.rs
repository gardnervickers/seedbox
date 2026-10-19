//! Checkers over recorded histories. Linearizability checks use the
//! Wing–Gong–Lowe checker (`wgl`) with the models in [`models`]; transaction
//! histories use the Elle-style list-append analysis in [`elle`], whose
//! history type is `wgl`'s, and [`edn`] exports them for Jepsen's Elle.
//! [`views`] checks single-transaction reads (non-repeatable reads, MAV);
//! [`realtime`] finds list-append reads that miss an earlier-acknowledged append;
//! [`ledger`] checks query results against a model of a ledger (TigerBeetle).
pub mod edn;
pub mod elle;
pub mod kafka;
pub mod kraft;
pub mod ledger;
pub mod lockset;
pub mod models;
pub mod queue;
pub mod realtime;
pub mod views;

/// How many of `total` per-key histories are linearizable, then what the
/// rest were: "5 of 5 registers linearizable", or "3 of 5 keys linearizable
/// (1 not linearizable, 1 undecided: search budget exhausted)".
pub fn linearizable_summary(
    noun: &str,
    total: usize,
    violations: usize,
    undecided: usize,
) -> String {
    let ok = total.saturating_sub(violations + undecided);
    let mut rest = Vec::new();
    if violations > 0 {
        rest.push(format!("{violations} not linearizable"));
    }
    if undecided > 0 {
        rest.push(format!("{undecided} undecided: search budget exhausted"));
    }
    let mut text = format!("{ok} of {total} {noun} linearizable");
    if !rest.is_empty() {
        text.push_str(&format!(" ({})", rest.join(", ")));
    }
    text
}

/// Whether the final reads agree: "all 5 registers agree across nodes", or
/// "2 of 5 registers divergent across nodes".
pub fn agreement_summary(noun: &str, total: usize, divergent: usize, across: &str) -> String {
    if divergent == 0 {
        format!("all {total} {noun} agree across {across}")
    } else {
        format!("{divergent} of {total} {noun} divergent across {across}")
    }
}

/// Reads that returned bytes no client wrote, said positively when none did.
pub fn unwritten_summary(count: usize) -> String {
    if count == 0 {
        "every read returned bytes some client wrote".into()
    } else {
        format!("{count} reads returned bytes no client wrote")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summaries_count_what_held() {
        assert_eq!(
            linearizable_summary("registers", 5, 0, 0),
            "5 of 5 registers linearizable"
        );
        assert_eq!(
            linearizable_summary("keys", 5, 1, 1),
            "3 of 5 keys linearizable (1 not linearizable, 1 undecided: search budget exhausted)"
        );
        assert_eq!(
            agreement_summary("registers", 5, 0, "nodes"),
            "all 5 registers agree across nodes"
        );
        assert_eq!(
            agreement_summary("keys", 5, 2, "replicas"),
            "2 of 5 keys divergent across replicas"
        );
        assert_eq!(
            unwritten_summary(0),
            "every read returned bytes some client wrote"
        );
        assert_eq!(
            unwritten_summary(2),
            "2 reads returned bytes no client wrote"
        );
    }
}
