//! Version-one language-neutral guest control schema. No transport or scheduler.
pub const PROCESS_EFFECT_TIMEOUT_NS: u64 = 5_000_000_000;

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
    Pause {
        target: Target,
    },
    Resume {
        target: Target,
    },
    Crash {
        target: Target,
    },
    Reboot {
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
    /// Freeze the machine's CPU and devices; the world goes on.
    Pause {
        machine: String,
    },
    /// Continue a paused machine, catching up its timers and held ingress.
    Resume {
        machine: String,
    },
    /// Destroy the machine's VM: RAM, devices, queued I/O and sockets.
    Crash {
        machine: String,
    },
    /// Boot a crashed machine again as a new incarnation.
    Reboot {
        machine: String,
    },
}
impl Action {
    pub fn name(&self) -> &'static str {
        match self {
            Self::Stop { .. } => "stop",
            Self::Restart { .. } => "restart",
            Self::Partition { .. } => "partition",
            Self::Heal { .. } => "heal",
            Self::Pause { .. } => "pause",
            Self::Resume { .. } => "resume",
            Self::Crash { .. } => "crash",
            Self::Reboot { .. } => "reboot",
        }
    }
    pub fn machine(&self) -> Option<&str> {
        match self {
            Self::Stop { machine }
            | Self::Restart { machine }
            | Self::Pause { machine }
            | Self::Resume { machine }
            | Self::Crash { machine }
            | Self::Reboot { machine } => Some(machine),
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
        for op in ["pause", "resume", "crash", "reboot"] {
            for target in [Target::Service("s".into()), Target::Machine("m".into())] {
                let wire = serde_json::json!({"version":1,"id":1,"op":op,"target":target});
                let request: Request = serde_json::from_value(wire.clone()).unwrap();
                assert_eq!(serde_json::to_value(request).unwrap(), wire);
            }
            for arguments in [
                serde_json::json!({}),
                serde_json::json!({"target":"s"}),
                serde_json::json!({"target":{"service":"s","machine":"m"}}),
                serde_json::json!({"target":{"service":"s"},"extra":1}),
            ] {
                let mut wire = serde_json::json!({"version":1,"id":1,"op":op});
                wire.as_object_mut()
                    .unwrap()
                    .extend(arguments.as_object().unwrap().clone());
                assert!(serde_json::from_value::<Request>(wire).is_err());
            }
        }
    }
}
