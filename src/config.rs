use crate::error::{Error, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    net::Ipv4Addr,
    path::Path,
};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SchedulerConfig {
    pub tick_ns: u64,
    pub quantum_min: u64,
    pub quantum_max: u64,
    pub max_instructions: u64,
}
impl Default for SchedulerConfig {
    fn default() -> Self {
        Self {
            tick_ns: 10,
            quantum_min: 3,
            quantum_max: 17,
            max_instructions: 100_000,
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Partition {
    pub source: String,
    pub destination: String,
    pub start_ns: u64,
    pub end_ns: u64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct NetworkConfig {
    pub latency_ns: u64,
    pub jitter_ns: u64,
    pub loss_ppm: u64,
    pub duplicate_ppm: u64,
    pub partitions: Vec<Partition>,
}
impl Default for NetworkConfig {
    fn default() -> Self {
        Self {
            latency_ns: 10_000,
            jitter_ns: 0,
            loss_ppm: 0,
            duplicate_ppm: 0,
            partitions: vec![],
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DiskConfig {
    pub latency_ns: u64,
    pub error_ppm: u64,
}
impl Default for DiskConfig {
    fn default() -> Self {
        Self {
            latency_ns: 5_000,
            error_ppm: 0,
        }
    }
}

#[derive(Clone, Debug)]
pub struct RunConfig {
    pub scheduler: SchedulerConfig,
    pub network: NetworkConfig,
    pub disk: DiskConfig,
    pub epoch_ns: u64,
}
impl Default for RunConfig {
    fn default() -> Self {
        Self {
            scheduler: SchedulerConfig::default(),
            network: NetworkConfig::default(),
            disk: DiskConfig::default(),
            epoch_ns: 1_577_836_800_000_000_000,
        }
    }
}
impl RunConfig {
    pub fn fingerprint(&self) -> Value {
        let mut value = serde_json::to_value(&self.scheduler).expect("serialize scheduler");
        value["network"] = json!(self.network);
        value["disk"] = json!(self.disk);
        value["epoch_ns"] = json!(self.epoch_ns);
        value
    }
}

#[derive(Clone)]
pub struct NodeSpec {
    pub name: String,
    pub image: Vec<u8>,
    pub ip: Ipv4Addr,
    pub argv: Vec<String>,
    pub files: BTreeMap<String, Vec<u8>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    schema: u64,
    nodes: Vec<ManifestNode>,
    #[serde(default)]
    scheduler: SchedulerConfig,
    #[serde(default)]
    network: NetworkConfig,
    #[serde(default)]
    disk: DiskConfig,
    #[serde(default = "default_epoch")]
    epoch_ns: u64,
}
fn default_epoch() -> u64 {
    RunConfig::default().epoch_ns
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ManifestNode {
    name: String,
    executable: String,
    ip: Ipv4Addr,
    #[serde(default = "default_argv")]
    argv: Vec<String>,
    #[serde(default)]
    files: BTreeMap<String, Value>,
}
fn default_argv() -> Vec<String> {
    vec!["/app".into()]
}

pub fn load_manifest(path: &Path) -> Result<(RunConfig, Vec<NodeSpec>)> {
    let manifest: Manifest = serde_json::from_slice(&fs::read(path)?)?;
    if manifest.schema != 1 {
        return Err(Error::InvalidInput("manifest schema must be 1".into()));
    }
    let config = RunConfig {
        scheduler: manifest.scheduler,
        network: manifest.network,
        disk: manifest.disk,
        epoch_ns: manifest.epoch_ns,
    };
    let parent = path.parent().unwrap_or(Path::new("."));
    let mut nodes = Vec::new();
    for node in manifest.nodes {
        let mut files = BTreeMap::new();
        for (name, value) in node.files {
            let bytes = match value {
                Value::String(s) => s.into_bytes(),
                Value::Object(ref fields)
                    if fields.len() == 1 && fields.get("hex").and_then(Value::as_str).is_some() =>
                {
                    hex::decode(fields["hex"].as_str().unwrap())
                        .map_err(|e| Error::InvalidInput(e.to_string()))?
                }
                _ => {
                    return Err(Error::InvalidInput(
                        "file contents must be text or {hex: string}".into(),
                    ));
                }
            };
            files.insert(name, bytes);
        }
        nodes.push(NodeSpec {
            name: node.name,
            image: fs::read(parent.join(node.executable))?,
            ip: node.ip,
            argv: node.argv,
            files,
        });
    }
    validate(&config, &nodes)?;
    Ok((config, nodes))
}

pub fn validate(config: &RunConfig, nodes: &[NodeSpec]) -> Result<()> {
    fn range(value: u64, name: &str, min: u64, max: u64) -> Result<()> {
        if !(min..=max).contains(&value) {
            return Err(Error::InvalidInput(format!(
                "{name} must be in {min}..{max}"
            )));
        }
        Ok(())
    }
    let c = &config.scheduler;
    range(c.tick_ns, "tick_ns", 1, 1_000_000_000)?;
    range(c.quantum_min, "quantum_min", 1, 100_000)?;
    range(c.quantum_max, "quantum_max", c.quantum_min, 100_000)?;
    range(c.max_instructions, "max_instructions", 1, 100_000_000)?;
    range(config.epoch_ns, "epoch_ns", 0, i64::MAX as u64)?;
    range(
        config.disk.latency_ns,
        "disk.latency_ns",
        0,
        1_000_000_000_000_000,
    )?;
    range(config.disk.error_ppm, "disk.error_ppm", 0, 1_000_000)?;
    for (name, value, max) in [
        (
            "latency_ns",
            config.network.latency_ns,
            1_000_000_000_000_000,
        ),
        ("jitter_ns", config.network.jitter_ns, 1_000_000_000_000_000),
        ("loss_ppm", config.network.loss_ppm, 1_000_000),
        ("duplicate_ppm", config.network.duplicate_ppm, 1_000_000),
    ] {
        range(value, name, 0, max)?;
    }
    range(nodes.len() as u64, "node count", 1, 32)?;
    let mut names = BTreeSet::new();
    let mut ips = BTreeSet::new();
    for node in nodes {
        if node.name.is_empty()
            || node.name.chars().count() > 64
            || !names.insert(node.name.as_str())
        {
            return Err(Error::InvalidInput(
                "node names must be unique nonempty strings of at most 64 characters".into(),
            ));
        }
        if node.ip.is_unspecified() || !ips.insert(node.ip) {
            return Err(Error::InvalidInput(
                "nodes require unique nonzero IPv4 addresses".into(),
            ));
        }
        if node.argv.is_empty()
            || node.argv.len() > 128
            || node
                .argv
                .iter()
                .any(|s| s.contains('\0') || s.len() + 1 > 16384)
        {
            return Err(Error::InvalidInput("invalid guest argv".into()));
        }
        if node
            .files
            .values()
            .any(|data| data.len() > 32 * 1024 * 1024)
        {
            return Err(Error::InvalidInput("initial file exceeds 32 MiB".into()));
        }
    }
    for p in &config.network.partitions {
        if !names.contains(p.source.as_str())
            || !names.contains(p.destination.as_str())
            || p.start_ns >= p.end_ns
            || p.end_ns > i64::MAX as u64
        {
            return Err(Error::InvalidInput("invalid directed partition".into()));
        }
    }
    Ok(())
}
