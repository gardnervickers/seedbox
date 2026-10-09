//! ARM application admission and retained report projection over the shared World.
use super::{
    worker::Boot,
    world_adapter::{ArmMachine, Factory},
};
use crate::{
    control::Control, faults::Faults, links::Links, network_counters::NetworkCounters, Result,
};
use seedbox_engine_core::{
    world::{self as world_core, Payload, WorldMachine},
    world_windowed,
};
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    time::Duration,
};

pub const CONTRACT: &str = "arm64-world-v1";
pub const LAUNCH_POLICY: &str = "seeded-barrier-v1";
pub const MAX_LAUNCH_SKEW_NS: u64 = 1_000_000;
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
    /// Machine threads under the windowed scheduler; a performance setting
    /// that never changes results. 1 runs every machine on this thread.
    pub machine_workers: usize,
}

/// One segment of the run under the windowed scheduler. Every machine is
/// its own stopped worker process, so the pool threads need no clock: the
/// adapter's clock is `()`.
fn simulate_windowed(
    world: &mut world_core::World<ArmMachine>,
    run: &world_windowed::Run,
    machine_workers: usize,
    boundary: &mut dyn FnMut() -> Result<()>,
) -> Result<(&'static str, Value)> {
    if machine_workers <= 1 {
        return world_windowed::simulate(world, run, Duration::ZERO, boundary)
            .map(|(outcome, scheduler)| (outcome.as_str(), scheduler));
    }
    world_windowed::simulate_parallel(
        world,
        run,
        world_windowed::Workers {
            count: machine_workers,
            skew: Duration::ZERO,
        },
        |_| Ok(()),
        boundary,
    )
    .map(|(outcome, scheduler)| (outcome.as_str(), scheduler))
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    launch_skew_ns: Option<u64>,
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
    validate_launch_skew(cfg.launch_skew_ns, lookahead)?;
    let initial_barrier = cfg.launch_skew_ns.is_some();
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
            initial_barrier,
            world_core::entropy_seed(options.seed, &format!("boot-entropy/{}", m.name)),
        )?);
        boots.push(boot);
    }
    let mut world = world_core::World {
        machines,
        factory,
        boot: boots,
        barrier: initial_barrier,
        boot_seed: options.seed,
        run_seed: options.seed,
        branch_identity: None,
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
    let end_on = Some(cfg.end_on_exit.clone());
    let mut boundary = || {
        if let Some(host) = &mut host {
            host.boundary().map_err(|e| format!("host control: {e}"))?;
        }
        Ok(())
    };
    let mut initial_launch = cfg.launch_skew_ns.map(|skew| {
        json!({
            "policy": LAUNCH_POLICY, "max_skew_ns": skew, "released": false,
            "barrier_ns": null, "release_ns": null, "release_times": {}
        })
    });
    // Boot initial guests to their launch barriers without converting an
    // ordinary duration or end-machine exit into a failed fork.
    let bootstrap = if initial_barrier {
        let launch = world_windowed::Run {
            duration: cfg.duration_ns,
            end_on: end_on.as_deref(),
            fork: Some(world_windowed::Fork::Barrier),
            checkpoint: None,
            require_barrier: false,
        };
        Some(simulate_windowed(
            &mut world,
            &launch,
            options.machine_workers,
            &mut boundary,
        )?)
    } else {
        None
    };
    let main = world_windowed::Run {
        duration: cfg.duration_ns,
        end_on: end_on.as_deref(),
        fork: None,
        checkpoint: None,
        require_barrier: true,
    };
    let (outcome, scheduler) = if let Some(stopped) = bootstrap.filter(|(s, _)| *s != "barrier") {
        stopped
    } else if let Some(skew) = cfg.launch_skew_ns {
        let at = world.machines.iter().map(|m| m.time).max().unwrap_or(0);
        if let Some(launch) = &mut initial_launch {
            launch["barrier_ns"] = json!(at);
        }
        if let Some(release_at) = launch_release_bound(&raw_config, at, skew, cfg.duration_ns)? {
            validate_pending_launch(&world, release_at)?;
            let (_, reseed) = world_core::release(
                &mut world,
                &raw_config,
                &names,
                options.seed,
                skew,
                &mut |_, _| {},
            )?;
            if names.iter().any(|name| reseed[name] != "barrier") {
                return Err("ARM initial launch did not reseed through the launch barrier".into());
            }
            let release_times: BTreeMap<_, _> = world
                .machines
                .iter()
                .map(|m| (m.name.clone(), m.time))
                .collect();
            if world
                .machines
                .iter()
                .any(|m| m.parked || m.time < at || m.time > release_at)
            {
                return Err("ARM initial launch acknowledgment outside release bounds".into());
            }
            initial_launch = Some(json!({
                "policy": LAUNCH_POLICY, "max_skew_ns": skew, "released": true,
                "barrier_ns": at, "release_ns": release_at, "release_times": release_times
            }));
            simulate_windowed(&mut world, &main, options.machine_workers, &mut boundary)?
        } else {
            // The duration is absolute: never move a release or extend a run
            // to make room for initial launch, including the exact endpoint.
            ("duration", Value::Null)
        }
    } else {
        simulate_windowed(&mut world, &main, options.machine_workers, &mut boundary)?
    };
    world.finish(outcome, cfg.duration_ns, &Some(cfg.end_on_exit.clone()))?;
    let mut hash = Sha256::new_with_prefix(b"seedbox-arm64-world-witness-v2\0");
    // Each machine's transcript in machine order: independent of the thread
    // that ran it and of round timing.
    let mut horizon_hash = Sha256::new_with_prefix(b"seedbox-arm64-world-horizons-v2\0");
    let mut event_hash = Sha256::new_with_prefix(b"seedbox-arm64-world-events-v2\0");
    for m in &world.machines {
        let transcript = m.transcript();
        horizon_hash.update(transcript.horizons.clone().finalize());
        event_hash.update(transcript.events.clone().finalize());
    }
    let horizons = format!("{:x}", horizon_hash.finalize());
    hash.update(horizons.as_bytes());
    hash.update(event_hash.finalize());
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
    let counts = witness_counts(&world.link_counts);
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
    if let Some(launch) = initial_launch {
        report["initial_launch"] = launch;
    }
    if !scheduler.is_null() {
        report["scheduler"] = scheduler;
    }
    if let Some(evidence) = trigger_evidence {
        report["triggers"] = evidence;
    }
    Ok(report)
}
/// Queue drops are appended only when present, so a run without them keeps
/// the four-counter witness bytes.
fn witness_counts(counts: &BTreeMap<(usize, usize), [u64; 5]>) -> Vec<(&(usize, usize), &[u64])> {
    counts
        .iter()
        .map(|(pair, c)| (pair, if c[4] == 0 { &c[..4] } else { &c[..] }))
        .collect()
}
fn arm_counters(world: &world_core::World<ArmMachine>) -> Value {
    let counts: BTreeMap<_,_> = world.link_counts.iter().map(|(&(from,to),c)|
        (format!("{}->{}",world.machines[from].name,world.machines[to].name),
         {
             let mut row = json!({"delivered":c[0],"partition_dropped":c[1],"receiver_dropped":c[2],"loss_dropped":c[3]});
             if c[4] > 0 { row["queue_dropped"] = json!(c[4]); }
             row
         })).collect();
    json!(counts)
}

fn validate_launch_skew(skew: Option<u64>, lookahead: u64) -> Result<()> {
    if skew.is_some_and(|skew| skew > lookahead) {
        return Err("ARM launch_skew_ns exceeds network lookahead".into());
    }
    Ok(())
}

fn launch_release_bound(cfg: &Value, at: u64, skew: u64, duration: u64) -> Result<Option<u64>> {
    let release = at
        .checked_add(skew)
        .ok_or("ARM initial launch time overflow")?;
    if release >= duration {
        return Ok(None);
    }
    world_core::check_plan_after(cfg, release)?;
    Ok(Some(release))
}

fn validate_pending_launch<M: WorldMachine>(
    world: &world_core::World<M>,
    release: u64,
) -> Result<()> {
    // No guest enters during release. Since skew <= lookahead, every new
    // packet, RPC or console-trigger effect is due at or after the latest
    // release: sender time + lookahead >= barrier time + maximum skew.
    // Already sampled bootstrap effects must satisfy that boundary too.
    let pending = world
        .events
        .iter()
        .filter_map(|q| q.keys().next().map(|k| k.0))
        .chain(
            world
                .strict_events
                .iter()
                .filter_map(|q| q.keys().next().map(|k| k.0)),
        )
        .chain([
            world.control.boundary(),
            world.control.triggers.boundary(),
            world.faults.next_boundary(),
        ]);
    if pending.into_iter().any(|at| at < release) {
        return Err("ARM bootstrap effect is before the initial launch release bound".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> Value {
        json!({"kernel":"kernel", "mem_mb":64, "duration_ns":100,
            "end_on_exit":"a", "timer_jitter_ns":0, "stop_tolerance_ns":0,
            "network":{"latency_ns":10}, "machines":[{"name":"a","initrd":"initrd"}],
            "control":null, "control_channel":"virtio-console-v1"})
    }

    #[test]
    fn link_counts_without_queue_drops_keep_the_four_counter_witness_bytes() {
        let counts = BTreeMap::from([((0, 1), [1, 2, 3, 4, 0]), ((1, 0), [5, 0, 0, 0, 6])]);
        assert_eq!(
            serde_json::to_vec(&witness_counts(&counts)).unwrap(),
            br#"[[[0,1],[1,2,3,4]],[[1,0],[5,0,0,0,6]]]"#
        );
    }

    #[test]
    fn absent_launch_policy_preserves_legacy_config_shape_and_zero_is_explicit() {
        let mut raw = config();
        let cfg: Config = serde_json::from_value(raw.clone()).unwrap();
        assert_eq!(cfg.launch_skew_ns, None);
        assert!(serde_json::to_value(cfg)
            .unwrap()
            .get("launch_skew_ns")
            .is_none());
        raw["launch_skew_ns"] = json!(0);
        let cfg: Config = serde_json::from_value(raw).unwrap();
        assert_eq!(cfg.launch_skew_ns, Some(0));
        assert_eq!(serde_json::to_value(cfg).unwrap()["launch_skew_ns"], 0);
    }

    #[test]
    fn launch_bound_preserves_causality_without_a_fixed_cap_gate() {
        assert!(validate_launch_skew(None, 10).is_ok());
        assert!(validate_launch_skew(Some(0), 10).is_ok());
        assert!(validate_launch_skew(Some(10), 10).is_ok());
        assert!(validate_launch_skew(Some(11), 10).is_err());
        assert!(validate_launch_skew(Some(MAX_LAUNCH_SKEW_NS), u64::MAX).is_ok());
        assert!(validate_launch_skew(Some(MAX_LAUNCH_SKEW_NS + 1), u64::MAX).is_ok());
        for value in [json!(-1), json!(1.5), json!("10")] {
            let mut raw = config();
            raw["launch_skew_ns"] = value;
            assert!(serde_json::from_value::<Config>(raw).is_err());
        }
    }

    #[test]
    fn initial_release_never_uses_or_extends_the_absolute_deadline() {
        assert_eq!(
            launch_release_bound(&Value::Null, 100, 10, 111).unwrap(),
            Some(110)
        );
        assert_eq!(
            launch_release_bound(&Value::Null, 100, 10, 110).unwrap(),
            None
        );
        assert_eq!(
            launch_release_bound(&Value::Null, 100, 10, 109).unwrap(),
            None
        );
        assert_eq!(
            launch_release_bound(&Value::Null, 100, 0, 100).unwrap(),
            None
        );
    }

    #[test]
    fn release_admission_rejects_earlier_actions_without_shifting_them() {
        for key in ["faults", "scenario"] {
            let mut raw = json!({});
            raw[key] = json!([{"at_ns":109}]);
            assert!(launch_release_bound(&raw, 100, 10, 1000)
                .unwrap_err()
                .contains("109"));
            assert_eq!(raw[key][0]["at_ns"], 109);
            raw[key][0]["at_ns"] = json!(110);
            assert_eq!(
                launch_release_bound(&raw, 100, 10, 1000).unwrap(),
                Some(110)
            );
        }
        assert!(
            launch_release_bound(&json!({"kill_restart":{"start_ns":109}}), 100, 10, 1000).is_err()
        );
        assert!(launch_release_bound(&Value::Null, u64::MAX, 1, u64::MAX).is_err());
        assert_eq!(
            launch_release_bound(&Value::Null, 100, 0, 1000).unwrap(),
            Some(100)
        );
    }
}
