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
/// deserializing a resource, it accepts any status representation while still
/// validating the API version, resource kind and entire spec.
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

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SimulationSpec {
    pub seed: Option<u64>,
    pub inputs: InputSource,
    pub configuration: seedbox_spec::Simulation,
    pub program: Vec<seedbox_spec::definition::Override>,
}
/// Wire form for every input source except a resolved definition, which is
/// serialized as the definition itself.
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ConfiguredSpec {
    seed: Option<u64>,
    inputs: InputSource,
    configuration: seedbox_spec::Simulation,
    #[serde(default)]
    program: Vec<seedbox_spec::definition::Override>,
}
impl Serialize for SimulationSpec {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        if let InputSource::Resolved { definition } = &self.inputs {
            let mut definition = definition.as_ref().clone();
            definition.seed = self.seed.unwrap_or(definition.seed);
            definition.overrides = self.program.clone();
            definition.serialize(serializer)
        } else {
            ConfiguredSpec {
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
            return Self::resolved(definition).map_err(serde::de::Error::custom);
        }
        let spec: ConfiguredSpec =
            serde_json::from_value(value).map_err(serde::de::Error::custom)?;
        if matches!(spec.inputs, InputSource::Resolved { .. }) {
            return Err(serde::de::Error::custom(
                "resolved inputs are represented by their definition",
            ));
        }
        Ok(Self {
            seed: spec.seed,
            inputs: spec.inputs,
            configuration: spec.configuration,
            program: spec.program,
        })
    }
}
impl SimulationSpec {
    pub fn resolved(definition: seedbox_spec::definition::Definition) -> Result<Self, String> {
        let configuration = definition.configuration("")?;
        Ok(Self {
            seed: Some(definition.seed),
            configuration,
            program: definition.overrides.clone(),
            inputs: InputSource::Resolved {
                definition: Box::new(definition),
            },
        })
    }

    /// Resolve omitted architecture against the execution server before preparation.
    pub fn resolve_project_architecture(
        &mut self,
        architecture: seedbox_spec::definition::Architecture,
    ) -> Result<(), String> {
        use seedbox_spec::definition::Architecture;
        let (expected, maximum) = match architecture {
            Architecture::X86_64 => ("linux/amd64", 65536),
            Architecture::Aarch64 => ("linux/arm64", 3072),
        };
        for (name, service) in &mut self.configuration.services {
            if !service.platform.is_empty() && service.platform != expected {
                return Err(format!(
                    "machine {name}: architecture {} does not match execution server {expected}",
                    service.platform
                ));
            }
            if let Some(memory) = service.memory_mb {
                if !(64..=maximum).contains(&memory) {
                    return Err(format!("machine {name}: memoryMiB must be 64..{maximum}"));
                }
            }
            service.platform = expected.into();
        }
        Ok(())
    }
}
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ExecutionOptions {
    /// CPU executor for fresh projects. Retained definitions own their backend.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backend: Option<seedbox_spec::backend::CpuBackend>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub start_paused: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wall_time_ns: Option<u64>,
    /// Threads that run machines within a round. Performance only; never
    /// part of the simulation identity and never changes results.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub machine_workers: Option<u32>,
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

// Keep the derived verdict at the wire boundary, never in mutable status.
// Supplied verdicts are ignored on input.
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
    verdict: TestVerdict,
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
            verdict: self.verdict(),
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
    use seedbox_spec::definition::{Architecture, Override};

    fn project() -> SimulationSpec {
        let service = |reference: &str| seedbox_spec::Service {
            memory_mb: None,
            source: seedbox_spec::ImageSource::Registry {
                reference: reference.into(),
            },
            platform: String::new(),
            entrypoint: None,
            command: None,
            environment: BTreeMap::new(),
            working_dir: None,
            user: None,
            configs: Vec::new(),
            volumes: Vec::new(),
            network: "default".into(),
        };
        SimulationSpec {
            seed: Some(123),
            inputs: InputSource::Project,
            program: Vec::new(),
            configuration: seedbox_spec::Simulation {
                name: "test".into(),
                source: Default::default(),
                services: BTreeMap::from([
                    ("app".into(), service("example/app:latest")),
                    ("oracle".into(), service("example/oracle:latest")),
                ]),
                configs: BTreeMap::new(),
                network: seedbox_spec::Network {
                    name: "default".into(),
                    latency_ns: 1_000_000,
                    jitter_ns: 0,
                    bandwidth_bps: None,
                    queue_bound_ns: None,
                    links: Vec::new(),
                },
                simulator: Default::default(),
                schedule: Default::default(),
                control: Some(seedbox_spec::Control {
                    service: "oracle".into(),
                }),
                limits: seedbox_spec::Limits {
                    simulated_time_ns: u64::MAX,
                    wall_time_ns: u64::MAX,
                },
                check: seedbox_spec::Check {
                    service: "oracle".into(),
                    exit_code: 0,
                },
                seed: None,
                wall_clock_epoch: None,
                machine: Default::default(),
                disk: Default::default(),
            },
        }
    }

    #[test]
    fn cpu_backend_round_trips_only_as_a_request_option() {
        let options: ExecutionOptions =
            serde_json::from_value(serde_json::json!({"backend":"tcg"})).unwrap();
        assert_eq!(
            options.backend,
            Some(seedbox_spec::backend::CpuBackend::Tcg)
        );
        assert_eq!(serde_json::to_value(&options).unwrap()["backend"], "tcg");
        assert_eq!(ExecutionOptions::default().backend, None);
        for field in ["backend", "simulator"] {
            let mut spec = serde_json::to_value(project()).unwrap();
            spec[field] = if field == "backend" {
                serde_json::json!("tcg")
            } else {
                serde_json::json!({"backend":"tcg"})
            };
            assert!(serde_json::from_value::<SimulationSpec>(spec).is_err());
        }
    }

    #[test]
    fn project_inputs_round_trip_with_configuration_and_program() {
        let mut spec = project();
        spec.program.push(Override {
            id: "stop".into(),
            at_ns: 42,
            action: serde_json::json!({"action":"stop","machine":"oracle"}),
            timeout_ns: 50,
        });
        let wire = serde_json::to_value(&spec).unwrap();
        assert_eq!(wire["inputs"]["kind"], "project");
        assert_eq!(wire["program"][0]["id"], "stop");
        assert_eq!(wire["program"][0]["timeoutNs"], 50);
        assert_eq!(
            serde_json::from_value::<SimulationSpec>(wire.clone()).unwrap(),
            spec
        );
        let mut defaulted = wire;
        defaulted["program"][0]
            .as_object_mut()
            .unwrap()
            .remove("timeoutNs");
        assert_eq!(
            serde_json::from_value::<SimulationSpec>(defaulted)
                .unwrap()
                .program[0]
                .timeout_ns,
            seedbox_control::PROCESS_EFFECT_TIMEOUT_NS
        );
    }

    #[test]
    fn project_architecture_resolves_against_the_server_and_bounds_memory() {
        let mut spec = project();
        spec.resolve_project_architecture(Architecture::Aarch64)
            .unwrap();
        assert!(spec
            .configuration
            .services
            .values()
            .all(|s| s.platform == "linux/arm64"));
        assert!(spec
            .resolve_project_architecture(Architecture::X86_64)
            .unwrap_err()
            .contains("does not match"));
        let mut large = project();
        large
            .configuration
            .services
            .get_mut("app")
            .unwrap()
            .memory_mb = Some(4096);
        assert!(large
            .clone()
            .resolve_project_architecture(Architecture::Aarch64)
            .unwrap_err()
            .contains("memoryMiB"));
        large
            .resolve_project_architecture(Architecture::X86_64)
            .unwrap();
    }

    #[test]
    fn resubmission_ignores_server_fields_but_rejects_unknown_spec_fields() {
        let expected = project();
        let spec = serde_json::to_value(&expected).unwrap();
        assert!(spec.get("startPaused").is_none());
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
    fn input_verdict_cannot_override_oracle() {
        let status = SimulationStatus {
            phase: Phase::Completed,
            oracle_exit_code: Some(5),
            ..Default::default()
        };
        let mut document = serde_json::to_value(&status).unwrap();
        document["verdict"] = serde_json::json!("passed");
        let decoded: SimulationStatus = serde_json::from_value(document.clone()).unwrap();
        assert_eq!(decoded, status);
        assert_eq!(decoded.verdict(), TestVerdict::Failed);
        assert_eq!(serde_json::to_value(decoded).unwrap()["verdict"], "failed");
        document["unknown"] = serde_json::json!(true);
        assert!(serde_json::from_value::<SimulationStatus>(document).is_err());
    }
}
