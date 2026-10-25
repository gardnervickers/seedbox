//! Checks over a finished history and the final reads.
use crate::Record;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Jepsen's `:valid?`: true, false or unknown.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Validity {
    Valid,
    Invalid,
    /// The run cannot decide: nothing was acknowledged, a node was never
    /// read, the clients made too little progress to count.
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Verdict {
    pub validity: Validity,
    /// One line, such as `412 acked values present on every node`.
    pub summary: String,
    /// The evidence behind the verdict: counts, examples, ranges. It becomes
    /// the property's details, which keep at most 1024 serialized bytes.
    pub details: Value,
}

impl Verdict {
    /// The condition of the checker's `always` property. Unknown leaves the
    /// property unevaluated, so it reports as unmet, which does not fail a
    /// run by itself.
    pub fn condition(&self) -> Option<bool> {
        match self.validity {
            Validity::Valid => Some(true),
            Validity::Invalid => Some(false),
            Validity::Unknown => None,
        }
    }
}

/// One node's final read: its value, or why no read completed. Serialized
/// with the history, so a saved run can be checked again.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Read<R> {
    pub node: String,
    pub result: Result<R, String>,
}

/// History and final reads in, verdict out. A checker is a pure function of
/// its inputs, so a saved history can be checked again offline.
pub trait Checker<O, V, R> {
    /// The guest property this checker's verdict evaluates.
    fn name(&self) -> &str;
    fn check(&self, history: &[Record<O, V>], reads: &[Read<R>]) -> Verdict;
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn final_reads_round_trip_for_offline_checks() {
        let reads = vec![
            Read {
                node: "n1".to_string(),
                result: Ok(vec![1u64, 2]),
            },
            Read {
                node: "n2".to_string(),
                result: Err("no read".to_string()),
            },
        ];
        let wire = serde_json::to_value(&reads).unwrap();
        assert_eq!(
            wire,
            json!([
                {"node": "n1", "result": {"Ok": [1, 2]}},
                {"node": "n2", "result": {"Err": "no read"}},
            ])
        );
        assert_eq!(
            serde_json::from_value::<Vec<Read<Vec<u64>>>>(wire).unwrap(),
            reads
        );
    }
}
