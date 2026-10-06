//! Import pinned engine fixtures without rebuilding their guests or changing
//! machine order, command lines, network settings, or seeded fault schedules.
//! This is a compatibility input format, not an extension of Compose syntax.
use crate::{
    config::*,
    execution::*,
    store::{ContentDigest, ContentStore},
    Error, Result,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

pub const ABI: &str = "world-fixture-v1";
pub const DEFINITION: &str = "world-definition";

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Machine {
    name: String,
    initrd: PathBuf,
    #[serde(default)]
    cmdline: Option<String>,
}
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Link {
    latency_ns: u64,
    #[serde(default)]
    jitter_ns: u64,
}
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Kills {
    members: Vec<String>,
    start_ns: u64,
    end_ns: u64,
    gap_ns: [u64; 2],
    down_ns: [u64; 2],
}
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Event {
    at_ns: u64,
    machine: Option<String>,
    serial: Option<String>,
    partition: Option<[String; 2]>,
    on: Option<bool>,
}
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct World {
    kernel: PathBuf,
    mem_mb: u32,
    duration_ns: u64,
    end_on_exit: String,
    machines: Vec<Machine>,
    network: Option<Link>,
    #[serde(default)]
    timer_jitter_ns: u64,
    #[serde(default)]
    stop_tolerance_ns: u64,
    #[serde(default)]
    exact_timer_ppm: u64,
    #[serde(default)]
    scenario: Vec<Event>,
    kill_restart: Option<Kills>,
}
fn decode(bytes: &[u8]) -> Result<World> {
    let world: World = serde_json::from_slice(bytes)?;
    let names: std::collections::BTreeSet<_> = world.machines.iter().map(|m| &m.name).collect();
    if world.machines.is_empty()
        || world.machines.len() > 254
        || names.len() != world.machines.len()
        || !names.contains(&world.end_on_exit)
        || world.duration_ns == 0
        || !(64..=65536).contains(&world.mem_mb)
        || names.iter().any(|n| {
            n.is_empty()
                || !n
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
        })
        || world.machines.iter().any(|m| {
            m.initrd.as_os_str().is_empty() || m.cmdline.as_ref().is_some_and(|c| c.contains('\0'))
        })
        || world.exact_timer_ppm > 1_000_000
    {
        return Err(Error::Execution(
            "invalid fixture machines, workload, memory, or limits".into(),
        ));
    }
    if let Some(link) = &world.network {
        if link.latency_ns == 0
            || link.latency_ns.checked_add(link.jitter_ns).is_none()
            || link.jitter_ns == u64::MAX
        {
            return Err(Error::Execution("invalid fixture network delay".into()));
        }
    }
    for ev in &world.scenario {
        let serial = ev.machine.as_ref().zip(ev.serial.as_ref());
        if ev.at_ns > world.duration_ns
            || serial.is_some() == ev.partition.is_some()
            || (ev.machine.is_some() != ev.serial.is_some())
            || serial.is_some_and(|(n, _)| !names.contains(n))
            || ev
                .partition
                .as_ref()
                .is_some_and(|p| p.iter().any(|n| !names.contains(n)))
            || (ev.on.is_some() && ev.partition.is_none())
        {
            return Err(Error::Execution(
                "invalid fixture scenario target or action".into(),
            ));
        }
    }
    if let Some(k) = &world.kill_restart {
        if k.members.is_empty()
            || k.members.iter().any(|n| !names.contains(n))
            || k.start_ns >= k.end_ns
            || k.end_ns > world.duration_ns
            || k.gap_ns[0] == 0
            || k.gap_ns[0] > k.gap_ns[1]
            || k.down_ns[0] > k.down_ns[1]
            || k.end_ns
                .checked_add(k.gap_ns[1])
                .and_then(|v| v.checked_add(k.down_ns[1]))
                .is_none()
        {
            return Err(Error::Execution(
                "invalid fixture kill/restart schedule".into(),
            ));
        }
    }
    Ok(world)
}

#[derive(Clone)]
pub struct Fixture {
    pub simulation: Simulation,
    pub memory_mb: u32,
    definition: Vec<u8>,
    kernel: PathBuf,
    guests: BTreeMap<String, PathBuf>,
}
impl Fixture {
    pub fn load(path: &Path, artifacts: Option<&Path>, wall_time_ns: u64) -> Result<Self> {
        let source = path.canonicalize()?;
        let definition = fs::read(&source)?;
        let world = decode(&definition)?;
        if wall_time_ns == 0 {
            return Err(Error::Execution("wall limit must be positive".into()));
        }
        let resolve = |p: &Path| -> Result<PathBuf> {
            Ok(match artifacts {
                Some(base) => base.join(
                    p.file_name()
                        .ok_or_else(|| Error::Execution("fixture input has no filename".into()))?,
                ),
                None => source.parent().unwrap().join(p),
            }
            .canonicalize()?)
        };
        let kernel = resolve(&world.kernel)?;
        let guests = world
            .machines
            .iter()
            .map(|m| Ok((m.name.clone(), resolve(&m.initrd)?)))
            .collect::<Result<BTreeMap<_, _>>>()?;
        let services = guests
            .iter()
            .map(|(name, path)| {
                (
                    name.clone(),
                    Service {
                        source: ImageSource::BootArchive { path: path.clone() },
                        platform: "linux/amd64".into(),
                        entrypoint: None,
                        command: None,
                        environment: BTreeMap::new(),
                        working_dir: None,
                        user: None,
                        configs: vec![],
                        network: "fixture".into(),
                    },
                )
            })
            .collect();
        let network = Network {
            name: "fixture".into(),
            latency_ns: world.network.as_ref().map_or(1_000_000, |n| n.latency_ns),
            jitter_ns: world.network.as_ref().map_or(0, |n| n.jitter_ns),
        };
        let simulation = Simulation {
            name: source.file_stem().unwrap().to_string_lossy().into(),
            source,
            services,
            configs: BTreeMap::new(),
            network,
            schedule: Schedule {
                timer_jitter_ns: world.timer_jitter_ns,
                stop_tolerance_ns: world.stop_tolerance_ns,
            },
            limits: Limits {
                simulated_time_ns: world.duration_ns,
                wall_time_ns,
            },
            check: Check {
                service: world.end_on_exit,
                exit_code: 0,
            },
            seed: None,
        };
        Ok(Self {
            simulation,
            memory_mb: world.mem_mb,
            definition,
            kernel,
            guests,
        })
    }
    pub fn prepare(&self, profile: ExecutionProfile, store: &ContentStore) -> Result<Frozen> {
        if profile
            .semantic_parameters
            .get("guest_abi")
            .map(String::as_str)
            != Some(ABI)
            || profile.semantic_parameters.get("memory_mb") != Some(&self.memory_mb.to_string())
        {
            return Err(Error::Integrity(
                "fixture execution profile mismatch".into(),
            ));
        }
        let kernel = store.put_file(&self.kernel)?;
        let guests = self
            .guests
            .iter()
            .map(|(n, p)| Ok((n.clone(), store.put_file(p)?)))
            .collect::<Result<BTreeMap<_, _>>>()?;
        let definition = store.put(&self.definition)?;
        let mut identity = BTreeMap::from([
            ("world".into(), definition.clone()),
            ("kernel".into(), kernel.clone()),
        ]);
        for (n, p) in &self.guests {
            let key = format!("initrd:{}", p.file_name().unwrap().to_string_lossy());
            if identity
                .insert(key, guests[n].clone())
                .is_some_and(|v| v != guests[n])
            {
                return Err(Error::Integrity(
                    "ambiguous fixture archive filenames".into(),
                ));
            }
        }
        Ok(Frozen {
            simulation: self.simulation.clone(),
            inputs: PreparedInputs {
                kernel,
                images: BTreeMap::new(),
                placement: guests.keys().map(|n| (n.clone(), n.clone())).collect(),
                guests,
                profile,
                auxiliary: BTreeMap::from([(DEFINITION.into(), definition)]),
                provenance: BTreeMap::from([(
                    "fixture_input_identity".into(),
                    serde_json::to_string(&identity)?,
                )]),
            },
        })
    }
}
#[derive(Clone)]
pub struct Frozen {
    pub simulation: Simulation,
    pub inputs: PreparedInputs,
}
impl PreparationBackend for Frozen {
    fn prepare(&self, simulation: &Simulation, _: &ContentStore) -> Result<PreparedInputs> {
        if simulation != &self.simulation {
            return Err(Error::Integrity(
                "frozen fixture configuration changed".into(),
            ));
        }
        Ok(self.inputs.clone())
    }
}

pub fn world(request: &ExecutionRequest<'_>) -> Result<Value> {
    let hash = request
        .prepared
        .auxiliary
        .get(DEFINITION)
        .ok_or_else(|| Error::Integrity("missing fixture definition".into()))?;
    let path = request
        .artifacts
        .get(hash)
        .ok_or_else(|| Error::Integrity("missing verified fixture".into()))?;
    let bytes = fs::read(path)?;
    let original = decode(&bytes)?;
    if original.duration_ns != request.limits.simulated_time_ns
        || original.end_on_exit != request.check.service
        || original.mem_mb.to_string() != request.prepared.profile.semantic_parameters["memory_mb"]
    {
        return Err(Error::Integrity("fixture semantic settings changed".into()));
    }
    let mut value: Value = serde_json::from_slice(&bytes)?;
    let artifact = |hash: &ContentDigest| -> Result<PathBuf> {
        Ok(std::path::absolute(
            request
                .artifacts
                .get(hash)
                .ok_or_else(|| Error::Integrity("missing fixture input".into()))?,
        )?)
    };
    value["kernel"] = json!(artifact(&request.prepared.kernel)?);
    for m in value["machines"].as_array_mut().unwrap() {
        let name = m["name"].as_str().unwrap();
        let hash = request
            .prepared
            .guests
            .get(name)
            .ok_or_else(|| Error::Integrity("fixture placement changed".into()))?;
        m["initrd"] = json!(artifact(hash)?);
    }
    Ok(value)
}

pub fn observe(report: &mut ExecutionReport, request: &ExecutionRequest<'_>) -> Result<()> {
    let Some(evidence) = &mut report.diagnostics else {
        return Err(Error::Integrity(
            "fixture report lacks machine evidence".into(),
        ));
    };
    let mut cursors = BTreeMap::<String, usize>::new();
    for fault in &mut evidence.legacy_faults {
        if !request.prepared.guests.contains_key(&fault.member) || fault.start_ns < fault.stop_ns {
            return Err(Error::Integrity(
                "invalid reported fixture fault schedule".into(),
            ));
        }
        let text = read_console(
            &request
                .output
                .join("machines")
                .join(format!("{}.mgmt", fault.member)),
        )?;
        let cursor = cursors.entry(fault.member.clone()).or_default();
        (fault.stopped, fault.restarted) = acknowledge(&text, &fault.member, cursor);
    }
    if request.check.service == "client" && report.reason == StopReason::Completed {
        let text = read_console(&request.output.join("machines/client.console"))?;
        let lines: Vec<_> = text
            .lines()
            .filter(|l| {
                matches!(
                    l.split_whitespace().take(2).collect::<Vec<_>>().as_slice(),
                    ["RESULT", "pass"] | ["RESULT", "fail"]
                )
            })
            .collect();
        if lines.len() != 1
            || report.workload_exit.as_ref().is_none_or(|exit| {
                exit.code != u8::from(lines[0].split_whitespace().nth(1) == Some("fail"))
            })
        {
            report.reason = StopReason::Error;
            report.detail = Some(
                "legacy checker requires exactly one RESULT pass/fail matching its exit status"
                    .into(),
            );
        } else {
            evidence.checker_result = Some(lines[0].into());
        }
    }
    Ok(())
}

pub fn read_console(path: &Path) -> Result<String> {
    use std::io::Read;
    let mut bytes = Vec::new();
    fs::File::open(path)?
        .take(8 * 1024 * 1024 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > 8 * 1024 * 1024 {
        return Err(Error::Execution("acceptance console exceeds 8 MiB".into()));
    }
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

// Consume a stop and its following start in order. Initial boot messages or
// duplicate acknowledgements cannot satisfy a later scheduled restart.
fn acknowledge(text: &str, member: &str, cursor: &mut usize) -> (bool, bool) {
    let lines: Vec<_> = text.lines().collect();
    let stop = format!("{member} stopped");
    let start = format!("{member} started");
    let Some(at) = lines
        .iter()
        .enumerate()
        .skip(*cursor)
        .find_map(|(i, l)| (*l == stop).then_some(i))
    else {
        *cursor = lines.len();
        return (false, false);
    };
    *cursor = at + 1;
    for (i, line) in lines.iter().enumerate().skip(*cursor) {
        if *line == stop {
            return (true, false);
        }
        if *line == start {
            *cursor = i + 1;
            return (true, true);
        }
    }
    *cursor = lines.len();
    (true, false)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn recovery_requires_ordered_distinct_acknowledgements() {
        let mut cursor = 0;
        let log = "n1 started\nn1 stopped\nn1 started\nn1 stopped\n";
        assert_eq!(acknowledge(log, "n1", &mut cursor), (true, true));
        assert_eq!(acknowledge(log, "n1", &mut cursor), (true, false));
        assert_eq!(acknowledge(log, "n1", &mut cursor), (false, false));
        assert_eq!(
            acknowledge("n1 started\nn1 stopped", "n1", &mut 0),
            (true, false)
        );
    }
}
