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
/// A scheduled fault. Targets are services; the engine maps them to machines
/// through the retained placement, so several services may share a machine.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum Action {
    Stop {
        service: String,
    },
    Restart {
        service: String,
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
    pub fn service(&self) -> Option<&str> {
        match self {
            Self::Stop { service } | Self::Restart { service } => Some(service),
            _ => None,
        }
    }
}
/// Resolve a request target to the service it names. A machine target names
/// the only service placed on it. The controller is outside the failure model.
pub fn resolve(
    target: &Target,
    placement: &BTreeMap<String, String>,
    controller: Option<&str>,
) -> Result<String, &'static str> {
    let service = match target {
        Target::Service(service) => {
            if !placement.contains_key(service) {
                return Err("unknown service");
            }
            service.clone()
        }
        Target::Machine(machine) => {
            let mut hosted = placement.iter().filter(|(_, m)| *m == machine);
            match (hosted.next(), hosted.next()) {
                (None, _) => return Err("unknown machine"),
                (Some((service, _)), None) => service.clone(),
                (Some(_), Some(_)) => {
                    if placement
                        .iter()
                        .any(|(s, m)| m == machine && Some(s.as_str()) == controller)
                    {
                        return Err("controller is outside failure model");
                    }
                    return Err("machine hosts several services; target a service");
                }
            }
        }
    };
    if Some(service.as_str()) == controller {
        return Err("controller is outside failure model");
    }
    Ok(service)
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
    #[test]
    fn targets_resolve_to_services_and_exclude_the_controller() {
        let shared: BTreeMap<String, String> = ["a", "b", "scenario"]
            .into_iter()
            .map(|s| (s.to_owned(), "kernel".to_owned()))
            .collect();
        let service = |s: &str| Target::Service(s.into());
        let machine = |m: &str| Target::Machine(m.into());
        assert_eq!(resolve(&service("a"), &shared, Some("scenario")), Ok("a".into()));
        assert_eq!(resolve(&service("x"), &shared, Some("scenario")), Err("unknown service"));
        assert_eq!(
            resolve(&service("scenario"), &shared, Some("scenario")),
            Err("controller is outside failure model")
        );
        assert_eq!(
            resolve(&machine("kernel"), &shared, Some("scenario")),
            Err("controller is outside failure model")
        );
        assert_eq!(
            resolve(&machine("kernel"), &shared, None),
            Err("machine hosts several services; target a service")
        );
        let separate: BTreeMap<String, String> = ["a", "scenario"]
            .into_iter()
            .map(|s| (s.to_owned(), format!("{s}-vm")))
            .collect();
        assert_eq!(resolve(&machine("a-vm"), &separate, Some("scenario")), Ok("a".into()));
        assert_eq!(resolve(&machine("x-vm"), &separate, Some("scenario")), Err("unknown machine"));
        assert_eq!(
            resolve(&machine("scenario-vm"), &separate, Some("scenario")),
            Err("controller is outside failure model")
        );
    }
}
