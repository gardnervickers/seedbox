//! Operation histories in Jepsen's vocabulary.
//!
//! A process invokes an operation, then completes it once: `ok` when it took
//! effect with a value, `fail` when it certainly did not take effect, `info`
//! when the outcome is unknown (a timeout, a lost connection). An `info`
//! operation may still take effect, and a late acknowledgement may complete
//! it again as `ok`; checkers decide what that means. Times are guest
//! monotonic nanoseconds since the history began, which under Seedbox is
//! simulated time.
use serde::{Deserialize, Serialize};

/// How an invocation ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Completion<V> {
    Ok(V),
    Fail(String),
    Info(String),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event<O, V> {
    Invoke {
        op: O,
    },
    /// `invoke` is the index of the invocation this completes.
    Ok {
        invoke: u64,
        value: V,
    },
    Fail {
        invoke: u64,
        error: String,
    },
    Info {
        invoke: u64,
        error: String,
    },
}

/// One history line. Serialized, it reads
/// `{"index":3,"process":1,"time_ns":42,"type":"ok","invoke":2,"value":...}`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Record<O, V> {
    /// Position in the history, from 0.
    pub index: u64,
    pub process: u64,
    pub time_ns: u64,
    #[serde(flatten)]
    pub event: Event<O, V>,
}

/// A shared, append-only history. Client threads record into it
/// concurrently; an implementation assigns indexes and times in the order
/// calls arrive.
pub trait History<O, V>: Send + Sync {
    /// Record that `process` invoked `op`; returns the invocation's index.
    fn invoke(&self, process: u64, op: O) -> u64;
    /// Complete the invocation at index `invoke`, as its process.
    fn complete(&self, invoke: u64, completion: Completion<V>);
    /// Every record so far, by index.
    fn records(&self) -> Vec<Record<O, V>>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn records_serialize_as_flat_jepsen_lines() {
        let records: Vec<Record<String, u64>> = vec![
            Record {
                index: 0,
                process: 1,
                time_ns: 10,
                event: Event::Invoke { op: "0-0".into() },
            },
            Record {
                index: 1,
                process: 1,
                time_ns: 20,
                event: Event::Info {
                    invoke: 0,
                    error: "timeout".into(),
                },
            },
            // A late acknowledgement of the same invocation.
            Record {
                index: 2,
                process: 1,
                time_ns: 30,
                event: Event::Ok {
                    invoke: 0,
                    value: 7,
                },
            },
        ];
        let wire = serde_json::to_value(&records).unwrap();
        assert_eq!(
            wire,
            json!([
                {"index": 0, "process": 1, "time_ns": 10, "type": "invoke", "op": "0-0"},
                {"index": 1, "process": 1, "time_ns": 20, "type": "info", "invoke": 0, "error": "timeout"},
                {"index": 2, "process": 1, "time_ns": 30, "type": "ok", "invoke": 0, "value": 7},
            ])
        );
        assert_eq!(
            serde_json::from_value::<Vec<Record<String, u64>>>(wire).unwrap(),
            records
        );
    }
}
