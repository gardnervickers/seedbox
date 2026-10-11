//! Partial simulation submissions. Paths are explicit shared-host local inputs;
//! there is no implicit project directory, environment lookup or remote upload.
use crate::{InputSource, ProgramInput, SimulationSpec};
use seedbox_spec::{
    definition::{Architecture, Override},
    project::{self, Machine},
};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    path::{Component, PathBuf},
};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Submission {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seed: Option<u64>,
    /// Omission uses the execution server's architecture.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub architecture: Option<Architecture>,
    #[serde(default)]
    pub machines: BTreeMap<String, Machine>,
    pub oracle: Machine,
    #[serde(default)]
    pub network: Network,
    #[serde(default)]
    pub simulator: Simulator,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub overrides: Vec<Override>,
}

/// Result-affecting defaults for a partial submission.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default, deny_unknown_fields)]
pub struct Simulator {
    pub instructions_per_progress_call: std::num::NonZeroU32,
    pub progress_points_per_block: std::num::NonZeroU32,
    /// Bound on the seeded extra charge per progress call; zero is off.
    #[serde(skip_serializing_if = "is_zero")]
    pub progress_jitter_ns: u64,
    /// Expose world controls to the oracle process.
    pub oracle_control: bool,
}
fn is_zero(value: &u64) -> bool {
    *value == 0
}

impl Default for Simulator {
    fn default() -> Self {
        let progress = project::SimulatorDefaults::default();
        Self {
            instructions_per_progress_call: progress.instructions_per_progress_call,
            progress_points_per_block: progress.progress_points_per_block,
            progress_jitter_ns: progress.progress_jitter_ns,
            oracle_control: true,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default, deny_unknown_fields)]
pub struct Network {
    pub latency_ns: u64,
    pub jitter_ns: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bandwidth_bps: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub queue_bound_ns: Option<u64>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub links: Vec<Link>,
}
impl Default for Network {
    fn default() -> Self {
        Self {
            latency_ns: 1_000_000,
            jitter_ns: 0,
            bandwidth_bps: None,
            queue_bound_ns: None,
            links: Vec::new(),
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Link {
    pub from: String,
    pub to: String,
    pub latency_ns: u64,
    #[serde(default)]
    pub jitter_ns: u64,
    #[serde(default)]
    pub loss_ppm: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bandwidth_bps: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub queue_bound_ns: Option<u64>,
}

fn platform(architecture: Architecture) -> &'static str {
    match architecture {
        Architecture::X86_64 => "linux/amd64",
        Architecture::Aarch64 => "linux/arm64",
    }
}
fn label(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 63
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && !name.starts_with('-')
        && !name.ends_with('-')
}
fn guest_path(path: &str) -> bool {
    path.starts_with('/')
        && path != "/"
        && !path.contains('\0')
        && !std::path::Path::new(path)
            .components()
            .any(|p| matches!(p, Component::ParentDir))
}
fn overlaps(a: &str, b: &str) -> bool {
    a == b || a.starts_with(&format!("{b}/")) || b.starts_with(&format!("{a}/"))
}
impl Submission {
    pub fn into_spec(self) -> Result<SimulationSpec, String> {
        if self.machines.len() > 253 || self.machines.contains_key("oracle") {
            return Err(
                "machines supports at most 253 entries and reserves the name oracle".into(),
            );
        }
        let mut services = BTreeMap::new();
        let mut configs = BTreeMap::new();
        for (name, machine) in self
            .machines
            .into_iter()
            .chain(std::iter::once(("oracle".into(), self.oracle)))
        {
            if !label(&name) {
                return Err(format!("machines.{name}: expected a lowercase DNS label"));
            }
            let source = match (machine.image, machine.build) {
                (Some(reference), None) => seedbox_spec::ImageSource::Registry { reference },
                (None, Some(build)) => {
                    if !build.context.is_absolute() {
                        return Err(format!(
                            "{name}.build.context: expected an absolute shared-host local path"
                        ));
                    }
                    let dockerfile = build.context.join(build.dockerfile);
                    seedbox_spec::ImageSource::Build {
                        context: build.context,
                        dockerfile,
                    }
                }
                _ => return Err(format!("{name}: expected exactly one of image or build")),
            };
            for value in machine
                .entrypoint
                .iter()
                .flatten()
                .chain(machine.command.iter().flatten())
                .chain(machine.working_directory.iter())
                .chain(machine.user.iter())
            {
                if value.contains('\0') {
                    return Err(format!("{name}: launch fields cannot contain NUL"));
                }
            }
            for (key, value) in &machine.environment {
                if key.is_empty()
                    || key.contains(['=', '\0'])
                    || value.as_ref().is_some_and(|v| v.contains('\0'))
                {
                    return Err(format!("{name}.environment: invalid name or NUL value"));
                }
            }
            let mut mounts: Vec<seedbox_spec::ConfigMount> = Vec::new();
            for (index, file) in machine.files.into_iter().enumerate() {
                if !file.source.is_absolute() {
                    return Err(format!(
                        "{name}.files: expected an absolute shared-host local path"
                    ));
                }
                if !guest_path(&file.target)
                    || mounts.iter().any(|m| overlaps(&m.target, &file.target))
                {
                    return Err(format!(
                        "{name}.files: targets must be absolute non-overlapping guest paths"
                    ));
                }
                let key = format!("{name}.file.{index}");
                configs.insert(key.clone(), file.source);
                mounts.push(seedbox_spec::ConfigMount {
                    source: key,
                    target: file.target,
                });
            }
            let mut volumes: Vec<seedbox_spec::VolumeMount> = Vec::new();
            for (key, volume) in machine.volumes {
                if key.is_empty()
                    || !key
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b))
                    || !guest_path(&volume.target)
                    || mounts.iter().any(|m| overlaps(&m.target, &volume.target))
                    || volumes.iter().any(|v| overlaps(&v.target, &volume.target))
                {
                    return Err(format!(
                        "{name}.volumes.{key}: invalid name or overlapping guest target"
                    ));
                }
                volumes.push(seedbox_spec::VolumeMount {
                    source: key,
                    target: volume.target,
                });
            }
            services.insert(
                name,
                seedbox_spec::Service {
                    source,
                    platform: self.architecture.map(platform).unwrap_or_default().into(),
                    entrypoint: machine.entrypoint,
                    command: machine.command,
                    environment: machine.environment,
                    working_dir: machine.working_directory,
                    user: machine.user,
                    configs: mounts,
                    volumes,
                    network: "default".into(),
                },
            );
        }
        let network = seedbox_spec::Network {
            name: "default".into(),
            latency_ns: self.network.latency_ns,
            jitter_ns: self.network.jitter_ns,
            bandwidth_bps: self.network.bandwidth_bps,
            queue_bound_ns: self.network.queue_bound_ns,
            links: self
                .network
                .links
                .into_iter()
                .map(|l| seedbox_spec::Link {
                    from: l.from,
                    to: l.to,
                    latency_ns: l.latency_ns,
                    jitter_ns: l.jitter_ns,
                    loss_ppm: l.loss_ppm,
                    bandwidth_bps: l.bandwidth_bps,
                    queue_bound_ns: l.queue_bound_ns,
                })
                .collect(),
        };
        for link in &network.links {
            if !services.contains_key(&link.from)
                || !services.contains_key(&link.to)
                || link.loss_ppm > 1_000_000
            {
                return Err("network.links: unknown machine or lossPpm exceeds 1000000".into());
            }
        }
        if self.simulator.progress_jitter_ns > project::MAX_PROGRESS_JITTER_NS {
            return Err(format!(
                "simulator.progressJitterNs: exceeds {}",
                project::MAX_PROGRESS_JITTER_NS
            ));
        }
        let configuration = seedbox_spec::Simulation {
            name: String::new(),
            source: PathBuf::new(),
            services,
            configs,
            network,
            simulator: project::SimulatorDefaults {
                instructions_per_progress_call: self.simulator.instructions_per_progress_call,
                progress_points_per_block: self.simulator.progress_points_per_block,
                progress_jitter_ns: self.simulator.progress_jitter_ns,
            },
            schedule: Default::default(),
            control: self
                .simulator
                .oracle_control
                .then(|| seedbox_spec::Control {
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
            seed: self.seed,
            wall_clock_epoch: None,
            machine: Default::default(),
            disk: Default::default(),
        };
        Ok(SimulationSpec {
            start_paused: false,
            seed: self.seed,
            inputs: InputSource::Project,
            configuration,
            program: self
                .overrides
                .into_iter()
                .map(|o| ProgramInput {
                    key: o.id,
                    at_ns: o.at_ns,
                    action: o.action,
                    timeout_ns: o.timeout_ns,
                })
                .collect(),
        })
    }
    pub(crate) fn from_spec(spec: &SimulationSpec) -> Result<Self, String> {
        let config = &spec.configuration;
        if config.check.exit_code != 0 {
            return Err("oracle success exit code must be zero; custom success exit codes are not supported".into());
        }
        for (nondefault, setting) in [
            (
                config.machine != seedbox_spec::MachineOptions::default(),
                "machine (memory_mb, page_timer or deferred_tx)",
            ),
            (
                config.schedule != seedbox_spec::Schedule::default(),
                "schedule",
            ),
            (config.disk != seedbox_spec::DiskOptions::default(), "disk"),
            (config.wall_clock_epoch.is_some(), "wall_clock_epoch"),
        ] {
            if nondefault {
                return Err(format!("partial simulation submissions do not support custom {setting}; submit a complete resolved Simulation definition instead"));
            }
        }
        if config
            .control
            .as_ref()
            .is_some_and(|c| c.service != config.check.service)
        {
            return Err("partial simulation submissions expose controls only to the oracle; submit a complete resolved Simulation definition instead".into());
        }
        let oracle = config
            .services
            .get(&config.check.service)
            .ok_or("oracle machine is missing")?;
        let architecture = match config
            .services
            .values()
            .map(|s| s.platform.as_str())
            .find(|p| !p.is_empty())
        {
            None => None,
            Some("linux/amd64") => Some(Architecture::X86_64),
            Some("linux/arm64") => Some(Architecture::Aarch64),
            Some(other) => return Err(format!("unsupported architecture platform {other}")),
        };
        if config.services.values().any(|s| {
            !s.platform.is_empty() && Some(s.platform.as_str()) != architecture.map(platform)
        }) {
            return Err("all machines must share one simulation architecture".into());
        }
        let convert = |service: &seedbox_spec::Service| -> Result<Machine, String> {
            let (image, build) = match &service.source {
                seedbox_spec::ImageSource::Registry { reference } => {
                    (Some(reference.clone()), None)
                }
                seedbox_spec::ImageSource::Build {
                    context,
                    dockerfile,
                } => (
                    None,
                    Some(project::Build {
                        context: context.clone(),
                        dockerfile: dockerfile.clone(),
                    }),
                ),
            };
            let files = service
                .configs
                .iter()
                .map(|mount| {
                    Ok(project::InitialFile {
                        source: config
                            .configs
                            .get(&mount.source)
                            .ok_or_else(|| format!("missing local file {}", mount.source))?
                            .clone(),
                        target: mount.target.clone(),
                    })
                })
                .collect::<Result<Vec<_>, String>>()?;
            Ok(Machine {
                image,
                build,
                entrypoint: service.entrypoint.clone(),
                command: service.command.clone(),
                environment: service.environment.clone(),
                working_directory: service.working_dir.clone(),
                user: service.user.clone(),
                files,
                volumes: service
                    .volumes
                    .iter()
                    .map(|v| {
                        (
                            v.source.clone(),
                            project::Volume {
                                target: v.target.clone(),
                            },
                        )
                    })
                    .collect(),
            })
        };
        Ok(Self {
            seed: spec.seed,
            architecture,
            oracle: convert(oracle)?,
            machines: config
                .services
                .iter()
                .filter(|(name, _)| *name != &config.check.service)
                .map(|(n, s)| Ok((n.clone(), convert(s)?)))
                .collect::<Result<_, String>>()?,
            network: Network {
                latency_ns: config.network.latency_ns,
                jitter_ns: config.network.jitter_ns,
                bandwidth_bps: config.network.bandwidth_bps,
                queue_bound_ns: config.network.queue_bound_ns,
                links: config
                    .network
                    .links
                    .iter()
                    .map(|l| Link {
                        from: l.from.clone(),
                        to: l.to.clone(),
                        latency_ns: l.latency_ns,
                        jitter_ns: l.jitter_ns,
                        loss_ppm: l.loss_ppm,
                        bandwidth_bps: l.bandwidth_bps,
                        queue_bound_ns: l.queue_bound_ns,
                    })
                    .collect(),
            },
            simulator: Simulator {
                instructions_per_progress_call: config.simulator.instructions_per_progress_call,
                progress_points_per_block: config.simulator.progress_points_per_block,
                progress_jitter_ns: config.simulator.progress_jitter_ns,
                oracle_control: config.control.is_some(),
            },
            overrides: spec
                .program
                .iter()
                .map(|p| Override {
                    id: p.key.clone(),
                    at_ns: p.at_ns,
                    action: p.action.clone(),
                    timeout_ns: p.timeout_ns,
                })
                .collect(),
        })
    }
}
impl SimulationSpec {
    /// Resolve omitted architecture against the execution server before preparation.
    pub fn resolve_project_architecture(
        &mut self,
        architecture: Architecture,
    ) -> Result<(), String> {
        let expected = platform(architecture);
        for (name, service) in &mut self.configuration.services {
            if !service.platform.is_empty() && service.platform != expected {
                return Err(format!(
                    "machine {name}: architecture {} does not match execution server {expected}",
                    service.platform
                ));
            }
            service.platform = expected.into();
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn partial() -> serde_json::Value {
        serde_json::json!({
            "machines":{"app":{"image":"example/app:latest","entrypoint":[],"environment":{"REMOVE":null}}},
            "oracle":{"image":"example/oracle@sha256:abc"}
        })
    }
    #[test]
    fn progress_jitter_round_trips_and_is_omitted_when_off() {
        let spec: SimulationSpec = serde_json::from_value(partial()).unwrap();
        assert_eq!(spec.configuration.simulator.progress_jitter_ns, 0);
        let wire = serde_json::to_value(&spec).unwrap();
        assert!(wire["simulator"].get("progressJitterNs").is_none());
        let mut value = partial();
        value["simulator"] = serde_json::json!({"progressJitterNs": 5000});
        let spec: SimulationSpec = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(spec.configuration.simulator.progress_jitter_ns, 5_000);
        let wire = serde_json::to_value(&spec).unwrap();
        assert_eq!(wire["simulator"]["progressJitterNs"], 5000);
        value["simulator"]["progressJitterNs"] = 1_000_001.into();
        assert!(serde_json::from_value::<SimulationSpec>(value).is_err());
    }
    #[test]
    fn omitted_architecture_uses_server_and_partial_wire_has_no_project_wrapper() {
        let mut spec: SimulationSpec = serde_json::from_value(partial()).unwrap();
        assert!(spec
            .configuration
            .services
            .values()
            .all(|s| s.platform.is_empty()));
        assert_eq!(
            spec.configuration.simulator,
            seedbox_spec::project::SimulatorDefaults::default()
        );
        assert_eq!(spec.configuration.network.latency_ns, 1_000_000);
        let wire = serde_json::to_value(&spec).unwrap();
        for private in [
            "inputs",
            "configuration",
            "source",
            "limits",
            "check",
            "platform",
            "architecture",
        ] {
            assert!(wire.get(private).is_none(), "{private}");
        }
        assert_eq!(wire["machines"]["app"]["entrypoint"], serde_json::json!([]));
        assert_eq!(
            wire["machines"]["app"]["environment"]["REMOVE"],
            serde_json::Value::Null
        );
        spec.resolve_project_architecture(Architecture::Aarch64)
            .unwrap();
        assert!(spec
            .configuration
            .services
            .values()
            .all(|s| s.platform == "linux/arm64"));
    }
    #[test]
    fn explicit_architecture_is_supported_and_mismatch_fails_before_preparation() {
        let mut value = partial();
        value["architecture"] = "x86_64".into();
        let mut spec: SimulationSpec = serde_json::from_value(value).unwrap();
        assert!(spec
            .resolve_project_architecture(Architecture::Aarch64)
            .unwrap_err()
            .contains("does not match"));
        spec.resolve_project_architecture(Architecture::X86_64)
            .unwrap();
    }
    #[test]
    fn custom_internal_semantics_are_rejected_instead_of_dropped() {
        let baseline: SimulationSpec = serde_json::from_value(partial()).unwrap();
        for field in ["machine", "schedule", "disk", "wall_clock_epoch"] {
            let mut spec = baseline.clone();
            match field {
                "machine" => spec.configuration.machine.memory_mb = 512,
                "schedule" => spec.configuration.schedule.timer_jitter_ns = 10,
                "disk" => spec.configuration.disk.torn_writes = true,
                "wall_clock_epoch" => spec.configuration.wall_clock_epoch = Some(86400),
                _ => unreachable!(),
            }
            let error = serde_json::to_value(&spec).unwrap_err().to_string();
            assert!(error.contains(field), "{error}");
            assert!(error.contains("complete resolved Simulation"), "{error}");
        }
    }
    #[test]
    fn nonzero_oracle_success_code_is_rejected_instead_of_normalized() {
        let mut spec: SimulationSpec = serde_json::from_value(partial()).unwrap();
        spec.configuration.check.exit_code = 7;
        let error = serde_json::to_value(&spec).unwrap_err().to_string();
        assert!(
            error.contains("oracle success exit code must be zero"),
            "{error}"
        );
    }

    #[test]
    fn oracle_control_defaults_to_enabled_and_explicit_disabled_roundtrips() {
        let enabled: SimulationSpec = serde_json::from_value(partial()).unwrap();
        assert!(enabled.configuration.control.is_some());
        let mut value = partial();
        value["simulator"] = serde_json::json!({"oracleControl":false});
        let disabled: SimulationSpec = serde_json::from_value(value).unwrap();
        assert!(disabled.configuration.control.is_none());
        let wire = serde_json::to_value(&disabled).unwrap();
        assert_eq!(wire["simulator"]["oracleControl"], false);
        assert_eq!(wire["simulator"]["instructionsPerProgressCall"], 50000);
        assert_eq!(wire["simulator"]["progressPointsPerBlock"], 1);
        let restored: SimulationSpec = serde_json::from_value(wire).unwrap();
        assert!(restored.configuration.control.is_none());
    }

    #[test]
    fn invalid_partial_fields_are_rejected() {
        for (field, value) in [
            ("source", serde_json::json!("/project")),
            ("platform", serde_json::json!("linux/arm64")),
            (
                "simulator",
                serde_json::json!({"instructionsPerProgressCall":0}),
            ),
            ("simulator", serde_json::json!({"progressPointsPerBlock":0})),
            (
                "simulator",
                serde_json::json!({"blocksPerProgressCall":10000}),
            ),
            (
                "machines",
                serde_json::json!({"oracle":{"image":"example/app"}}),
            ),
            (
                "oracle",
                serde_json::json!({"image":"example/app","build":{"context":"/app"}}),
            ),
        ] {
            let mut input = partial();
            input[field] = value;
            assert!(
                serde_json::from_value::<SimulationSpec>(input).is_err(),
                "{field}"
            );
        }
    }
    #[test]
    fn explicit_shared_host_inputs_roundtrip_without_project_source() {
        let value = serde_json::json!({"oracle":{"build":{"context":"/local/oracle"},
            "files":[{"source":"/local/config","target":"/etc/config"}],
            "volumes":{"cache_data":{"target":"/var/cache"}}},
            "overrides":[{"id":"stop","atNs":42,"timeoutNs":50,"action":{"action":"stop","machine":"oracle"}}]});
        let spec: SimulationSpec = serde_json::from_value(value).unwrap();
        let wire = serde_json::to_value(&spec).unwrap();
        assert_eq!(wire["oracle"]["build"]["context"], "/local/oracle");
        assert_eq!(
            wire["oracle"]["build"]["dockerfile"],
            "/local/oracle/Dockerfile"
        );
        assert_eq!(wire["oracle"]["files"][0]["source"], "/local/config");
        assert_eq!(wire["overrides"][0]["timeoutNs"], 50);
        assert!(spec.configuration.source.as_os_str().is_empty());
        let mut relative = wire;
        relative["oracle"]["build"]["context"] = "relative".into();
        assert!(serde_json::from_value::<SimulationSpec>(relative).is_err());
    }
}
