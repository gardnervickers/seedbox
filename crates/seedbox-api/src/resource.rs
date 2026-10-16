use crate::{ExecutionTermination, Phase, SimulationId, Stage, StageTiming, TestVerdict};
pub use seedbox_engine_protocol::property::Assessment as PropertyAssessment;
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
    #[serde(default)]
    pub options: ExecutionOptions,
    pub api_version: ApiVersion,
    pub kind: ResourceKind,
    pub spec: SimulationSpec,
    #[serde(default, deserialize_with = "document_metadata")]
    pub metadata: seedbox_spec::definition::Metadata,
    #[serde(default)]
    pub status: serde::de::IgnoredAny,
}

fn document_metadata<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<seedbox_spec::definition::Metadata, D::Error> {
    // A resource adds catalogue fields to metadata. Resubmission preserves only
    // descriptive metadata, independent of the old catalogue and status.
    #[derive(Default, Deserialize)]
    #[serde(default)]
    struct DocumentMetadata {
        name: String,
        labels: BTreeMap<String, String>,
        annotations: BTreeMap<String, String>,
    }
    let metadata = DocumentMetadata::deserialize(deserializer)?;
    Ok(seedbox_spec::definition::Metadata {
        name: metadata.name,
        labels: metadata.labels,
        annotations: metadata.annotations,
    })
}

/// Private input representation. Clients resolve local paths; the server resolves
/// architecture, image inputs and semantic defaults before execution. Prepared
/// definitions refer to immutable assets and never read the original project.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase", deny_unknown_fields)]
pub enum InputSource {
    Project,
    Resolved {
        definition: Box<seedbox_spec::definition::Definition>,
    },
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
pub struct ProgramInput {
    pub key: String,
    pub at_ns: u64,
    /// Opaque on reads so newer server actions do not break older clients.
    pub action: serde_json::Value,
    #[serde(default = "seedbox_engine_protocol::input_program::default_timeout_ns")]
    pub timeout_ns: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SimulationSpec {
    /// Internal compatibility field; execution options are submitted separately.
    pub start_paused: bool,
    pub seed: Option<u64>,
    pub inputs: InputSource,
    pub configuration: seedbox_spec::Simulation,
    pub program: Vec<ProgramInput>,
}
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct LegacySpec {
    seed: Option<u64>,
    inputs: InputSource,
    configuration: seedbox_spec::Simulation,
    #[serde(default)]
    program: Vec<ProgramInput>,
}
impl Serialize for SimulationSpec {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        if let InputSource::Resolved { definition } = &self.inputs {
            let mut definition = definition.as_ref().clone();
            definition.seed = self.seed.unwrap_or(definition.seed);
            definition.overrides = self
                .program
                .iter()
                .map(|p| seedbox_spec::definition::Override {
                    id: p.key.clone(),
                    at_ns: p.at_ns,
                    action: p.action.clone(),
                    timeout_ns: p.timeout_ns,
                })
                .collect();
            definition.serialize(serializer)
        } else if matches!(self.inputs, InputSource::Project) {
            crate::Submission::from_spec(self)
                .map_err(serde::ser::Error::custom)?
                .serialize(serializer)
        } else {
            LegacySpec {
                seed: self.seed,
                inputs: self.inputs.clone(),
                configuration: self.configuration.clone(),
                program: self.program.clone(),
            }
            .serialize(serializer)
        }
    }
}
impl<'de> Deserialize<'de> for SimulationSpec {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = serde_json::Value::deserialize(deserializer)?;
        if value.get("runtime").is_some() {
            let definition: seedbox_spec::definition::Definition =
                serde_json::from_value(value).map_err(serde::de::Error::custom)?;
            Ok(Self::resolved(definition))
        } else {
            if value.get("configuration").is_none() {
                let partial: crate::Submission =
                    serde_json::from_value(value).map_err(serde::de::Error::custom)?;
                return partial.into_spec().map_err(serde::de::Error::custom);
            }
            let project: LegacySpec =
                serde_json::from_value(value).map_err(serde::de::Error::custom)?;
            Ok(Self {
                start_paused: false,
                seed: project.seed,
                inputs: project.inputs,
                configuration: project.configuration,
                program: project.program,
            })
        }
    }
}
impl SimulationSpec {
    pub fn resolved(definition: seedbox_spec::definition::Definition) -> Self {
        Self {
            start_paused: false,
            seed: Some(definition.seed),
            configuration: definition.configuration(""),
            program: definition
                .overrides
                .iter()
                .map(|p| ProgramInput {
                    key: p.id.clone(),
                    at_ns: p.at_ns,
                    action: p.action.clone(),
                    timeout_ns: p.timeout_ns,
                })
                .collect(),
            inputs: InputSource::Resolved {
                definition: Box::new(definition),
            },
        }
    }
}
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ExecutionOptions {
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub start_paused: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wall_time_ns: Option<u64>,
}

/// Request identity is scoped to this catalogue and retained for as long as its
/// simulation exists. Reusing it with a different spec is a conflict.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SubmitRequest {
    #[serde(default)]
    pub options: ExecutionOptions,
    #[serde(default)]
    pub metadata: seedbox_spec::definition::Metadata,
    pub request_id: String,
    pub spec: SimulationSpec,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Metadata {
    #[serde(default)]
    pub name: String,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub labels: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub annotations: BTreeMap<String, String>,
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

/// The engine build that executed a simulation. Provenance only: comparison
/// and replay never depend on it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EngineBuild {
    pub version: String,
    /// Source revision, suffixed `-dirty` when built with uncommitted changes;
    /// absent when the build had no Git metadata.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision: Option<String>,
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
    /// Set when execution starts on this server's engine.
    pub engine: Option<EngineBuild>,
    /// Guest property results, reported beside the oracle verdict. Absent
    /// until a final report with property evidence arrives.
    pub properties: Option<PropertyAssessment>,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    engine: Option<EngineBuild>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    properties: Option<PropertyAssessment>,
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
            engine: self.engine.clone(),
            properties: self.properties.clone(),
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
            engine: document.engine,
            properties: document.properties,
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
            engine: None,
            properties: None,
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
    #[serde(default)]
    pub options: ExecutionOptions,
    pub api_version: ApiVersion,
    pub kind: ResourceKind,
    pub metadata: Metadata,
    pub spec: SimulationSpec,
    pub status: SimulationStatus,
}

impl Simulation {
    pub fn queued(metadata: Metadata, spec: SimulationSpec) -> Self {
        Self {
            options: ExecutionOptions::default(),
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
        let spec = serde_json::json!({"seed":123,"oracle":{"image":"example/oracle:latest"}});
        let expected: SimulationSpec = serde_json::from_value(spec.clone()).unwrap();
        assert!(!expected.start_paused);
        assert!(serde_json::to_value(&expected)
            .unwrap()
            .get("startPaused")
            .is_none());
        let mut paused = spec.clone();
        paused["startPaused"] = true.into();
        assert!(serde_json::from_value::<SimulationSpec>(paused).is_err());
        let mut document = serde_json::json!({
            "apiVersion": "seedbox/v1",
            "kind": "Simulation",
            "spec": spec,
            "status": {"futureField": true},
            "metadata": {"id": "deliberately-invalid-old-id", "name":"saved", "labels":{"app":"example"}, "annotations":{"derived-from":"old"}}
        });
        let resubmit: SimulationDocument = serde_json::from_value(document.clone()).unwrap();
        assert_eq!(resubmit.spec, expected);
        assert_eq!(resubmit.metadata.name, "saved");
        assert_eq!(resubmit.metadata.labels["app"], "example");
        assert_eq!(resubmit.metadata.annotations["derived-from"], "old");
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
