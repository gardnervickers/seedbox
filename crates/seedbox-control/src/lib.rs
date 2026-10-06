//! Version-one language-neutral guest control schema. No transport or scheduler.
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, num::NonZeroU32};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Target {
    Service(String),
    Machine(String),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum Operation {
    Now {},
    Inspect {},
    Sleep {
        duration_ns: u64,
    },
    Stop {
        target: Target,
    },
    Restart {
        target: Target,
    },
    Partition {
        left: Vec<Target>,
        right: Vec<Target>,
    },
    Heal {
        handle: String,
    },
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Request {
    pub version: u32,
    pub id: NonZeroU32,
    #[serde(flatten)]
    pub operation: Operation,
}
/// Parse identity first: malformed identity is a protocol failure; valid identity
/// with invalid operation arguments can receive a correlated operation error.
#[derive(Deserialize)]
pub struct Header {
    pub version: u32,
    pub id: NonZeroU32,
    #[serde(flatten)]
    pub arguments: BTreeMap<String, serde_json::Value>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum Action {
    Stop {
        machine: String,
    },
    Restart {
        machine: String,
    },
    Partition {
        handle: String,
        left: Vec<String>,
        right: Vec<String>,
    },
    Heal {
        handle: String,
    },
}
impl Action {
    pub fn name(&self) -> &'static str {
        match self {
            Self::Stop { .. } => "stop",
            Self::Restart { .. } => "restart",
            Self::Partition { .. } => "partition",
            Self::Heal { .. } => "heal",
        }
    }
    pub fn machine(&self) -> Option<&str> {
        match self {
            Self::Stop { machine } | Self::Restart { machine } => Some(machine),
            _ => None,
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Plan {
    pub id: u32,
    pub at_ns: u64,
    pub deadline_ns: u64,
    #[serde(flatten)]
    pub action: Action,
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn strict_wire_rejects_unknown_fields_bad_targets_and_duplicate_identity() {
        for bad in [
            r#"{"version":1,"id":1,"op":"now","extra":1}"#,
            r#"{"version":1,"id":1,"op":"stop","target":{"service":"s","machine":"m"}}"#,
            r#"{"version":1,"id":1,"op":"sleep","duration_ns":-1}"#,
            r#"{"version":1,"id":1,"op":"now","id":2}"#,
        ] {
            assert!(
                serde_json::from_str::<Request>(bad).is_err(),
                "accepted {bad}"
            );
        }
        let r:Request=serde_json::from_str(r#"{"version":1,"id":1,"op":"partition","left":[{"service":"c"}],"right":[{"machine":"s"}]}"#).unwrap();
        assert!(matches!(r.operation, Operation::Partition { .. }));
    }
}
