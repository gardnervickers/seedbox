//! Deterministic world ownership over one HVF worker per machine. Host process
//! scheduling cannot advance guest clocks: every entry has an explicit horizon.
use super::{
    worker::{Answer, Boot, Operation, OwnedWorker},
    Event, Evidence,
};
use crate::{
    control::{Control, ManagementFramer},
    faults::{Faults, Life, Lifecycle},
    links::{self, Links},
    machine::MachineStop,
    network_counters::NetworkCounters,
    Result,
};
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    io::Write,
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
#[derive(Deserialize)]
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
    disk_loss: Option<String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MachineConfig {
    name: String,
    initrd: PathBuf,
    #[serde(default)]
    disk: Option<DiskConfig>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DiskConfig {
    #[serde(default)]
    size_mib: Option<u64>,
    #[serde(default)]
    image: Option<PathBuf>,
}

struct Machine {
    name: String,
    worker: OwnedWorker,
    boot: Boot,
    time: u64,
    exit: Option<(u8, bool)>,
    incarnation: u32,
    booted_at_ns: u64,
    crashed: Vec<Value>,
    console: Capture<std::fs::File>,
    management: Capture<std::fs::File>,
    framer: ManagementFramer,
    down_evidence: Option<Evidence>,
}
// Capture limits bound retained files only. Device framing and witness hashing
// always consume the complete stream, including bytes beyond this limit.
struct Capture<W> {
    writer: W,
    remaining: usize,
}
impl<W: Write> Capture<W> {
    fn write_all(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        let length = bytes.len().min(self.remaining);
        self.writer.write_all(&bytes[..length])?;
        self.remaining -= length;
        Ok(())
    }
}
enum Payload {
    Frame { from: usize, bytes: Vec<u8> },
    Control(Vec<u8>),
}
struct World {
    machines: Vec<Machine>,
    faults: Faults,
    control: Control,
    events: Vec<BTreeMap<(u64, u64), Payload>>,
    sequence: u64,
    lookahead: u64,
    delivered: u64,
    dropped: u64,
    counts: BTreeMap<(usize, usize), [u64; 4]>,
    nominal: NetworkCounters,
    macs: Vec<[u8; 6]>,
    seed: u64,
    slices: u64,
    horizon_hash: Sha256,
    event_hash: Sha256,
    full_state: bool,
}
impl World {
    fn active(&self, m: usize) -> bool {
        self.machines[m].exit.is_none() && self.faults.life(m) == Life::Up
    }
    fn key(&self, m: usize) -> u64 {
        if self.active(m) {
            self.machines[m].time
        } else {
            u64::MAX
        }
    }
    fn schedule(&mut self, m: usize, at: u64, payload: Payload) -> Result<()> {
        if self.active(m) && at < self.machines[m].time {
            return Err("world delivery is in machine's past".into());
        }
        self.sequence = self
            .sequence
            .checked_add(1)
            .ok_or("world event sequence overflow")?;
        self.events[m].insert((at, self.sequence), payload);
        Ok(())
    }
    fn count(&mut self, at: u64, from: usize, to: usize, outcome: usize) {
        self.counts.entry((from, to)).or_default()[outcome] += 1;
        self.nominal.record(at, from, to, outcome);
        if outcome == 0 {
            self.delivered += 1;
        } else {
            self.dropped += 1;
        }
    }
    fn counters(&self, counts: &BTreeMap<(usize, usize), [u64; 4]>) -> Value {
        let counts:BTreeMap<_,_>=counts.iter().map(|(&(from,to),c)|
            (format!("{}->{}",self.machines[from].name,self.machines[to].name),json!({"delivered":c[0],"partition_dropped":c[1],"receiver_dropped":c[2],"loss_dropped":c[3]}))).collect();
        json!(counts)
    }
    fn route(&mut self, from: usize, time: u64, bytes: Vec<u8>) -> Result<()> {
        if bytes.len() < 14 {
            return Err("short Ethernet frame".into());
        }
        let mac = bytes[..6]
            .try_into()
            .map_err(|_| "invalid Ethernet destination")?;
        for to in links::candidates(from, mac, &self.macs) {
            let previous = self.faults.links.last.get(&(from, to)).copied();
            let cut = self.faults.blocked(from, to);
            match self.faults.links.decide(from, to, time, cut) {
                links::Outcome::Cut => self.count(time, from, to, 1),
                links::Outcome::Lost => self.count(time, from, to, 3),
                links::Outcome::Deliver(at) => {
                    // Backlogs are bounded independently of host wall speed.
                    let held = self.events[to]
                        .values()
                        .filter(|p| matches!(p, Payload::Frame { .. }))
                        .count();
                    if (self.faults.life(to) == Life::Down && at < self.faults.next_lifecycle(to))
                        || (self.faults.life(to) == Life::Paused && held >= 256)
                    {
                        if let Some(last) = previous {
                            self.faults.links.last.insert((from, to), last);
                        } else {
                            self.faults.links.last.remove(&(from, to));
                        }
                        self.count(at, from, to, 2);
                    } else {
                        self.schedule(
                            to,
                            at,
                            Payload::Frame {
                                from,
                                bytes: bytes.clone(),
                            },
                        )?;
                    }
                }
            }
        }
        Ok(())
    }
    fn deliver(&mut self, m: usize) -> Result<()> {
        while self.events[m]
            .first_key_value()
            .is_some_and(|(key, _)| key.0 <= self.machines[m].time)
        {
            let ((at, _), payload) = self.events[m].pop_first().ok_or("missing due event")?;
            match payload {
                Payload::Frame { from, bytes } => {
                    let Answer::Accepted(accepted) =
                        self.machines[m].worker.call(Operation::Frame(bytes))?
                    else {
                        return Err("invalid frame response".into());
                    };
                    self.count(at, from, m, if accepted { 0 } else { 2 });
                }
                Payload::Control(bytes) => {
                    let reply = bytes.starts_with(b"SBX_REPLY1 ");
                    if !matches!(
                        self.machines[m].worker.call(Operation::Control(bytes))?,
                        Answer::Ready
                    ) {
                        return Err("invalid control delivery response".into());
                    }
                    if reply {
                        self.control.mark_delivered(at)?;
                    }
                }
            }
        }
        Ok(())
    }
    fn lifecycle(&mut self, m: usize, op: Lifecycle, at: u64) -> Result<()> {
        if self.machines[m].time > at {
            return Err("lifecycle boundary is in machine's past".into());
        }
        match op {
            Lifecycle::Pause => {}
            Lifecycle::Resume => {
                if !matches!(
                    self.machines[m].worker.call(Operation::ResumeAt(at))?,
                    Answer::Ready
                ) {
                    return Err("invalid resume response".into());
                }
                self.machines[m].time = at;
            }
            Lifecycle::Crash => {
                let incarnation = self.machines[m].incarnation;
                let name = self.machines[m].name.clone();
                let Answer::Crashed {
                    evidence,
                    lost_sectors,
                    surviving_disk,
                } = self.machines[m].worker.call(Operation::Crash {
                    run_seed: self.seed,
                    name,
                    incarnation,
                    full_state: self.full_state,
                })?
                else {
                    return Err("invalid crash response".into());
                };
                self.machines[m].crashed.push(json!({"incarnation":incarnation,"crashed_at_ns":at,"time_ns":evidence.time_ns,"witness":evidence.witness,"retired_state_digest":evidence.state_digest,"disk_lost_sectors":lost_sectors,"surviving_disk":surviving_disk}));
                self.machines[m].down_evidence = Some(evidence);
                self.events[m].retain(|_, payload| matches!(payload, Payload::Frame { .. }));
                self.machines[m].exit = None;
            }
            Lifecycle::Reboot => {
                let incarnation = self.machines[m]
                    .incarnation
                    .checked_add(1)
                    .ok_or("incarnation overflow")?;
                self.machines[m].incarnation = incarnation;
                let mut rng = crate::rng::Rng::new(
                    self.seed,
                    &format!("reboot-entropy/{}/{incarnation}", self.machines[m].name),
                );
                self.machines[m].boot.seed = rng.next();
                let domain = format!("timer-jitter/{}/{incarnation}", self.machines[m].name);
                if let Some(policy) = self.machines[m].boot.timer_policy.as_mut() {
                    policy.domain = domain;
                }
                let boot = self.machines[m].boot.clone();
                if !matches!(
                    self.machines[m].worker.call(Operation::Boot(boot))?,
                    Answer::Ready
                ) {
                    return Err("invalid reboot response".into());
                }
                if !matches!(
                    self.machines[m].worker.call(Operation::ResumeAt(at))?,
                    Answer::Ready
                ) {
                    return Err("invalid reboot clock response".into());
                }
                self.machines[m].time = at;
                self.machines[m].down_evidence = None;
                self.machines[m].booted_at_ns = at;
                self.machines[m].framer = ManagementFramer::default();
                while self.events[m]
                    .first_key_value()
                    .is_some_and(|(key, _)| key.0 < at)
                {
                    let ((arrival, _), payload) =
                        self.events[m].pop_first().ok_or("missing stale event")?;
                    if let Payload::Frame { from, .. } = payload {
                        self.count(arrival, from, m, 2);
                    }
                }
            }
        }
        Ok(())
    }
    fn simulate(
        &mut self,
        cfg: &Config,
        host: &mut Option<crate::host_control::HostControl>,
    ) -> Result<&'static str> {
        loop {
            if let Some(host) = host {
                host.boundary().map_err(|e| format!("host control: {e}"))?;
            }
            let keys: Vec<_> = (0..self.machines.len()).map(|m| self.key(m)).collect();
            let (m, &key) = keys
                .iter()
                .enumerate()
                .min_by_key(|(m, key)| (**key, *m))
                .ok_or("empty world")?;
            let control = self.control.boundary();
            let fault = self.faults.next_boundary();
            self.nominal
                .advance(key.min(control).min(fault).min(cfg.duration_ns));
            if control <= key && control < cfg.duration_ns {
                let exited = self
                    .machines
                    .iter()
                    .filter(|m| m.exit.is_some())
                    .map(|m| m.name.clone())
                    .collect::<Vec<_>>();
                self.control.reject_unavailable(&exited);
                if self.control.pending.as_ref().is_some_and(|p| {
                    !p.admitted_done
                        && matches!(p.operation, Some(seedbox_control::Operation::Inspect {}))
                }) {
                    let machines:Vec<_>=self.machines.iter().enumerate().map(|(i,m)|json!({"name":m.name,"state":m.exit.map(|(code,_)|format!("Exited({code})")).unwrap_or_else(||self.faults.process_state(i).into())})).collect();
                    let counters = self.counters(self.nominal.counts());
                    if let Some(p) = self.control.pending.as_mut() {
                        p.result = json!({"services":cfg.control["placement"],"machines":machines,"active_handles":self.faults.active_handles(),"network_counters":counters});
                    }
                }
                if let Some(bytes) = self.control.apply_boundary(&mut self.faults, control)? {
                    self.schedule(
                        self.control.controller.ok_or("missing controller")?,
                        control,
                        Payload::Control(bytes),
                    )?;
                }
                continue;
            }
            if fault <= key && fault < cfg.duration_ns {
                for (target, bytes) in self.faults.apply_due(fault) {
                    self.schedule(target, fault, Payload::Control(bytes))?;
                }
                for i in self.faults.take_changed_network() {
                    self.faults.log[i]["network_counters"] = self.counters(self.nominal.counts());
                }
                for (target, op) in self.faults.take_lifecycle() {
                    self.lifecycle(target, op, fault)?;
                }
                self.control.observe_faults(&self.faults, fault)?;
                if self.faults.error.is_some() && self.control.pending.is_none() {
                    return Ok("fault_failed");
                }
                continue;
            }
            if key >= cfg.duration_ns {
                return Ok(
                    if key == u64::MAX && self.machines.iter().all(|m| m.exit.is_some()) {
                        "quiescent"
                    } else {
                        "duration"
                    },
                );
            }
            self.deliver(m)?;
            let horizon = keys
                .iter()
                .enumerate()
                .filter(|(i, _)| *i != m)
                .map(|(_, key)| key.saturating_add(self.lookahead))
                .min()
                .unwrap_or(u64::MAX)
                .min(
                    self.events[m]
                        .first_key_value()
                        .map(|(key, _)| key.0)
                        .unwrap_or(u64::MAX),
                )
                .min(control)
                .min(fault)
                .min(cfg.duration_ns);
            if horizon <= self.machines[m].time {
                return Err("world horizon failed to advance after delivery".into());
            }
            self.horizon_hash.update((m as u64).to_le_bytes());
            self.horizon_hash.update(horizon.to_le_bytes());
            self.slices += 1;
            let Answer::Advanced {
                stop,
                time_ns,
                events,
                console,
            } = self.machines[m].worker.call(Operation::Advance {
                horizon_ns: horizon,
            })?
            else {
                return Err("invalid machine advancement response".into());
            };
            if time_ns < self.machines[m].time || time_ns > horizon {
                return Err("machine escaped its world horizon".into());
            }
            self.machines[m].time = time_ns;
            self.machines[m]
                .console
                .write_all(&console)
                .map_err(|e| format!("console: {e}"))?;
            for event in events {
                let bytes = serde_json::to_vec(&event).map_err(|e| e.to_string())?;
                self.event_hash.update((m as u64).to_le_bytes());
                self.event_hash.update(time_ns.to_le_bytes());
                self.event_hash.update((bytes.len() as u64).to_le_bytes());
                self.event_hash.update(bytes);
                match event {
                    Event::Frame(bytes) => self.route(m, time_ns, bytes)?,
                    Event::Control(bytes) => {
                        self.machines[m]
                            .management
                            .write_all(&bytes)
                            .map_err(|e| format!("management output: {e}"))?;
                        for frame in self.machines[m].framer.receive(&bytes)? {
                            let max = self.machines.iter().map(|m| m.time).max().unwrap_or(0);
                            self.control.request(m, time_ns, &frame, max)?;
                        }
                    }
                    Event::Ready => {
                        self.faults
                            .management_ready(m, time_ns, self.control.latency)?
                    }
                    Event::Effect { id, success } => self.faults.effect(m, id, time_ns, success),
                    Event::Exit { code, setup_error } => {
                        self.machines[m].exit = Some((code, setup_error));
                        if let Some(id) = self
                            .control
                            .pending
                            .as_ref()
                            .and_then(|pending| pending.fault)
                        {
                            self.faults.target_exited(id, m, time_ns);
                        }
                    }
                    Event::Barrier => {
                        return Err("unexpected launch barrier in application world".into())
                    }
                }
            }
            if matches!(stop, MachineStop::PoweredOff) && self.machines[m].exit.is_none() {
                self.machines[m].exit = Some((255, true));
            }
            self.control.observe_faults(&self.faults, time_ns)?;
            if (self.control.error.is_some()
                || self.faults.error.is_some() && self.control.pending.is_none())
                && !self.control.has_undelivered_reply()
            {
                return Ok("fault_failed");
            }
            if self.machines[m].exit.is_some() && self.machines[m].name == cfg.end_on_exit {
                return Ok("end_machine_exited");
            }
        }
    }
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
    let torn = match cfg.disk_loss.as_deref() {
        None => false,
        Some(crate::disk::TORN_MODEL) => true,
        _ => return Err("unknown disk loss model".into()),
    };
    let base = options.config.parent().unwrap_or(Path::new("."));
    std::fs::create_dir_all(&options.output).map_err(|e| format!("world output: {e}"))?;
    let mut machines = Vec::new();
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
            torn_writes: torn,
            timer_policy: Some(super::TimerPolicy {
                seed: options.seed,
                domain: format!("timer-jitter/{}", m.name),
                jitter_ns: cfg.timer_jitter_ns,
            }),
        };
        let mut worker = OwnedWorker::spawn(&options.executable)?;
        if !matches!(worker.call(Operation::Boot(boot.clone()))?, Answer::Ready) {
            return Err("invalid machine boot response".into());
        }
        let console = std::fs::File::create(options.output.join(format!("{}.console", m.name)))
            .map_err(|e| format!("console output: {e}"))?;
        let management = std::fs::File::create(options.output.join(format!("{}.mgmt", m.name)))
            .map_err(|e| format!("management output: {e}"))?;
        machines.push(Machine {
            name: m.name.clone(),
            worker,
            boot,
            time: 0,
            exit: None,
            incarnation: 0,
            booted_at_ns: 0,
            crashed: Vec::new(),
            console: Capture {
                writer: console,
                remaining: options.console_limit,
            },
            management: Capture {
                writer: management,
                remaining: options.console_limit,
            },
            framer: ManagementFramer::default(),
            down_evidence: None,
        });
    }
    let mut horizon_hash = Sha256::new();
    horizon_hash.update(b"seedbox-arm64-world-horizons-v1\0");
    let mut event_hash = Sha256::new();
    event_hash.update(b"seedbox-arm64-world-events-v1\0");
    let mut world = World {
        events: (0..machines.len()).map(|_| BTreeMap::new()).collect(),
        machines,
        faults,
        control,
        sequence: 0,
        lookahead,
        delivered: 0,
        dropped: 0,
        counts: BTreeMap::new(),
        nominal: NetworkCounters::default(),
        macs,
        seed: options.seed,
        slices: 0,
        horizon_hash,
        event_hash,
        full_state: options.full_state,
    };
    let mut host = options
        .host_control
        .as_deref()
        .map(crate::host_control::HostControl::bind)
        .transpose()
        .map_err(|e| format!("host control: {e}"))?;
    let outcome = world.simulate(&cfg, &mut host)?;
    let cutoff = if outcome == "duration" {
        cfg.duration_ns
    } else {
        world.machines.iter().map(|m| m.time).max().unwrap_or(0)
    };
    world.control.cut_off(&world.faults, cutoff);
    let mut hash = Sha256::new();
    hash.update(b"seedbox-arm64-world-witness-v1\0");
    let horizons = format!("{:x}", world.horizon_hash.clone().finalize());
    hash.update(horizons.as_bytes());
    hash.update(world.event_hash.clone().finalize());
    let mut rows = Vec::new();
    for (i, m) in world.machines.iter_mut().enumerate() {
        let evidence = if let Some(evidence) = m.down_evidence.as_ref() {
            evidence.clone()
        } else {
            let Answer::Evidence(evidence) = m.worker.call(Operation::Evidence {
                full_state: options.full_state,
            })?
            else {
                return Err("invalid machine evidence response".into());
            };
            evidence
        };
        hash.update(evidence.witness.as_bytes());
        if let Some(state) = &evidence.state_digest {
            hash.update(state.as_bytes());
        }
        hash.update(serde_json::to_vec(&m.crashed).map_err(|e| e.to_string())?);
        rows.push(json!({"name":m.name,"state":if world.faults.life(i)==Life::Down {"Down".into()} else if world.faults.life(i)==Life::Paused {"Paused".into()} else {m.exit.map(|(code,_)|format!("Exited({code})")).unwrap_or_else(||"Running".into())},"time_ns":m.time,"guest_exit_code":m.exit.filter(|(_,setup)|!*setup).map(|(code,_)|code),"guest_init_error":m.exit.filter(|(_,setup)|*setup).map(|(code,_)|code),"infrastructure_error":m.exit.is_some_and(|(_,setup)|setup),"witness":evidence.witness,"state_digest":evidence.state_digest,"state_digest_parts":evidence.state_digest_parts,"control_channel":evidence.control_channel,"booted_at_ns":m.booted_at_ns,"application_evidence":evidence,"incarnation":m.incarnation,"crashed_incarnations":m.crashed}));
    }
    let mut network = Sha256::new();
    world.faults.network_witness(&mut network);
    for (to, events) in world.events.iter().enumerate() {
        for (&(at, seq), payload) in events {
            network.update((to as u64).to_le_bytes());
            network.update(at.to_le_bytes());
            network.update(seq.to_le_bytes());
            match payload {
                Payload::Frame { from, bytes } => {
                    network.update([0]);
                    network.update((*from as u64).to_le_bytes());
                    network.update(Sha256::digest(bytes));
                }
                Payload::Control(bytes) => {
                    network.update([1]);
                    network.update(Sha256::digest(bytes));
                }
            }
        }
    }
    let network = format!("{:x}", network.finalize());
    hash.update(network.as_bytes());
    let counts: Vec<_> = world.counts.iter().collect();
    hash.update(
        serde_json::to_vec(&(&world.faults.log, &world.control.log, counts))
            .map_err(|e| e.to_string())?,
    );
    Ok(
        json!({"report_version":2,"seed":options.seed,"timebase":"arm64-pv-world-v1","outcome":outcome,"world_witness":format!("{:x}",hash.finalize()),"network_witness":network,"horizon_witness":horizons,"machines":rows,"frames_delivered":world.delivered,"frames_dropped":world.dropped,"network_counters":world.counters(&world.counts),"faults":world.faults.log,"fault_error":world.faults.error.or(world.control.error),"control_history":world.control.log,"control_latency_ns":world.control.latency,"rejected_fault_effects":world.faults.rejected_effects,"management_readiness":world.faults.readiness,"slices":world.slices}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn capture_boundary_does_not_reject_or_retain_overflow() {
        let mut capture = Capture {
            writer: Vec::new(),
            remaining: 5,
        };
        capture.write_all(b"abc").expect("first");
        capture.write_all(b"defgh").expect("overflow");
        capture.write_all(b"ignored").expect("exhausted");
        assert_eq!(capture.writer, b"abcde");
        assert_eq!(capture.remaining, 0);
    }
}
