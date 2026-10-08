//! Strict Compose subset parsing, without builds, registry access, or execution.

use crate::{invalid, Error, Result};
use serde::{Deserialize, Serialize};
use serde_yaml_ng::{Mapping, Value};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Control {
    pub service: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Simulation {
    pub name: String,
    pub source: PathBuf,
    pub services: BTreeMap<String, Service>,
    pub configs: BTreeMap<String, PathBuf>,
    pub network: Network,
    pub schedule: Schedule,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub control: Option<Control>,
    pub limits: Limits,
    pub check: Check,
    pub seed: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Service {
    pub source: ImageSource,
    pub platform: String,
    /// None inherits image defaults; Some([]) explicitly clears them.
    pub entrypoint: Option<Vec<String>>,
    pub command: Option<Vec<String>>,
    /// None removes an image variable when passthrough was not supplied.
    pub environment: BTreeMap<String, Option<String>>,
    pub working_dir: Option<String>,
    pub user: Option<String>,
    pub configs: Vec<ConfigMount>,
    /// Named volumes, kept on the service machine's retained disk.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub volumes: Vec<VolumeMount>,
    pub network: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ImageSource {
    Registry {
        reference: String,
    },
    Build {
        context: PathBuf,
        dockerfile: PathBuf,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigMount {
    pub source: String,
    pub target: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VolumeMount {
    /// A top-level `volumes` name.
    pub source: String,
    /// Absolute directory in the service's filesystem.
    pub target: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Network {
    pub name: String,
    pub latency_ns: u64,
    pub jitter_ns: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bandwidth_bps: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub queue_bound_ns: Option<u64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub links: Vec<Link>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Link {
    pub from: String,
    pub to: String,
    pub latency_ns: u64,
    pub jitter_ns: u64,
    pub loss_ppm: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bandwidth_bps: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub queue_bound_ns: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Limits {
    pub simulated_time_ns: u64,
    pub wall_time_ns: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Schedule {
    pub timer_jitter_ns: u64,
    pub stop_tolerance_ns: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Check {
    pub service: String,
    pub exit_code: u8,
}

/// Environment passthrough reads only the caller's map. The library never reads
/// process environment or `.env` files, making programmatic calls reproducible.
pub fn load(path: &Path, environment: &BTreeMap<String, String>) -> Result<Simulation> {
    let source = path.canonicalize()?;
    parse(&std::fs::read_to_string(&source)?, &source, environment)
}

pub fn parse(yaml: &str, source: &Path, host: &BTreeMap<String, String>) -> Result<Simulation> {
    parse_for_architecture(yaml, source, host, crate::guest::Architecture::Amd64)
}

/// The CLI selects a guest ISA explicitly. The library never derives it from
/// the host; historical callers retain the original amd64 default.
pub fn load_for_architecture(
    path: &Path,
    environment: &BTreeMap<String, String>,
    architecture: crate::guest::Architecture,
) -> Result<Simulation> {
    let source = std::fs::canonicalize(path)?;
    parse_for_architecture(
        &std::fs::read_to_string(&source)?,
        &source,
        environment,
        architecture,
    )
}

pub fn parse_for_architecture(
    yaml: &str,
    source: &Path,
    host: &BTreeMap<String, String>,
    architecture: crate::guest::Architecture,
) -> Result<Simulation> {
    let doc: Value = serde_yaml_ng::from_str(yaml)?;
    reject_tags(&doc, "$")?;
    let top = fields(
        &doc,
        "$",
        &[
            "name",
            "services",
            "configs",
            "volumes",
            "networks",
            "x-seedbox",
        ],
    )?;
    let base = source.parent().unwrap_or(Path::new("."));
    let name = match top.get("name") {
        Some(value) => text(value, "$.name")?,
        None => base
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("simulation")
            .into(),
    };
    if name.is_empty()
        || !name.as_bytes()[0].is_ascii_alphanumeric()
        || !name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"_-".contains(&b))
    {
        return invalid("$.name", "expected a lowercase Compose project name");
    }
    let mut configs = BTreeMap::new();
    for (key, value) in entries(top.get("configs"), "$.configs")? {
        if !identifier(&key) {
            return invalid("$.configs", format!("invalid config name {key:?}"));
        }
        let p = format!("$.configs.{key}");
        let cfg = fields(value, &p, &["file"])?;
        configs.insert(
            key,
            local_path(get(cfg, "file"), &format!("{p}.file"), base, false)?,
        );
    }
    let mut volumes = BTreeMap::new();
    for (key, value) in entries(top.get("volumes"), "$.volumes")? {
        if !identifier(&key) {
            return invalid("$.volumes", format!("invalid volume name {key:?}"));
        }
        empty_options(value, &format!("$.volumes.{key}"))?;
        // The service that mounts it, once one does.
        volumes.insert(key, None::<String>);
    }
    let network_name = if let Some(networks) = top.get("networks") {
        let nets = entries(Some(networks), "$.networks")?;
        if nets.len() != 1 {
            return invalid("$.networks", "exactly one private network is supported");
        }
        let (key, options) = &nets[0];
        if !identifier(key) {
            return invalid("$.networks", "invalid network name");
        }
        empty_options(options, &format!("$.networks.{key}"))?;
        key.clone()
    } else {
        "default".into()
    };
    let raw_services = entries(Some(get(top, "services")), "$.services")?;
    if raw_services.is_empty() || raw_services.len() > 254 {
        return invalid("$.services", "expected 1 to 254 services");
    }
    let mut services = BTreeMap::new();
    for (key, value) in raw_services {
        let p = format!("$.services.{key}");
        if !dns_label(&key) {
            return invalid(
                &p,
                "service names must be lowercase DNS labels (1 to 63 characters)",
            );
        }
        let svc = fields(
            value,
            &p,
            &[
                "image",
                "build",
                "platform",
                "entrypoint",
                "command",
                "environment",
                "working_dir",
                "user",
                "configs",
                "volumes",
                "networks",
            ],
        )?;
        let image_source =
            match (svc.get("image"), svc.get("build")) {
                (Some(image), None) => {
                    let reference = text(image, &format!("{p}.image"))?;
                    if reference.chars().any(char::is_whitespace) || reference.contains("://") {
                        return invalid(
                            &format!("{p}.image"),
                            "expected a registry image reference without a transport prefix",
                        );
                    }
                    ImageSource::Registry { reference }
                }
                (None, Some(build)) => {
                    let (context, dockerfile) = if build.is_string() {
                        let context = local_path(build, &format!("{p}.build"), base, true)?;
                        let dockerfile = local_path(
                            &Value::String("Dockerfile".into()),
                            &format!("{p}.build.dockerfile"),
                            &context,
                            false,
                        )?;
                        (context, dockerfile)
                    } else {
                        let b = fields(build, &format!("{p}.build"), &["context", "dockerfile"])?;
                        let context = local_path(
                            b.get("context").unwrap_or(&Value::String(".".into())),
                            &format!("{p}.build.context"),
                            base,
                            true,
                        )?;
                        let dockerfile = local_path(
                            b.get("dockerfile")
                                .unwrap_or(&Value::String("Dockerfile".into())),
                            &format!("{p}.build.dockerfile"),
                            &context,
                            false,
                        )?;
                        (context, dockerfile)
                    };
                    ImageSource::Build {
                        context,
                        dockerfile,
                    }
                }
                _ => return invalid(
                    &p,
                    "specify exactly one of image or build; pull/build precedence is unsupported",
                ),
            };
        let platform = optional_text(svc.get("platform"), &format!("{p}.platform"))?
            .unwrap_or(architecture.platform().into());
        if platform != architecture.platform() {
            return invalid(
                &format!("{p}.platform"),
                format!("only {} is supported", architecture.platform()),
            );
        }
        let nets = match svc.get("networks") {
            None => vec!["default".into()],
            Some(Value::Sequence(nets)) => nets
                .iter()
                .map(|n| text(n, &format!("{p}.networks")))
                .collect::<Result<Vec<_>>>()?,
            Some(v) => entries(Some(v), &format!("{p}.networks"))?
                .into_iter()
                .map(|(name, opts)| {
                    empty_options(opts, &format!("{p}.networks.{name}"))?;
                    Ok(name)
                })
                .collect::<Result<Vec<_>>>()?,
        };
        if nets != [network_name.clone()] {
            return invalid(
                &format!("{p}.networks"),
                format!("must attach to the one declared network {network_name:?}"),
            );
        }
        let mut mounts: Vec<ConfigMount> = Vec::new();
        if let Some(raw) = svc.get("configs") {
            let list = sequence(raw, &format!("{p}.configs"))?;
            for (i, item) in list.iter().enumerate() {
                let cp = format!("{p}.configs[{i}]");
                let (source, target) = if item.is_string() {
                    let name = text(item, &cp)?;
                    (name.clone(), format!("/{name}"))
                } else {
                    let mount = fields(item, &cp, &["source", "target"])?;
                    let source = text(get(mount, "source"), &format!("{cp}.source"))?;
                    let target = optional_text(mount.get("target"), &format!("{cp}.target"))?
                        .unwrap_or(format!("/{source}"));
                    (source, target)
                };
                if !configs.contains_key(&source) {
                    return invalid(&cp, format!("unknown config {source:?}"));
                }
                guest_path(&target, &format!("{cp}.target"))?;
                if target == "/" || mounts.iter().any(|m| m.target == target) {
                    return invalid(&cp, "config target must be a unique file path");
                }
                mounts.push(ConfigMount { source, target });
            }
        }
        let mut volume_mounts: Vec<VolumeMount> = Vec::new();
        if let Some(raw) = svc.get("volumes") {
            for (i, item) in sequence(raw, &format!("{p}.volumes"))?.iter().enumerate() {
                let vp = format!("{p}.volumes[{i}]");
                let (source, target) = if item.is_string() {
                    let short = text(item, &vp)?;
                    match short.split(':').collect::<Vec<_>>()[..] {
                        [source, target] => (source.to_owned(), target.to_owned()),
                        _ => {
                            return invalid(
                                &vp,
                                "expected NAME:/path; access modes and bind mounts are unsupported",
                            )
                        }
                    }
                } else {
                    let mount = fields(item, &vp, &["type", "source", "target"])?;
                    if text(get(mount, "type"), &format!("{vp}.type"))? != "volume" {
                        return invalid(&format!("{vp}.type"), "only named volumes are supported");
                    }
                    (
                        text(get(mount, "source"), &format!("{vp}.source"))?,
                        text(get(mount, "target"), &format!("{vp}.target"))?,
                    )
                };
                match volumes.get_mut(&source) {
                    None => return invalid(&vp, format!("unknown volume {source:?}; declare it under top-level volumes")),
                    Some(Some(user)) => return invalid(&vp, format!("volume {source:?} is already mounted by service {user:?}; each volume lives on one machine's disk")),
                    Some(owner) => *owner = Some(key.clone()),
                }
                guest_path(&target, &format!("{vp}.target"))?;
                if target == "/" || volume_mounts.iter().any(|m| m.target == target) {
                    return invalid(&vp, "volume target must be a unique directory other than /");
                }
                if mounts
                    .iter()
                    .any(|m| m.target == target || m.target.starts_with(&format!("{target}/")))
                {
                    return invalid(&vp, "volume target overlaps a config mount");
                }
                volume_mounts.push(VolumeMount { source, target });
            }
        }
        let working_dir = optional_text(svc.get("working_dir"), &format!("{p}.working_dir"))?;
        if let Some(dir) = &working_dir {
            guest_path(dir, &format!("{p}.working_dir"))?;
        }
        services.insert(
            key,
            Service {
                source: image_source,
                platform,
                entrypoint: command(svc.get("entrypoint"), &format!("{p}.entrypoint"))?,
                command: command(svc.get("command"), &format!("{p}.command"))?,
                environment: resolve_environment(
                    svc.get("environment"),
                    &format!("{p}.environment"),
                    host,
                )?,
                working_dir,
                user: optional_text(svc.get("user"), &format!("{p}.user"))?,
                configs: mounts,
                volumes: volume_mounts,
                network: network_name.clone(),
            },
        );
    }
    let ext = fields(
        get(top, "x-seedbox"),
        "$.x-seedbox",
        &[
            "version", "seed", "limits", "network", "schedule", "faults", "control", "check",
        ],
    )?;
    if get(ext, "version").as_u64() != Some(1) {
        return invalid("$.x-seedbox.version", "expected version 1");
    }
    let lim = fields(
        get(ext, "limits"),
        "$.x-seedbox.limits",
        &["simulated_time", "wall_time"],
    )?;
    let limits = Limits {
        simulated_time_ns: duration(
            get(lim, "simulated_time"),
            "$.x-seedbox.limits.simulated_time",
            false,
        )?,
        wall_time_ns: duration(get(lim, "wall_time"), "$.x-seedbox.limits.wall_time", false)?,
    };
    let mut network = Network {
        name: network_name,
        latency_ns: 1_000_000,
        jitter_ns: 0,
        bandwidth_bps: None,
        queue_bound_ns: None,
        links: Vec::new(),
    };
    if let Some(raw) = ext.get("network") {
        let net = fields(
            raw,
            "$.x-seedbox.network",
            &["latency", "jitter", "links", "bandwidth_bps", "queue_bound"],
        )?;
        if let Some(v) = net.get("latency") {
            network.latency_ns = duration(v, "$.x-seedbox.network.latency", false)?;
        }
        if let Some(v) = net.get("jitter") {
            network.jitter_ns = duration(v, "$.x-seedbox.network.jitter", true)?;
        }
        (network.bandwidth_bps, network.queue_bound_ns) = bandwidth(net, "$.x-seedbox.network")?;
    }
    if network.jitter_ns >= network.latency_ns {
        return invalid("$.x-seedbox.network.jitter", "must be less than latency");
    }
    if let Some(raw) = ext.get("network").and_then(|v| v.get("links")) {
        let mut pairs = std::collections::BTreeSet::new();
        for (i, raw) in sequence(raw, "$.x-seedbox.network.links")?
            .iter()
            .enumerate()
        {
            let p = format!("$.x-seedbox.network.links[{i}]");
            let entry = fields(
                raw,
                &p,
                &[
                    "from",
                    "to",
                    "latency",
                    "jitter",
                    "loss_ppm",
                    "bandwidth_bps",
                    "queue_bound",
                ],
            )?;
            let from = text(get(entry, "from"), &format!("{p}.from"))?;
            let to = text(get(entry, "to"), &format!("{p}.to"))?;
            for (key, name) in [("from", &from), ("to", &to)] {
                if !services.contains_key(name) {
                    return invalid(&format!("{p}.{key}"), "unknown service");
                }
            }
            if !pairs.insert((from.clone(), to.clone())) {
                return invalid(&p, "duplicate directed pair");
            }
            let latency_ns = duration(get(entry, "latency"), &format!("{p}.latency"), false)?;
            let jitter_ns = entry
                .get("jitter")
                .map(|v| duration(v, &format!("{p}.jitter"), true))
                .transpose()?
                .unwrap_or(0);
            let loss_ppm = entry
                .get("loss_ppm")
                .map(|v| {
                    v.as_u64().filter(|n| *n <= 1_000_000).ok_or_else(|| {
                        validation(
                            &format!("{p}.loss_ppm"),
                            "expected an integer in [0, 1000000]",
                        )
                    })
                })
                .transpose()?
                .unwrap_or(0);
            if latency_ns < network.latency_ns {
                return invalid(&format!("{p}.latency"), "must be at least global latency");
            }
            if jitter_ns >= latency_ns {
                return invalid(&format!("{p}.jitter"), "must be less than latency");
            }
            if latency_ns
                .checked_add(jitter_ns)
                .is_none_or(|n| n > 3_600_000_000_000)
            {
                return invalid(&p, "latency + jitter must not exceed one hour");
            }
            let (bandwidth_bps, queue_bound_ns) = bandwidth(entry, &p)?;
            network.links.push(Link {
                from,
                to,
                latency_ns,
                jitter_ns,
                loss_ppm,
                bandwidth_bps,
                queue_bound_ns,
            });
        }
    }
    let mut schedule = Schedule::default();
    if let Some(raw) = ext.get("schedule") {
        let options = fields(
            raw,
            "$.x-seedbox.schedule",
            &["timer_jitter", "stop_tolerance"],
        )?;
        if let Some(v) = options.get("timer_jitter") {
            schedule.timer_jitter_ns = duration(v, "$.x-seedbox.schedule.timer_jitter", true)?;
        }
        if let Some(v) = options.get("stop_tolerance") {
            schedule.stop_tolerance_ns = duration(v, "$.x-seedbox.schedule.stop_tolerance", true)?;
        }
    }
    reject_static_faults(ext.get("faults"))?;
    let control = ext
        .get("control")
        .map(|raw| {
            let options = fields(raw, "$.x-seedbox.control", &["service"])?;
            let service = text(get(options, "service"), "$.x-seedbox.control.service")?;
            if !services.contains_key(&service) {
                return invalid("$.x-seedbox.control.service", "unknown controller service");
            }
            Ok(Control { service })
        })
        .transpose()?;
    let raw_check = fields(
        get(ext, "check"),
        "$.x-seedbox.check",
        &["service", "exit_code"],
    )?;
    let check_service = text(get(raw_check, "service"), "$.x-seedbox.check.service")?;
    if !services.contains_key(&check_service) {
        return invalid(
            "$.x-seedbox.check.service",
            format!("unknown service {check_service:?}"),
        );
    }
    if control.as_ref().is_some_and(|c| c.service != check_service) {
        return invalid(
            "$.x-seedbox.control.service",
            "controller must also be the check service",
        );
    }
    let exit_code = match raw_check.get("exit_code") {
        None => 0,
        Some(v) => v.as_u64().filter(|v| *v <= 255).ok_or_else(|| {
            validation(
                "$.x-seedbox.check.exit_code",
                "expected an integer in [0, 255]",
            )
        })? as u8,
    };
    let seed = ext
        .get("seed")
        .map(|v| {
            v.as_u64().ok_or_else(|| {
                validation("$.x-seedbox.seed", "expected an unsigned 64-bit integer")
            })
        })
        .transpose()?;
    Ok(Simulation {
        name,
        source: source.to_owned(),
        services,
        configs,
        network,
        schedule,
        control,
        limits,
        check: Check {
            service: check_service,
            exit_code,
        },
        seed,
    })
}

fn bandwidth(fields: &Mapping, path: &str) -> Result<(Option<u64>, Option<u64>)> {
    let rate = fields
        .get("bandwidth_bps")
        .map(|v| {
            v.as_u64().filter(|n| *n > 0).ok_or_else(|| {
                validation(
                    &format!("{path}.bandwidth_bps"),
                    "expected a positive integer in bits per second",
                )
            })
        })
        .transpose()?;
    let bound = fields
        .get("queue_bound")
        .map(|v| duration(v, &format!("{path}.queue_bound"), true))
        .transpose()?;
    if bound.is_some() && rate.is_none() {
        return invalid(&format!("{path}.queue_bound"), "requires bandwidth_bps");
    }
    Ok((rate, bound))
}

fn reject_static_faults(raw: Option<&Value>) -> Result<()> {
    if let Some(raw) = raw {
        if !sequence(raw, "$.x-seedbox.faults")?.is_empty() {
            return invalid(
                "$.x-seedbox.faults",
                "static schedules are unsupported; use the in-world control RPC API",
            );
        }
    }
    Ok(())
}

fn validation(path: &str, message: &str) -> Error {
    Error::Validation {
        path: path.into(),
        message: message.into(),
    }
}

fn reject_tags(value: &Value, path: &str) -> Result<()> {
    match value {
        Value::Tagged(_) => return invalid(path, "YAML tags are unsupported"),
        Value::Mapping(map) => {
            for (key, value) in map {
                reject_tags(key, path)?;
                reject_tags(
                    value,
                    &format!("{path}.{}", key.as_str().unwrap_or("<key>")),
                )?;
            }
        }
        Value::Sequence(list) => {
            for (index, value) in list.iter().enumerate() {
                reject_tags(value, &format!("{path}[{index}]"))?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn get<'a>(map: &'a Mapping, key: &str) -> &'a Value {
    map.get(key).unwrap_or(&Value::Null)
}

fn fields<'a>(value: &'a Value, path: &str, allowed: &[&str]) -> Result<&'a Mapping> {
    let map = value
        .as_mapping()
        .ok_or_else(|| validation(path, "expected a mapping"))?;
    for key in map.keys() {
        let key = key
            .as_str()
            .ok_or_else(|| validation(path, "mapping keys must be strings"))?;
        if !allowed.contains(&key) {
            return invalid(&format!("{path}.{key}"), "unsupported field");
        }
    }
    Ok(map)
}

fn entries<'a>(value: Option<&'a Value>, path: &str) -> Result<Vec<(String, &'a Value)>> {
    let Some(value) = value else {
        return Ok(vec![]);
    };
    let map = value
        .as_mapping()
        .ok_or_else(|| validation(path, "expected a mapping"))?;
    map.iter()
        .map(|(k, v)| {
            Ok((
                k.as_str()
                    .ok_or_else(|| validation(path, "mapping keys must be strings"))?
                    .into(),
                v,
            ))
        })
        .collect()
}

fn sequence<'a>(value: &'a Value, path: &str) -> Result<&'a Vec<Value>> {
    value
        .as_sequence()
        .ok_or_else(|| validation(path, "expected a list"))
}

fn empty_options(value: &Value, path: &str) -> Result<()> {
    if !value.is_null() {
        fields(value, path, &[])?;
    }
    Ok(())
}

fn text(value: &Value, path: &str) -> Result<String> {
    let text = literal(value, path)?;
    if text.is_empty() {
        return invalid(path, "expected a nonempty string");
    }
    Ok(text)
}

fn literal(value: &Value, path: &str) -> Result<String> {
    let raw = value
        .as_str()
        .ok_or_else(|| validation(path, "expected a string (quote numbers and booleans)"))?;
    if raw.contains('\0') {
        return invalid(path, "NUL is not allowed");
    }
    let mut result = String::new();
    let mut chars = raw.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '$' {
            if chars.peek() == Some(&'$') {
                chars.next();
            } else if chars
                .peek()
                .is_some_and(|c| *c == '{' || *c == '_' || c.is_ascii_alphabetic())
            {
                return invalid(
                    path,
                    "Compose interpolation is unsupported; use literal values or $$",
                );
            }
        }
        result.push(c);
    }
    Ok(result)
}

fn optional_text(value: Option<&Value>, path: &str) -> Result<Option<String>> {
    value.map(|v| text(v, path)).transpose()
}

fn command(value: Option<&Value>, path: &str) -> Result<Option<Vec<String>>> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(_)) => shell_words::split(&literal(value.unwrap(), path)?)
            .map(Some)
            .map_err(|e| validation(path, &e.to_string())),
        Some(value) => sequence(value, path)?
            .iter()
            .enumerate()
            .map(|(i, v)| literal(v, &format!("{path}[{i}]")))
            .collect::<Result<Vec<_>>>()
            .map(Some),
    }
}

fn resolve_environment(
    value: Option<&Value>,
    path: &str,
    host: &BTreeMap<String, String>,
) -> Result<BTreeMap<String, Option<String>>> {
    let mut result = BTreeMap::new();
    let pairs = match value {
        None => vec![],
        Some(Value::Sequence(list)) => list
            .iter()
            .map(|v| {
                let item = text(v, path)?;
                Ok(match item.split_once('=') {
                    Some((k, v)) => (k.into(), Some(v.into())),
                    None => (item, None),
                })
            })
            .collect::<Result<Vec<_>>>()?,
        Some(v) => entries(Some(v), path)?
            .into_iter()
            .map(|(key, val)| {
                let val = if val.is_null() {
                    None
                } else {
                    Some(literal(val, &format!("{path}.{key}"))?)
                };
                Ok((key, val))
            })
            .collect::<Result<Vec<_>>>()?,
    };
    for (key, val) in pairs {
        if key.is_empty()
            || !(key.as_bytes()[0].is_ascii_alphabetic() || key.starts_with('_'))
            || !key.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_')
        {
            return invalid(path, format!("invalid environment name {key:?}"));
        }
        let resolved = val.or_else(|| host.get(&key).cloned());
        if resolved.as_ref().is_some_and(|v| v.contains('\0')) {
            return invalid(path, "environment value contains NUL");
        }
        if result.insert(key.clone(), resolved).is_some() {
            return invalid(path, format!("duplicate environment variable {key:?}"));
        }
    }
    Ok(result)
}

fn identifier(s: &str) -> bool {
    !s.is_empty()
        && s.as_bytes()[0].is_ascii_alphanumeric()
        && s.bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"_.-".contains(&c))
}
fn dns_label(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 63
        && s.as_bytes()[0].is_ascii_alphanumeric()
        && s.as_bytes()[s.len() - 1].is_ascii_alphanumeric()
        && s.bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
}

pub(crate) fn guest_path(s: &str, path: &str) -> Result<()> {
    if !s.starts_with('/')
        || s.starts_with("//")
        || s.contains('\0')
        || s.split('/').any(|p| p == ".." || p == ".")
        || (s != "/" && (s.ends_with('/') || s.contains("//")))
    {
        return invalid(path, "expected a normalized absolute guest path");
    }
    Ok(())
}

fn local_path(value: &Value, path: &str, base: &Path, directory: bool) -> Result<PathBuf> {
    let raw = text(value, path)?;
    if raw.contains("://") || raw.starts_with("git@") || raw.starts_with('~') {
        return invalid(path, "only explicit local paths are supported");
    }
    let resolved = base
        .join(raw)
        .canonicalize()
        .map_err(|e| validation(path, &e.to_string()))?;
    if !(if directory {
        resolved.is_dir()
    } else {
        resolved.is_file()
    }) {
        return invalid(
            path,
            if directory {
                "expected a directory"
            } else {
                "expected a file"
            },
        );
    }
    Ok(resolved)
}

fn duration(value: &Value, path: &str, zero: bool) -> Result<u64> {
    let raw = text(value, path)?;
    let pos = raw
        .find(|c: char| !c.is_ascii_digit() && c != '.')
        .ok_or_else(|| validation(path, "duration needs a unit: ns, us, ms, s, m, h"))?;
    let (number, unit) = raw.split_at(pos);
    let scale: u128 = match unit {
        "ns" => 1,
        "us" => 1000,
        "ms" => 1_000_000,
        "s" => 1_000_000_000,
        "m" => 60_000_000_000,
        "h" => 3_600_000_000_000,
        _ => return invalid(path, "unknown duration unit"),
    };
    let (whole, fraction) = number.split_once('.').unwrap_or((number, ""));
    if whole.is_empty()
        || fraction.len() > 18
        || (number.contains('.') && fraction.is_empty())
        || !fraction.bytes().all(|c| c.is_ascii_digit())
    {
        return invalid(path, "invalid decimal duration");
    }
    let denominator = 10u128.pow(fraction.len() as u32);
    let fraction: u128 = if fraction.is_empty() {
        0
    } else {
        fraction
            .parse()
            .map_err(|_| validation(path, "invalid duration"))?
    };
    let whole: u128 = whole
        .parse()
        .map_err(|_| validation(path, "invalid duration"))?;
    let fractional_ns = fraction * scale;
    if !fractional_ns.is_multiple_of(denominator) {
        return invalid(path, "duration must use whole nanoseconds");
    }
    let ns = whole
        .checked_mul(scale)
        .and_then(|v| v.checked_add(fractional_ns / denominator))
        .and_then(|v| u64::try_from(v).ok())
        .ok_or_else(|| validation(path, "duration exceeds unsigned 64-bit nanoseconds"))?;
    if ns == 0 && !zero {
        return invalid(path, "duration must be positive");
    }
    Ok(ns)
}
