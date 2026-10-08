//! ARM application admission and retained report projection over the shared World.
use super::{
    worker::Boot,
    world_adapter::{ArmMachine, Factory},
};
use crate::{
    control::Control, faults::Faults, links::Links, network_counters::NetworkCounters, Result,
};
use seedbox_engine_core::world::{self as world_core, Payload, WorldMachine};
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};

pub const CONTRACT: &str = "arm64-world-v1";
pub fn entropy_seed(seed: u64, domain: &str) -> u64 {
    crate::rng::Rng::new(seed, domain).next()
}
pub struct Options {
    pub executable: PathBuf,
    pub config: PathBuf,
    pub output: PathBuf,
    pub seed: u64,
    pub full_state: bool,
    pub host_control: Option<PathBuf>,
    pub console_limit: usize,
}
#[derive(Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
struct Config {
    kernel: PathBuf,
    mem_mb: u64,
    duration_ns: u64,
    end_on_exit: String,
    timer_jitter_ns: u64,
    stop_tolerance_ns: u64,
    network: Value,
    machines: Vec<MachineConfig>,
    control: Value,
    control_channel: String,
    #[serde(default)]
    disk_loss: Value,
}
#[derive(Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
struct MachineConfig {
    name: String,
    initrd: PathBuf,
    #[serde(default)]
    disk: Option<DiskConfig>,
}
#[derive(Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
struct DiskConfig {
    #[serde(default)]
    size_mib: Option<u64>,
    #[serde(default)]
    image: Option<PathBuf>,
}

pub fn run(options: &Options) -> Result<Value> {
    let cfg: Config = serde_json::from_slice(
        &std::fs::read(&options.config)
            .map_err(|e| format!("world {}: {e}", options.config.display()))?,
    )
    .map_err(|e| format!("world config: {e}"))?;
    if cfg.machines.is_empty() || cfg.machines.len() > 254 {
        return Err("world requires 1..254 machines".into());
    }
    if !(64..=3072).contains(&cfg.mem_mb) || cfg.duration_ns == 0 || cfg.duration_ns == u64::MAX {
        return Err("invalid ARM memory or duration".into());
    }
    if cfg.timer_jitter_ns > 3_600_000_000_000 {
        return Err("ARM application timer jitter exceeds one hour".into());
    }
    // PV stops are exact; the tolerance remains a retained schedule upper bound.
    if cfg.stop_tolerance_ns > 3_600_000_000_000 {
        return Err("invalid stop tolerance".into());
    }
    if cfg.control_channel != "virtio-console-v1" {
        return Err("unknown application control channel".into());
    }
    let names: Vec<_> = cfg.machines.iter().map(|m| m.name.clone()).collect();
    let unique: BTreeSet<_> = names.iter().collect();
    if unique.len() != names.len()
        || !names.contains(&cfg.end_on_exit)
        || names.iter().any(|n| {
            n.is_empty()
                || !n
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        })
    {
        return Err("invalid world machine names or workload".into());
    }
    let links = Links::from_config(options.seed, &names, &cfg.network)?;
    let lookahead = links
        .global
        .latency_ns
        .checked_sub(links.global.jitter_ns)
        .filter(|n| *n > 0)
        .ok_or("network lookahead must be positive")?;
    let control = Control::new(&cfg.control, names.clone(), lookahead, 0)?;
    let mut faults = Faults::with_links(Vec::new(), names.clone(), links)?;
    faults.require_readiness();
    let disk_fault_model = crate::disk::parse_disk_fault_model(&cfg.disk_loss)?;
    let base = options.config.parent().unwrap_or(Path::new("."));
    std::fs::create_dir_all(&options.output).map_err(|e| format!("world output: {e}"))?;
    let factory = Factory::new(
        options.executable.clone(),
        options.output.clone(),
        &names,
        options.console_limit,
        options.full_state,
    )?;
    let mut machines = Vec::new();
    let mut boots = Vec::new();
    let mut macs = Vec::new();
    for (i, m) in cfg.machines.iter().enumerate() {
        let mac = [2, 0, 0, 0, 0, (i + 1) as u8];
        macs.push(mac);
        let mut entropy = crate::rng::Rng::new(options.seed, &format!("boot-entropy/{}", m.name));
        let mut command_line = crate::boot::DEFAULT_COMMAND_LINE.to_owned();
        if control.controller == Some(i) {
            command_line.push_str(" seedbox.control=1");
        }
        let disk_sectors = m
            .disk
            .as_ref()
            .and_then(|d| d.size_mib)
            .map(|size| {
                size.checked_mul(2048)
                    .filter(|_| size > 0 && size <= 1 << 20)
                    .ok_or("invalid disk size")
            })
            .transpose()?;
        let boot = Boot {
            kernel: base.join(&cfg.kernel),
            initrd: base.join(&m.initrd),
            seed: entropy.next(),
            memory_bytes: cfg.mem_mb * 1024 * 1024,
            command_line,
            mac,
            disk_image: m
                .disk
                .as_ref()
                .and_then(|d| d.image.as_ref())
                .map(|p| base.join(p)),
            disk_sectors,
            disk_fault_model,
            timer_policy: Some(super::TimerPolicy {
                seed: options.seed,
                domain: format!("timer-jitter/{}", m.name),
                jitter_ns: cfg.timer_jitter_ns,
            }),
        };
        machines.push(ArmMachine::boot(
            &factory,
            &m.name,
            &boot,
            false,
            world_core::entropy_seed(options.seed, &format!("boot-entropy/{}", m.name)),
        )?);
        boots.push(boot);
    }
    let transcript = factory.transcript.clone();
    let mut world = world_core::World {
        machines,
        factory,
        boot: boots,
        barrier: false,
        boot_seed: options.seed,
        run_seed: options.seed,
        incarnations: names.iter().map(|_| Vec::new()).collect(),
        paused_ingress: vec![0; names.len()],
        clock: (),
        events: names.iter().map(|_| BTreeMap::new()).collect(),
        strict_events: names.iter().map(|_| BTreeMap::new()).collect(),
        seq: 0,
        lookahead,
        early: 0,
        latency: lookahead,
        partitioned: BTreeMap::new(),
        faults,
        control,
        macs,
        dropped: 0,
        delivered: 0,
        link_counts: BTreeMap::new(),
        semantic_counts: NetworkCounters::default(),
        slices: 0,
        failure: None,
    };
    let mut host = options
        .host_control
        .as_deref()
        .map(crate::host_control::HostControl::bind)
        .transpose()
        .map_err(|e| format!("host control: {e}"))?;
    let raw_config = serde_json::to_value(&cfg).map_err(|e| e.to_string())?;
    let outcome = world_core::simulate(
        &mut world,
        &raw_config,
        cfg.duration_ns,
        &Some(cfg.end_on_exit.clone()),
        None,
        &mut || {
            if let Some(host) = &mut host {
                host.boundary().map_err(|e| format!("host control: {e}"))?;
            }
            Ok(())
        },
    )?;
    let cutoff = if outcome == "duration" {
        cfg.duration_ns
    } else {
        world.machines.iter().map(|m| m.time).max().unwrap_or(0)
    };
    world.control.cut_off(&world.faults, cutoff);
    let mut hash = Sha256::new_with_prefix(b"seedbox-arm64-world-witness-v1\0");
    let horizons = format!("{:x}", transcript.borrow().horizons.clone().finalize());
    hash.update(horizons.as_bytes());
    hash.update(transcript.borrow().events.clone().finalize());
    let mut rows = Vec::new();
    for m in &mut world.machines {
        let evidence = m.evidence()?;
        hash.update(evidence.witness.as_bytes());
        if let Some(state) = &evidence.state_digest {
            hash.update(state.as_bytes());
        }
        hash.update(serde_json::to_vec(&m.crashed).map_err(|e| e.to_string())?);
        // Down rows describe retained predecessor evidence. The coordinator
        // still owns the successor's down state at the crash boundary.
        let (incarnation, booted_at) = m
            .retired
            .as_ref()
            .map(|r| (r.incarnation, r.booted_at))
            .unwrap_or((m.incarnation, m.booted_at));
        let exit = m.exit();
        let state = if m.down {
            "Down".into()
        } else if m.paused {
            "Paused".into()
        } else {
            exit.map(|(code, _)| format!("Exited({code})"))
                .unwrap_or_else(|| "Running".into())
        };
        rows.push(json!({"name":m.name,"state":state,"time_ns":evidence.time_ns,
            "guest_exit_code":exit.filter(|(_,setup)|!*setup).map(|(code,_)|code),
            "guest_init_error":exit.filter(|(_,setup)|*setup).map(|(code,_)|code),
            "infrastructure_error":exit.is_some_and(|(_,setup)|setup),
            "witness":evidence.witness,"state_digest":evidence.state_digest,
            "state_digest_parts":evidence.state_digest_parts,"control_channel":evidence.control_channel,
            "booted_at_ns":booted_at,"application_evidence":evidence,
            "incarnation":incarnation,"crashed_incarnations":m.crashed}));
    }
    let mut network = Sha256::new();
    world.faults.network_witness(&mut network);
    for (to, events) in world.events.iter().enumerate() {
        let mut retained = Vec::new();
        for (&key, payload) in events {
            match payload {
                Payload::Frame { from, bytes } => {
                    retained.push((key, Some(*from), bytes));
                }
                Payload::Serial(_) | Payload::Partition { .. } => {
                    return Err("unsupported ARM world event in retained network state".into())
                }
            }
        }
        for (&key, bytes) in &world.strict_events[to] {
            retained.push((key, None, bytes));
        }
        // Preserve the ARM witness's per-machine event sequence even though
        // the core stores strict management and network traffic separately.
        retained.sort_by_key(|(key, _, _)| *key);
        for ((at, seq), from, bytes) in retained {
            network.update((to as u64).to_le_bytes());
            network.update(at.to_le_bytes());
            network.update(seq.to_le_bytes());
            if let Some(from) = from {
                network.update([0]);
                network.update((from as u64).to_le_bytes());
            } else {
                network.update([1]);
            }
            network.update(Sha256::digest(bytes));
        }
    }
    let network = format!("{:x}", network.finalize());
    hash.update(network.as_bytes());
    let counts: Vec<_> = world.link_counts.iter().collect();
    hash.update(
        serde_json::to_vec(&(&world.faults.log, &world.control.log, counts))
            .map_err(|e| e.to_string())?,
    );
    world.control.triggers.hash_into(&mut hash)?;
    let trigger_evidence = world.control.triggers.evidence()?;
    let counters = arm_counters(&world);
    let mut report = json!({"report_version":2,"seed":options.seed,"timebase":"arm64-pv-world-v1",
        "outcome":outcome,"world_witness":format!("{:x}",hash.finalize()),
        "network_witness":network,"horizon_witness":horizons,"machines":rows,
        "frames_delivered":world.delivered,"frames_dropped":world.dropped,"network_counters":counters,
        "faults":world.faults.log,"fault_error":world.faults.error.or(world.control.error),
        "control_history":world.control.log,"control_latency_ns":world.control.latency,
        "rejected_fault_effects":world.faults.rejected_effects,"management_readiness":world.faults.readiness,
        "slices":world.slices});
    if let Some(evidence) = trigger_evidence {
        report["triggers"] = evidence;
    }
    Ok(report)
}
fn arm_counters(world: &world_core::World<ArmMachine>) -> Value {
    let counts: BTreeMap<_,_> = world.link_counts.iter().map(|(&(from,to),c)|
        (format!("{}->{}",world.machines[from].name,world.machines[to].name),
         json!({"delivered":c[0],"partition_dropped":c[1],"receiver_dropped":c[2],"loss_dropped":c[3]}))).collect();
    json!(counts)
}
