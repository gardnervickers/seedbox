use crate::{ExecutionTermination, Phase, SimulationId, Stage, StageTiming, TestVerdict};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ApiVersion {
    #[serde(rename = "seedbox/v1")]
    V1,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ResourceKind {
    Simulation,
}

/// The resubmission view deliberately ignores server-owned fields. Unlike
/// deserializing a resource, it accepts old or extended status representations
/// while still validating the API version, resource kind and entire spec.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SimulationDocument {
    pub api_version: ApiVersion,
    pub kind: ResourceKind,
    pub spec: SimulationSpec,
    #[serde(default)]
    pub metadata: serde::de::IgnoredAny,
    #[serde(default)]
    pub status: serde::de::IgnoredAny,
}

/// Authoring configuration is resolved by the client against its environment
/// before submission. Prepared resources instead refer to an immutable input
/// closure owned by the server, and never read the original project again.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase", deny_unknown_fields)]
pub enum InputSource {
    Project,
    Retained {
        simulation: SimulationId,
    },
    /// Reuse preparation inputs for a fresh timeline without interactive history.
    Prepared {
        simulation: SimulationId,
    },
    Checkpoint {
        simulation: SimulationId,
        checkpoint: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SimulationSpec {
    /// None asks the server to choose once, before returning an accepted resource.
    pub seed: Option<u64>,
    pub inputs: InputSource,
    /// Resolved experiment, including the designated workload oracle. Retained
    /// submissions must match the configuration owned by their input reference.
    pub configuration: seedbox_spec::Simulation,
}

/// Request identity is scoped to this catalogue and retained for as long as its
/// simulation exists. Reusing it with a different spec is a conflict.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SubmitRequest {
    pub request_id: String,
    pub spec: SimulationSpec,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Metadata {
    pub id: SimulationId,
    /// Durable resource revision. Live elapsed-time projections do not advance it.
    pub revision: u64,
    /// Host Unix time for display; never an input to guest execution.
    pub created_at_unix_ms: u64,
    /// Set once when cascading deletion is accepted; absent otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deletion_requested_at_unix_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent: Option<SimulationId>,
    /// Checkpoint in the parent simulation from which this timeline began.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checkpoint: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SimulationStatus {
    pub phase: Phase,
    pub stage: Option<Stage>,
    pub simulated_time_ns: u64,
    pub playback: Option<crate::PlaybackObservation>,
    pub stage_timings: BTreeMap<Stage, StageTiming>,
    pub oracle_exit_code: Option<u8>,
    pub termination: Option<ExecutionTermination>,
    pub stop_requested: bool,
}

// Keep the derived verdict at the wire boundary, never in mutable status. The
// same view accepts older resources without it and ignores supplied verdicts.
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct StatusDocument {
    phase: Phase,
    stage: Option<Stage>,
    simulated_time_ns: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    playback: Option<crate::PlaybackObservation>,
    stage_timings: BTreeMap<Stage, StageTiming>,
    oracle_exit_code: Option<u8>,
    termination: Option<ExecutionTermination>,
    stop_requested: bool,
    #[serde(default, deserialize_with = "ignore_verdict")]
    verdict: Option<TestVerdict>,
}

fn ignore_verdict<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<TestVerdict>, D::Error> {
    serde::de::IgnoredAny::deserialize(deserializer)?;
    Ok(None)
}

impl Serialize for SimulationStatus {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        StatusDocument {
            phase: self.phase,
            stage: self.stage,
            simulated_time_ns: self.simulated_time_ns,
            playback: self.playback.clone(),
            stage_timings: self.stage_timings.clone(),
            oracle_exit_code: self.oracle_exit_code,
            termination: self.termination.clone(),
            stop_requested: self.stop_requested,
            verdict: Some(self.verdict()),
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for SimulationStatus {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let document = StatusDocument::deserialize(deserializer)?;
        Ok(Self {
            phase: document.phase,
            stage: document.stage,
            simulated_time_ns: document.simulated_time_ns,
            playback: document.playback,
            stage_timings: document.stage_timings,
            oracle_exit_code: document.oracle_exit_code,
            termination: document.termination,
            stop_requested: document.stop_requested,
        })
    }
}

impl Default for SimulationStatus {
    fn default() -> Self {
        Self {
            phase: Phase::Queued,
            stage: None,
            simulated_time_ns: 0,
            playback: None,
            stage_timings: BTreeMap::new(),
            oracle_exit_code: None,
            termination: None,
            stop_requested: false,
        }
    }
}

impl SimulationStatus {
    /// Derived rather than independently writable, so diagnostics cannot
    /// accidentally replace an observed oracle verdict.
    pub fn verdict(&self) -> TestVerdict {
        TestVerdict::from_oracle(self.oracle_exit_code, self.phase)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Simulation {
    pub api_version: ApiVersion,
    pub kind: ResourceKind,
    pub metadata: Metadata,
    pub spec: SimulationSpec,
    pub status: SimulationStatus,
}

impl Simulation {
    pub fn queued(metadata: Metadata, spec: SimulationSpec) -> Self {
        Self {
            api_version: ApiVersion::V1,
            kind: ResourceKind::Simulation,
            metadata,
            spec,
            status: SimulationStatus::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resubmission_ignores_server_fields_but_rejects_unknown_spec_fields() {
        let spec = serde_json::json!({
            "seed": 123,
            "inputs": {"kind": "project"},
            "configuration": {
                "name": "test",
                "source": "/project/compose.yaml",
                "services": {},
                "configs": {},
                "network": {"name": "default", "latency_ns": 1000000, "jitter_ns": 0},
                "schedule": {"timer_jitter_ns": 0, "stop_tolerance_ns": 0},
                "limits": {"simulated_time_ns": 60000000000_u64, "wall_time_ns": 600000000000_u64},
                "check": {"service": "oracle", "exit_code": 0},
                "seed": null
            }
        });
        let expected: SimulationSpec = serde_json::from_value(spec.clone()).unwrap();
        let mut document = serde_json::json!({
            "apiVersion": "seedbox/v1",
            "kind": "Simulation",
            "spec": spec,
            "status": {"futureField": true},
            "metadata": {"id": "deliberately-invalid-old-id"}
        });
        let resubmit: SimulationDocument = serde_json::from_value(document.clone()).unwrap();
        assert_eq!(resubmit.spec, expected);
        document["spec"]["typo"] = serde_json::json!(true);
        assert!(serde_json::from_value::<SimulationDocument>(document).is_err());
    }

    #[test]
    fn status_serializes_derived_verdict_and_round_trips() {
        for (phase, code, verdict) in [
            (Phase::Running, None, "pending"),
            (Phase::Completed, None, "incomplete"),
            (Phase::Running, Some(0), "passed"),
            (Phase::Completed, Some(23), "failed"),
        ] {
            let status = SimulationStatus {
                phase,
                oracle_exit_code: code,
                termination: Some(ExecutionTermination::Error {
                    message: "cleanup failed".into(),
                }),
                ..Default::default()
            };
            let document = serde_json::to_value(&status).unwrap();
            assert_eq!(document["verdict"], verdict);
            assert!(!document.as_object().unwrap().contains_key("playback"));
            assert!(document["stage"].is_null());
            assert_eq!(
                serde_json::from_value::<SimulationStatus>(document).unwrap(),
                status
            );
        }
    }

    #[test]
    fn input_verdict_cannot_override_oracle_and_old_documents_remain_readable() {
        let status = SimulationStatus {
            phase: Phase::Completed,
            oracle_exit_code: Some(5),
            ..Default::default()
        };
        let mut document = serde_json::to_value(&status).unwrap();
        document["verdict"] = serde_json::json!("passed");
        let decoded: SimulationStatus = serde_json::from_value(document.clone()).unwrap();
        assert_eq!(decoded.verdict(), TestVerdict::Failed);
        assert_eq!(serde_json::to_value(decoded).unwrap()["verdict"], "failed");
        document.as_object_mut().unwrap().remove("verdict");
        assert_eq!(
            serde_json::from_value::<SimulationStatus>(document.clone()).unwrap(),
            status
        );
        document["unknown"] = serde_json::json!(true);
        assert!(serde_json::from_value::<SimulationStatus>(document).is_err());
    }
}
