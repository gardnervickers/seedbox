//! Seedbox world: several Linux machines, a network fabric and a scenario,
//! all executed by one host thread. A seed and the config fully determine the run.

mod boundary;
mod control;
mod counter;
mod devices;
mod disk;
mod events;
mod faults;
mod machine;
mod network_counters;
mod profile;
mod rng;
mod stepper;
mod timebase;
mod undo;
mod virtio_blk;
mod virtio_net;
mod virtio_console;

use counter::Counter;
use machine::{BootSpec, Machine, MachineSnapshot, State, Stop, NET_IRQ, NET_MMIO, CHANNEL_IRQ, CHANNEL_MMIO, BLK_IRQ, BLK_MMIO};
use rng::Rng;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::rc::Rc;
use std::time::Instant;

thread_local! {
    pub static START: Instant = Instant::now();
}

#[derive(Clone)]
enum Payload {
    Frame { from: usize, bytes: Vec<u8> },
    Serial(Vec<u8>),
    Partition { from: usize, to: usize, on: bool },
}

/// Frames that may wait for a paused machine, queued while it is paused and
/// due before it resumes. Later ones are dropped at the receiver. Part of the
/// execution semantics: changing it changes runs.
const PAUSED_INGRESS_FRAMES: u64 = 256;

/// Everything a machine boots from. A reboot starts a new incarnation from
/// exactly these inputs, with its own entropy and timer seeds.
#[derive(Clone)]
struct BootInputs {
    kernel: Rc<[u8]>,
    initrd: Rc<[u8]>,
    /// The command line before and after the launch barrier flag.
    cmdline: (String, String),
    mem: usize,
    mac: Option<[u8; 6]>,
    control_channel: bool,
    /// The disk as configured, with nothing written. Only the first
    /// incarnation boots from it; a reboot inherits its predecessor's.
    disk: Option<disk::Disk>,
    undo_margin: u64,
    tolerance: u64,
    timer_jitter: u64,
    exact_timer_ppm: u64,
    /// A fixed stepping margin; None adapts it.
    margin: Option<u64>,
    console_limit: Option<usize>,
}

fn entropy_seed(seed: u64, domain: &str) -> [u8; 32] {
    let mut rng = Rng::new(seed, domain);
    let mut out = [0u8; 32];
    for c in out.chunks_mut(8) {
        c.copy_from_slice(&rng.next().to_le_bytes());
    }
    out
}

/// A machine's `disk`: `{"size_mib": N}` for a zeroed disk, or
/// `{"image": PATH}` for a raw image (with `size_mib`, it must be that size).
fn machine_disk(cfg: &Value, path: &dyn Fn(&Value) -> std::path::PathBuf) -> Result<Option<disk::Disk>, String> {
    if cfg.is_null() {
        return Ok(None);
    }
    let fields = cfg.as_object().ok_or("disk must be an object")?;
    if let Some(key) = fields.keys().find(|k| !matches!(k.as_str(), "size_mib" | "image")) {
        return Err(format!("disk: unknown field {key}"));
    }
    let sectors = match &cfg["size_mib"] {
        Value::Null => None,
        v => Some(v.as_u64().filter(|&m| m > 0 && m <= 1 << 20).ok_or("disk.size_mib must be 1..=1048576")? * 2048),
    };
    let image = match &cfg["image"] {
        Value::Null => None,
        v if v.is_string() => {
            let p = path(v);
            Some(std::fs::read(&p).map_err(|e| format!("disk image {}: {e}", p.display()))?)
        }
        _ => return Err("disk.image must be a path".into()),
    };
    disk::Disk::new(image, sectors).map(Some)
}

fn boot_machine(kvm: &kvm_ioctls::Kvm, name: &str, b: &BootInputs, barrier: bool, rng_seed: [u8; 32]) -> Result<Machine, String> {
    let mut cmdline = b.cmdline.0.clone();
    if barrier {
        cmdline += " seedbox.barrier=1";
    }
    cmdline += &b.cmdline.1;
    let spec = BootSpec {
        kernel: &b.kernel,
        initrd: &b.initrd,
        cmdline,
        mem: b.mem,
        rng_seed,
        mac: b.mac,
        control_channel: b.control_channel,
        disk: b.disk.clone(),
        undo_margin: b.undo_margin,
        barrier,
    };
    let mut mach = Machine::new(kvm, name, &spec)?;
    mach.com1.output_limit = b.console_limit;
    mach.com2.output_limit = b.console_limit;
    mach.tolerance = b.tolerance;
    mach.timer_jitter = b.timer_jitter;
    mach.exact_timer_ppm = b.exact_timer_ppm;
    match b.margin {
        None => mach.adaptive_margin = true,
        Some(m) => mach.margin = m,
    }
    Ok(mach)
}

struct World {
    machines: Vec<Machine>,
    kvm: kvm_ioctls::Kvm,
    boot: Vec<BootInputs>,
    /// The first incarnation parks at its launch barrier.
    barrier: bool,
    boot_seed: u64,
    /// The seed the world plan was armed with; reboots draw from it.
    run_seed: u64,
    /// Per machine, a summary of each incarnation that crashed.
    incarnations: Vec<Vec<Value>>,
    /// Frames queued for each paused machine during its current pause.
    paused_ingress: Vec<u64>,
    counter: Counter,
    events: Vec<BTreeMap<(u64, u64), Payload>>,
    seq: u64,
    /// No frame can arrive sooner than this after it is sent: the horizon.
    lookahead: u64,
    /// Slices may end, and events be delivered, up to this long before their
    /// time. Below `lookahead`, so nothing is ever delivered into the past.
    early: u64,
    latency: u64,
    jitter: u64,
    link_rng: BTreeMap<(usize, usize), Rng>,
    link_last: BTreeMap<(usize, usize), u64>,
    partitioned: BTreeMap<(usize, usize), bool>,
    faults: faults::Faults,
    control: control::Control,
    strict_events: Vec<BTreeMap<(u64, u64), Vec<u8>>>,
    macs: Vec<[u8; 6]>,
    dropped: u64,
    delivered: u64,
    link_counts: BTreeMap<(usize,usize), [u64;3]>,
    semantic_counts:network_counters::NetworkCounters,
    slices: u64,
}

impl World {
    fn count_link(&mut self,at:u64,from:usize,to:usize,outcome:usize) {
        self.link_counts.entry((from,to)).or_default()[outcome]+=1;
        self.semantic_counts.record(at,from,to,outcome);
    }
    fn network_snapshot(&self)->Value {self.counter_snapshot(&self.link_counts)}
    fn counter_snapshot(&self,counts:&BTreeMap<(usize,usize),[u64;3]>)->Value {
        let snapshot:BTreeMap<_,_>=counts.iter().map(|(&(from,to),counts)|(
            format!("{}->{}",self.machines[from].name,self.machines[to].name),
            json!({"delivered":counts[0],"partition_dropped":counts[1],"receiver_dropped":counts[2]})
        )).collect();json!(snapshot)
    }
    fn schedule(&mut self, m: usize, t: u64, p: Payload) {
        self.seq += 1;
        self.events[m].insert((t, self.seq), p);
    }

    fn next_event(&self, m: usize) -> u64 {
        self.events[m]
            .keys()
            .next()
            .map(|k| k.0)
            .unwrap_or(u64::MAX)
            .min(
                self.strict_events[m]
                    .keys()
                    .next()
                    .map(|k| k.0)
                    .unwrap_or(u64::MAX),
            )
    }

    /// Earliest virtual time at which machine m can next act.
    fn key(&self, m: usize) -> u64 {
        let mach = &self.machines[m];
        if mach.parked || mach.paused || mach.down {
            return u64::MAX;
        }
        match mach.state {
            State::Exited(_) => u64::MAX,
            State::Running => mach.time,
            State::Halted => {
                if mach.has_pending_irq() {
                    mach.time
                } else {
                    mach.next_deadline().min(self.next_event(m)).max(mach.time)
                }
            }
        }
    }

    /// Deliver events due by the end of the machine's early window.
    fn deliver_due(&mut self, m: usize) -> Result<(),String> {
        while let Some(bytes) = events::pop_due(&mut self.strict_events[m], self.machines[m].time) {
            if self.machines[m].channel.is_some() {self.machines[m].deliver_control(&bytes)?;} else {self.machines[m].deliver_serial(&bytes);}
            if bytes.starts_with(b"SBX_REPLY1 ") {
                self.control.mark_delivered(self.machines[m].time)?;
            }
        }
        // Never early across this machine's next lifecycle boundary: what
        // arrives at or after a pause or crash must not reach it before.
        let now = events::early_bound(self.machines[m].time, self.early, self.faults.next_lifecycle(m));
        while let Some((at,p)) = events::pop_due_at(&mut self.events[m], now) {
            let mach = &mut self.machines[m];
            match p {
                Payload::Frame { from, bytes } => {
                    if !mach.deliver_frame(&bytes).accepted {
                        self.dropped += 1;
                        self.count_link(at,from, m, 2);
                    } else {
                        self.delivered += 1;
                        self.count_link(at,from, m, 0);
                    }
                }
                Payload::Serial(b) => mach.deliver_serial(&b),
                Payload::Partition { from, to, on } => {
                    self.partitioned.insert((from, to), on);
                }
            }
        }
        Ok(())
    }

    fn route(&mut self, src: usize, send_time: u64, frame: Vec<u8>) {
        if frame.len() < 14 {
            return;
        }
        let dst_mac: [u8; 6] = frame[0..6].try_into().unwrap();
        let targets: Vec<usize> = if dst_mac == [0xff; 6] {
            (0..self.machines.len()).filter(|&k| k != src).collect()
        } else {
            self.macs
                .iter()
                .position(|m| *m == dst_mac)
                .into_iter()
                .filter(|&k| k != src)
                .collect()
        };
        for dst in targets {
            if self.faults.blocked(src, dst) || *self.partitioned.get(&(src, dst)).unwrap_or(&false)
            {
                self.dropped += 1;
                self.count_link(send_time,src, dst, 1);
                continue;
            }
            let jitter = self
                .link_rng
                .get_mut(&(src, dst))
                .unwrap()
                .below(self.jitter + 1);
            let last = *self.link_last.get(&(src, dst)).unwrap_or(&0);
            let t = (send_time + self.latency + jitter).max(last + 1);
            // A frame arriving while its receiver is down meets no NIC; one
            // arriving while it is paused waits, up to a fixed backlog.
            let to = &self.machines[dst];
            match events::arrival(t, self.faults.next_lifecycle(dst), to.down, to.paused, self.paused_ingress[dst], PAUSED_INGRESS_FRAMES) {
                events::Arrival::Queue => {}
                events::Arrival::Held => self.paused_ingress[dst] += 1,
                events::Arrival::Drop => {
                    self.dropped += 1;
                    self.count_link(t, src, dst, 2);
                    continue;
                }
            }
            self.link_last.insert((src, dst), t);
            self.schedule(
                dst,
                t,
                Payload::Frame {
                    from: src,
                    bytes: frame.clone(),
                },
            );
        }
    }

    /// Carry out a lifecycle transition at world boundary `t`, before any
    /// machine runs at `t`.
    fn lifecycle(&mut self, m: usize, op: faults::Lifecycle, t: u64) -> Result<(), String> {
        if self.machines[m].time > t {
            return Err("lifecycle boundary is in committed machine past".into());
        }
        match op {
            faults::Lifecycle::Pause => {
                self.paused_ingress[m] = 0;
                self.machines[m].pause()
            }
            faults::Lifecycle::Resume => self.machines[m].resume(t),
            faults::Lifecycle::Crash => self.crash(m, t),
            faults::Lifecycle::Reboot => self.reboot(m, t),
        }
    }

    /// Destroy machine m's VM at `t` and build its next incarnation from the
    /// retained boot inputs, ready to boot when it reboots. Management
    /// commands queued for the dead incarnation go with it; frames in world
    /// transit stay queued for the reboot to sort out.
    fn crash(&mut self, m: usize, t: u64) -> Result<(), String> {
        let name = self.machines[m].name.clone();
        let next = self.machines[m].incarnation.checked_add(1).ok_or("incarnation overflow")?;
        let seed = entropy_seed(self.run_seed, &format!("reboot-entropy/{name}/{next}"));
        let mut fresh = boot_machine(&self.kvm, &name, &self.boot[m], false, seed)?;
        fresh.timer_rng = Some(Rng::new(self.run_seed, &format!("timer-jitter/{name}/{next}")));
        let mut old = std::mem::replace(&mut self.machines[m], fresh);
        let discarded: usize = self.strict_events[m].values().map(Vec::len).sum();
        self.strict_events[m].clear();
        let mut summary = json!({
            "incarnation": old.incarnation, "booted_at_ns": old.booted_at, "crashed_at_ns": t,
            "time_ns": old.time, "branches": old.branches, "exits": old.stats.exits,
            "state": format!("{:?}", old.state), "paused": old.paused,
            "discarded_pending_io": old.pending_io.is_some(), "discarded_management_bytes": discarded,
            "witness": old.exit_witness(),
        });
        summary["disk_lost_sectors"] = json!(self.machines[m].succeed(&mut old, t, self.run_seed)?);
        self.incarnations[m].push(summary);
        Ok(())
    }

    /// Boot machine m's next incarnation at `t`. Frames that reached it
    /// before `t` (held while it was paused, or arriving while it was down)
    /// belonged to the dead NIC and are dropped; later ones arrive normally.
    fn reboot(&mut self, m: usize, t: u64) -> Result<(), String> {
        while let Some((at, p)) = events::pop_before(&mut self.events[m], t) {
            match p {
                Payload::Frame { from, .. } => {
                    self.dropped += 1;
                    self.count_link(at, from, m, 2);
                }
                Payload::Serial(_) => {}
                Payload::Partition { from, to, on } => {
                    self.partitioned.insert((from, to), on);
                }
            }
        }
        self.machines[m].boot_at(t)
    }
}

fn arg(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1).cloned())
}

// The same machine may legitimately return at an equal-time delivery. Fail
// only if due processing/replanning produces the identical stalled plan twice.
type SlicePlan = (usize, u64, u64, u64, u64, u64, u64, u64, u64, u64);
fn stop_progress(previous: &mut Option<SlicePlan>, plan: SlicePlan, advanced: bool) -> Result<(), String> {
    if advanced { *previous=None; }
    else if previous.as_ref()==Some(&plan) {
        return Err(format!("scheduler made no progress after equal-time replanning: machine={} time={} limit={}",plan.0,plan.1,plan.2));
    } else { *previous=Some(plan); }
    Ok(())
}

fn run() -> Result<Value, String> {
    START.with(|_| ());
    let args: Vec<String> = std::env::args().collect();
    let config_path = arg(&args, "--config").ok_or("--config required")?;
    let seed: u64 = arg(&args, "--seed")
        .ok_or("--seed required")?
        .parse()
        .map_err(|e| format!("seed: {e}"))?;
    // Further seeds released from the same booted world, one after another
    // in this process (requires a boot snapshot in the config).
    let mut seeds = vec![seed];
    if let Some(list) = arg(&args, "--fork-seeds") {
        for s in list.split(',') {
            seeds.push(s.parse().map_err(|e| format!("--fork-seeds: {e}"))?);
        }
    }
    let out = arg(&args, "--out").ok_or("--out required")?;
    let digest = args.iter().any(|a| a == "--digest");
    // Undo-able precise stops (performance only): initial final margin, 0 = off.
    let undo_margin: u64 = match std::env::var("SEEDBOX_UNDO").as_deref() {
        Err(_) => 256,
        Ok(m) => m.parse().map_err(|e| format!("SEEDBOX_UNDO: {e}"))?,
    };
    std::fs::create_dir_all(&out).map_err(|e| e.to_string())?;
    let cfg: Value = serde_json::from_slice(
        &std::fs::read(&config_path).map_err(|e| format!("{config_path}: {e}"))?,
    )
    .map_err(|e| format!("config: {e}"))?;
    let torn_writes = match &cfg["disk_loss"] {
        Value::Null => false,
        Value::String(model) if model == disk::TORN_MODEL => true,
        _ => return Err("unknown disk_loss model".into()),
    };
    let base = std::path::Path::new(&config_path)
        .parent()
        .unwrap()
        .to_path_buf();
    let path = |v: &Value| base.join(v.as_str().unwrap());
    let kernel = std::fs::read(path(&cfg["kernel"])).map_err(|e| format!("kernel: {e}"))?;
    let mem = cfg["mem_mb"].as_u64().unwrap_or(512) as usize * 1024 * 1024;
    let duration = cfg["duration_ns"].as_u64().unwrap_or(60_000_000_000);
    let end_on: Option<String> = cfg["end_on_exit"].as_str().map(String::from);
    let names: Vec<String> = cfg["machines"]
        .as_array()
        .ok_or("machines")?
        .iter()
        .map(|m| m["name"].as_str().unwrap().to_string())
        .collect();
    let networked = cfg["network"].is_object();
    let control_channel=cfg["control_channel"]=="virtio-console-v1";
    if !cfg["control"].is_null() && !control_channel {return Err("runtime control requires virtio-console-v1 channel".into());}
    if !cfg["control_channel"].is_null() && !control_channel {return Err("unknown runtime control channel".into());}

    // Boot once under a fixed boot seed and park every machine at its launch
    // barrier; the run seed applies from the release onwards. The world at
    // the barrier is the same for every run seed, so it can be forked.
    let boot = &cfg["boot"];
    let forked = boot.is_object();
    let boot_seed = if forked { boot["seed"].as_u64().ok_or("boot.seed required")? } else { seed };
    let release_skew = boot["release_skew_ns"].as_u64().unwrap_or(0);
    // Fork at a simulated time instead of the guests' launch barrier: every
    // machine parks at its first deterministic point at or after it.
    let fork_at = boot["fork_at_ns"].as_u64();
    if fork_at.is_some_and(|t| t >= duration) {
        return Err("boot.fork_at_ns must be before duration_ns".into());
    }
    if let Some(t) = fork_at {
        // A barrier fork's release time is known only after boot; checked
        // again at the release.
        check_plan_after(&cfg, t.saturating_add(release_skew))?;
    }
    if seeds.len() > 1 && !forked {
        return Err("--fork-seeds requires boot.seed in the config".into());
    }
    if seeds.len() > 1 && args.iter().any(|a| a == "--trace") {
        return Err("--trace cannot be combined with --fork-seeds".into());
    }
    let kvm = kvm_ioctls::Kvm::new().map_err(|e| format!("kvm: {e}"))?;
    let kernel: Rc<[u8]> = kernel.into();
    let margin = match std::env::var("SEEDBOX_MARGIN").as_deref() {
        Ok("adaptive") | Err(_) => None,
        Ok(m) => Some(m.parse().map_err(|e| format!("margin: {e}"))?),
    };
    let console_limit = match arg(&args, "--console-limit") {
        Some(limit) => Some(limit.parse::<usize>().map_err(|error| format!("console limit: {error}"))?),
        None => None,
    };
    let barrier = forked && fork_at.is_none();
    let mut machines = Vec::new();
    let mut macs = Vec::new();
    let mut boots = Vec::new();
    let mut initrds: BTreeMap<String, Rc<[u8]>> = BTreeMap::new();
    for (i, mc) in cfg["machines"].as_array().unwrap().iter().enumerate() {
        let ipath = path(&mc["initrd"]).to_string_lossy().to_string();
        if !initrds.contains_key(&ipath) {
            initrds.insert(
                ipath.clone(),
                std::fs::read(&ipath).map_err(|e| format!("{ipath}: {e}"))?.into(),
            );
        }
        let mac = [0x52, 0x54, 0x00, 0x00, 0x00, i as u8 + 1];
        macs.push(mac);
        let mut cmdline = format!(
            "console=ttyS0 nokaslr noapic nolapic acpi=off io_delay=none tsc=reliable tsc_early_khz=1000000 lpj=1000000 \
             clocksource=tsc random.trust_cpu=off random.trust_bootloader=on panic=-1 loglevel=4 rdinit=/init \
             seedbox.name={} seedbox.index={}",
            names[i], i
        );
        if networked {
            cmdline += &format!(" virtio_mmio.device=4K@{NET_MMIO:#x}:{NET_IRQ}");
        }
        if control_channel {cmdline+=&format!(" virtio_mmio.device=4K@{CHANNEL_MMIO:#x}:{CHANNEL_IRQ}");}
        let disk = machine_disk(&mc["disk"], &path)?.map(|d| d.with_torn_writes(torn_writes));
        if disk.is_some() {
            cmdline += &format!(" virtio_mmio.device=4K@{BLK_MMIO:#x}:{BLK_IRQ}");
        }
        let mut after = String::new();
        if names.get(i).is_some_and(|name|cfg["control"]["machine"].as_str()==Some(name.as_str())) {after+=" seedbox.control=1";}
        if let Some(extra) = mc["cmdline"].as_str() {
            after += " ";
            after += extra;
        }
        let boot = BootInputs {
            kernel: kernel.clone(),
            initrd: initrds[&ipath].clone(),
            cmdline: (cmdline, after),
            mem,
            mac: networked.then_some(mac),
            control_channel,
            disk,
            undo_margin,
            tolerance: cfg["stop_tolerance_ns"].as_u64().unwrap_or(0),
            timer_jitter: cfg["timer_jitter_ns"].as_u64().unwrap_or(0),
            exact_timer_ppm: cfg["exact_timer_ppm"].as_u64().unwrap_or(0),
            margin,
            console_limit,
        };
        let seed = entropy_seed(boot_seed, &format!("boot-entropy/{}", names[i]));
        let mut mach = boot_machine(&kvm, &names[i], &boot, barrier, seed)?;
        boots.push(boot);
        if args.iter().any(|arg| arg == "--stream-console") {
            mach.com1.output_file = Some(
                std::fs::File::create(format!("{out}/{}.console", names[i]))
                    .map_err(|error| error.to_string())?,
            );
            mach.com2.output_file = Some(
                std::fs::File::create(format!("{out}/{}.mgmt", names[i]))
                    .map_err(|error| error.to_string())?,
            );
        }
        mach.timer_rng = Some(Rng::new(boot_seed, &format!("timer-jitter/{}", names[i])));
        if args.iter().any(|a| a == "--trace") {
            let f = std::fs::File::create(format!("{out}/{}.trace", names[i]))
                .map_err(|e| e.to_string())?;
            mach.trace = Some(std::io::BufWriter::new(f));
        }
        machines.push(mach);
    }
    if !cfg["control"].is_null()
        && (!cfg["faults"].as_array().is_none_or(Vec::is_empty)
            || !cfg["scenario"].as_array().is_none_or(Vec::is_empty)
            || cfg["kill_restart"].is_object())
    {
        return Err("runtime control cannot mix with static/legacy scenarios".into());
    }
    if !cfg["control"].is_null() && cfg["control"]["machine"] != cfg["end_on_exit"] {
        return Err("controller must also be the end_on_exit machine".into());
    }
    let n = machines.len();
    let net = &cfg["network"];
    let latency = net["latency_ns"].as_u64().unwrap_or(1_000_000);
    let early = (latency / 2).min(cfg["stop_tolerance_ns"].as_u64().unwrap_or(0));
    let mut world = World {
        kvm,
        boot: boots,
        barrier,
        boot_seed,
        run_seed: seed,
        incarnations: vec![Vec::new(); n],
        paused_ingress: vec![0; n],
        counter: Counter::new()?,
        events: (0..n).map(|_| BTreeMap::new()).collect(),
        seq: 0,
        lookahead: latency,
        early,
        latency,
        jitter: net["jitter_ns"].as_u64().unwrap_or(0),
        link_rng: link_rngs(boot_seed, &names),
        link_last: BTreeMap::new(),
        partitioned: BTreeMap::new(),
        // Faults, control and scenarios belong to the run seed: armed below,
        // or at the release of a booted world.
        faults: boot_faults(&names, &[], latency)?,
        control: control::Control::new(&Value::Null, names.clone(), latency, early)?,
        strict_events: (0..n).map(|_| BTreeMap::new()).collect(),
        macs,
        dropped: 0,
        delivered: 0,
        link_counts: BTreeMap::new(),
        semantic_counts:network_counters::NetworkCounters::default(),
        slices: 0,
        machines,
    };
    if !forked {
        let kills = arm_seed(&mut world, &cfg, &names, seed, 0)?;
        let started = Instant::now();
        let outcome = simulate(&mut world, &cfg, duration, &end_on, None)?;
        let host_seconds = started.elapsed().as_secs_f64();
        return report(&mut world, outcome, seed, &kills, host_seconds, digest, &out, duration, &end_on);
    }

    let started = Instant::now();
    simulate(&mut world, &cfg, duration, &end_on, Some(fork_at))?;
    let barrier_ns = world.machines.iter().map(|m| m.time).max().unwrap_or(0);
    let boot_host_seconds = started.elapsed().as_secs_f64();
    let started = Instant::now();
    let snapshot = if seeds.len() > 1 { Some(WorldSnapshot::take(&mut world)?) } else { None };
    let snapshot_host_seconds = started.elapsed().as_secs_f64();
    let stream = args.iter().any(|a| a == "--stream-console");
    let mut reports = Vec::new();
    for (i, &seed) in seeds.iter().enumerate() {
        let started = Instant::now();
        let restored_pages = match (i, snapshot.as_ref()) {
            (0, _) | (_, None) => 0,
            (_, Some(snapshot)) => snapshot.restore(&mut world)?,
        };
        let restore_host_seconds = started.elapsed().as_secs_f64();
        let dir = if seeds.len() > 1 { format!("{out}/seed-{seed}") } else { out.clone() };
        if seeds.len() > 1 {
            std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
            if stream {
                for mach in world.machines.iter_mut() {
                    for (uart, ext) in [(&mut mach.com1, "console"), (&mut mach.com2, "mgmt")] {
                        use std::io::Write;
                        let mut file = std::fs::File::create(format!("{dir}/{}.{ext}", mach.name))
                            .map_err(|e| e.to_string())?;
                        file.write_all(&uart.out).map_err(|e| e.to_string())?;
                        uart.output_file = Some(file);
                    }
                }
            }
        }
        let started = Instant::now();
        let kills = release(&mut world, &cfg, &names, seed, release_skew)?;
        let outcome = simulate(&mut world, &cfg, duration, &end_on, None)?;
        let host_seconds = started.elapsed().as_secs_f64();
        let mut r = report(&mut world, outcome, seed, &kills, host_seconds, digest, &dir, duration, &end_on)?;
        r["boot"] = json!({
            "seed": boot_seed, "barrier_ns": barrier_ns, "fork_at_ns": fork_at, "release_skew_ns": release_skew,
            "boot_host_seconds": boot_host_seconds, "forked": i > 0,
            "restored_pages": restored_pages, "restore_host_seconds": restore_host_seconds,
        });
        reports.push(r);
    }
    if reports.len() == 1 {
        return Ok(reports.pop().unwrap());
    }
    Ok(json!({
        "report_version": 1,
        "boot": {"seed": boot_seed, "barrier_ns": barrier_ns, "boot_host_seconds": boot_host_seconds,
                 "snapshot_host_seconds": snapshot_host_seconds},
        "seeds": reports,
    }))
}

fn link_rngs(seed: u64, names: &[String]) -> BTreeMap<(usize, usize), Rng> {
    let mut rngs = BTreeMap::new();
    for a in 0..names.len() {
        for b in 0..names.len() {
            if a != b {
                rngs.insert((a, b), Rng::new(seed, &format!("link/{}/{}", names[a], names[b])));
            }
        }
    }
    rngs
}

/// Faults while the world boots towards its fork point: no plan yet, but
/// management readiness is recorded, so a plan armed at the release finds the
/// services that became ready before it.
fn boot_faults(names: &[String], readiness: &[Value], latency: u64) -> Result<faults::Faults, String> {
    let mut faults = faults::Faults::new(Vec::new(), names.to_vec())?;
    faults.require_readiness();
    replay_readiness(&mut faults, names, readiness, latency)?;
    Ok(faults)
}

fn replay_readiness(faults: &mut faults::Faults, names: &[String], readiness: &[Value], latency: u64) -> Result<(), String> {
    for r in readiness {
        let m = names.iter().position(|n| r["machine"] == n.as_str()).ok_or("unknown ready machine")?;
        faults.management_ready(m, r["at_ns"].as_u64().ok_or("readiness at_ns")?, latency)?;
    }
    Ok(())
}

/// Every timed part of the run seed's plan (faults, scenario events and the
/// kill schedule) must start at or after `from`, when every machine of a
/// forked world has been released. Earlier times have already passed.
fn check_plan_after(cfg: &Value, from: u64) -> Result<(), String> {
    let mut times = Vec::new();
    for (key, field) in [("faults", "at_ns"), ("scenario", "at_ns")] {
        for (i, ev) in cfg[key].as_array().into_iter().flatten().enumerate() {
            times.push((format!("{key}[{i}].{field}"), ev[field].as_u64()));
        }
    }
    if cfg["kill_restart"].is_object() {
        times.push(("kill_restart.start_ns".into(), cfg["kill_restart"]["start_ns"].as_u64()));
    }
    for (what, t) in times {
        let t = t.ok_or(format!("{what} required"))?;
        if t < from {
            return Err(format!(
                "{what} {t} is before the fork point {from}; in a forked world it must be at or after the release (boot.fork_at_ns or the launch barrier, plus boot.release_skew_ns)"
            ));
        }
    }
    Ok(())
}

/// Install everything the run seed decides apart from the machines: link
/// jitter, faults, runtime control, scenario events and the kill schedule.
/// Every timed item must fall at or after `from`. Returns the kill schedule
/// for the report.
fn arm_seed(world: &mut World, cfg: &Value, names: &[String], seed: u64, from: u64) -> Result<Vec<Value>, String> {
    check_plan_after(cfg, from)?;
    let booted = std::mem::take(&mut world.faults.readiness);
    world.faults = faults::Faults::new(
        cfg["faults"].as_array().cloned().unwrap_or_default(),
        names.to_vec(),
    )?;
    world.control = control::Control::new(&cfg["control"], names.to_vec(), world.latency, world.early)?;
    world.faults.require_readiness();
    replay_readiness(&mut world.faults, names, &booted, world.latency)?;
    world.link_rng = link_rngs(seed, names);
    world.run_seed = seed;
    let index = |name: &str| {
        names
            .iter()
            .position(|x| x == name)
            .ok_or(format!("unknown machine {name}"))
    };
    for ev in cfg["scenario"].as_array().cloned().unwrap_or_default() {
        let t = ev["at_ns"].as_u64().ok_or("scenario at_ns")?;
        if let Some(s) = ev["serial"].as_str() {
            let m = index(ev["machine"].as_str().unwrap())?;
            world.schedule(m, t, Payload::Serial(s.as_bytes().to_vec()));
        }
        if let Some(p) = ev["partition"].as_array() {
            let (a, b) = (
                index(p[0].as_str().unwrap())?,
                index(p[1].as_str().unwrap())?,
            );
            let on = ev["on"].as_bool().unwrap_or(true);
            // Partition state is consulted when the sender routes; apply it at the sender.
            world.schedule(a, t, Payload::Partition { from: a, to: b, on });
        }
    }

    // Seeded unclean stop/restart schedule: one member down at a time.
    let mut kills = Vec::new();
    let g = &cfg["kill_restart"];
    if g.is_object() {
        let members: Vec<usize> = g["members"]
            .as_array()
            .ok_or("kill_restart.members")?
            .iter()
            .map(|v| index(v.as_str().unwrap()))
            .collect::<Result<_, _>>()?;
        let span =
            |k: &str| -> (u64, u64) { (g[k][0].as_u64().unwrap(), g[k][1].as_u64().unwrap()) };
        let (gap, down) = (span("gap_ns"), span("down_ns"));
        let end = g["end_ns"].as_u64().unwrap();
        let mut rng = Rng::new(seed, "scenario/kill_restart");
        let mut t = g["start_ns"].as_u64().unwrap() + gap.0 + rng.below(gap.1 - gap.0 + 1);
        while t < end {
            let m = members[rng.below(members.len() as u64) as usize];
            let d = down.0 + rng.below(down.1 - down.0 + 1);
            world.schedule(m, t, Payload::Serial(b"stop\n".to_vec()));
            world.schedule(m, t + d, Payload::Serial(b"start\n".to_vec()));
            kills.push(json!({"member": names[m], "stop_ns": t, "start_ns": t + d}));
            t += d + gap.0 + rng.below(gap.1 - gap.0 + 1);
        }
    }
    Ok(kills)
}

/// Release a world parked at its launch barrier under `seed`. Every machine
/// resumes at the latest barrier time plus a seeded skew, so which service
/// starts first still varies by seed. Each guest reads its seeded entropy and
/// reseeds its kernel CRNG; timer jitter and the seed's world plan start here.
fn release(world: &mut World, cfg: &Value, names: &[String], seed: u64, skew: u64) -> Result<Vec<Value>, String> {
    let at = world.machines.iter().map(|m| m.time).max().unwrap_or(0);
    check_plan_after(cfg, at.saturating_add(skew))?;
    for (i, mach) in world.machines.iter_mut().enumerate() {
        let mut rng = Rng::new(seed, &format!("fork-entropy/{}", names[i]));
        let entropy = std::array::from_fn(|_| rng.next() as u32);
        let delay = Rng::new(seed, &format!("release/{}", names[i])).below(skew.saturating_add(1));
        mach.release(at.saturating_add(delay), entropy)?;
        mach.timer_rng = Some(Rng::new(seed, &format!("timer-jitter/{}", names[i])));
    }
    arm_seed(world, cfg, names, seed, at.saturating_add(skew))
}

/// A world parked at its launch barrier, kept so that later seeds start from
/// it without booting. Faults, control and scenarios are not captured: they
/// are empty until the release, which builds them from the run seed. Only
/// the management readiness recorded during boot carries over.
struct WorldSnapshot {
    machines: Vec<MachineSnapshot>,
    events: Vec<BTreeMap<(u64, u64), Payload>>,
    strict_events: Vec<BTreeMap<(u64, u64), Vec<u8>>>,
    seq: u64,
    link_last: BTreeMap<(usize, usize), u64>,
    partitioned: BTreeMap<(usize, usize), bool>,
    dropped: u64,
    delivered: u64,
    link_counts: BTreeMap<(usize, usize), [u64; 3]>,
    semantic_counts: network_counters::NetworkCounters,
    slices: u64,
    readiness: Vec<Value>,
}

impl WorldSnapshot {
    fn take(world: &mut World) -> Result<Self, String> {
        if !world.machines.iter().all(|m| m.parked) {
            return Err("world fork requires every machine at its launch barrier".into());
        }
        Ok(Self {
            machines: world.machines.iter_mut().map(Machine::snapshot).collect::<Result<_, _>>()?,
            events: world.events.clone(),
            strict_events: world.strict_events.clone(),
            seq: world.seq,
            link_last: world.link_last.clone(),
            partitioned: world.partitioned.clone(),
            dropped: world.dropped,
            delivered: world.delivered,
            link_counts: world.link_counts.clone(),
            semantic_counts: world.semantic_counts.clone(),
            slices: world.slices,
            readiness: world.faults.readiness.clone(),
        })
    }

    /// Put the world back to the fork point. Returns RAM pages copied back.
    fn restore(&self, world: &mut World) -> Result<u64, String> {
        let mut pages = 0;
        for (i, snapshot) in self.machines.iter().enumerate() {
            // A machine the last seed crashed is a later incarnation in a new
            // VM: rebuild the first one to restore into.
            if world.machines[i].incarnation > 0 {
                let name = world.machines[i].name.clone();
                let seed = entropy_seed(world.boot_seed, &format!("boot-entropy/{name}"));
                world.machines[i] = boot_machine(&world.kvm, &name, &world.boot[i], world.barrier, seed)?;
            }
            pages += world.machines[i].restore(snapshot)?;
        }
        world.incarnations.iter_mut().for_each(Vec::clear);
        world.paused_ingress.fill(0);
        world.events = self.events.clone();
        world.strict_events = self.strict_events.clone();
        world.seq = self.seq;
        world.link_last = self.link_last.clone();
        world.partitioned = self.partitioned.clone();
        world.dropped = self.dropped;
        world.delivered = self.delivered;
        world.link_counts = self.link_counts.clone();
        world.semantic_counts = self.semantic_counts.clone();
        world.slices = self.slices;
        let names: Vec<String> = world.machines.iter().map(|m| m.name.clone()).collect();
        world.faults = boot_faults(&names, &self.readiness, world.latency)?;
        world.control = control::Control::new(&Value::Null, names, world.latency, world.early)?;
        Ok(pages)
    }
}

/// Run the world until it ends, or with `fork` until every machine is parked
/// at its fork point: the guest's launch barrier, or with `Some(Some(t))` its
/// first deterministic point at or after simulated time `t`. Returns the
/// outcome.
fn simulate(
    world: &mut World,
    cfg: &Value,
    duration: u64,
    end_on: &Option<String>,
    fork: Option<Option<u64>>,
) -> Result<&'static str, String> {
    let until_barrier = fork.is_some();
    let fork_at = fork.flatten();
    let n = world.machines.len();
    let mut stalled_plan = None;
    let mut frames = Vec::new();
    let outcome;
    loop {
        let keys: Vec<u64> = (0..n).map(|m| world.key(m)).collect();
        let (m, &key) = keys
            .iter()
            .enumerate()
            .min_by_key(|(i, k)| (**k, *i))
            .unwrap();
        if until_barrier
            && world.machines.iter().all(|m| m.parked || matches!(m.state, State::Exited(_)))
        {
            if let Some(m) = world.machines.iter().find(|m| !m.parked) {
                return Err(format!("{} exited before the fork point", m.name));
            }
            outcome = "barrier";
            break;
        }
        if fork_at.is_some_and(|t| key >= t) && world.machines[m].pending_io.is_none() {
            world.machines[m].park()?;
            continue;
        }
        let control_boundary = world.control.boundary();
        world.semantic_counts.advance(key.min(control_boundary).min(world.faults.next_boundary()).min(duration));
        if control_boundary <= key && control_boundary < duration {
            if world.machines.iter().any(|m| m.time > control_boundary) {
                return Err("control boundary is in committed machine past".into());
            }
            let exited=world.machines.iter().filter(|m|matches!(m.state,State::Exited(_))).map(|m|m.name.clone()).collect::<Vec<_>>();
            world.control.reject_unavailable(&exited);
            if world
                .control
                .pending
                .as_ref()
                .is_some_and(|p| !p.admitted_done && matches!(p.operation,Some(seedbox_control::Operation::Inspect{})))
            {
                world.control.pending.as_mut().unwrap().result = json!({"services":cfg["control"]["placement"],"machines":world.machines.iter().enumerate().map(|(i,m)|json!({"name":m.name,"state":if matches!(m.state,State::Exited(_)) {format!("{:?}",m.state)} else {world.faults.process_state(i).to_owned()}})).collect::<Vec<_>>(),"active_handles":world.faults.active_handles(),"network_counters":world.counter_snapshot(world.semantic_counts.counts())});
            }
            if let Some(bytes) =
                world
                    .control
                    .apply_boundary(&mut world.faults, control_boundary)?
            {
                let target = world.control.controller.unwrap();
                world.seq += 1;
                world.strict_events[target].insert((control_boundary, world.seq), bytes);
            }
            continue;
        }
        let fault_boundary = world.faults.next_boundary();
        if fault_boundary <= key && fault_boundary < duration {
            let commands = world.faults.apply_due(fault_boundary);
            for i in world.faults.take_changed_network() {
                let snapshot=world.counter_snapshot(world.semantic_counts.counts());world.faults.log[i]["network_counters"]=snapshot;
            }
            for (target, command) in commands {
                if world.machines[target].state == State::Halted && !world.machines[target].paused {
                    world.machines[target].wake(fault_boundary);
                }
                world.seq += 1;
                world.strict_events[target].insert((fault_boundary, world.seq), command);
            }
            // After the commands, so a crash at this boundary discards those
            // its machine was just sent.
            for (target, op) in world.faults.take_lifecycle() {
                world.lifecycle(target, op, fault_boundary)?;
            }
            world
                .control
                .observe_faults(&world.faults, fault_boundary)?;
            if world.faults.error.is_some() && world.control.pending.is_none() {
                outcome = "fault_failed";
                break;
            }
            continue;
        }
        if key == u64::MAX {
            // Paused or down machines with nothing left to resume or reboot
            // them sit out the rest of the run.
            outcome = if world.machines.iter().any(|m| m.paused || m.down) { "duration" } else { "quiescent" };
            break;
        }
        if key >= duration {
            outcome = "duration";
            break;
        }
        if world.machines[m].state == State::Halted {
            world.machines[m].wake(key);
        }
        if let Err(error)=world.deliver_due(m) {world.control.error=Some(error);outcome="fault_failed";break;}
        let horizon = keys
            .iter()
            .enumerate()
            .filter(|(k, _)| *k != m)
            .map(|(_, &k)| k.saturating_add(world.lookahead))
            .min()
            .unwrap_or(u64::MAX);
        let mut limit = horizon
            .min(world.next_event(m))
            .min(duration)
            .min(fault_boundary)
            .min(control_boundary);
        if let Some(t) = fork_at {
            // Stop exactly at the fork time, or as soon after it as owed I/O
            // cost allows.
            let owed = world.machines[m].pending_io.as_ref().map_or(0, |p| p.remaining_ns);
            limit = limit.min(t.max(world.machines[m].time.saturating_add(owed)));
        }
        world.slices += 1;
        // A slice ending at the horizon or an event may stop a little early
        // (at a cheap deterministic point); the run's end is exact.
        let early = if limit == duration
            || fork_at == Some(limit)
            || limit == fault_boundary
            || limit == control_boundary
            || world.strict_events[m]
                .keys()
                .next()
                .is_some_and(|k| k.0 == limit)
        {
            0
        } else {
            world.early
        };
        let plan=(m,world.machines[m].time,limit,horizon,world.next_event(m),fault_boundary,control_boundary,world.machines[m].next_deadline(),world.machines[m].pending_io.as_ref().map_or(0,|p|p.remaining_ns),world.seq);
        let stop = world.machines[m].run_until(&mut world.counter, limit, early, &mut frames)?;
        stop_progress(&mut stalled_plan,plan,world.machines[m].time!=plan.1 || stop!=Stop::Reached)?;
        for (id,at,success) in world.machines[m].fault_effects.drain(..) {
            if id==0 && success {
                if let Err(error)=world.faults.management_ready(m,at,world.control.latency) {world.faults.error=Some(error);}
            } else {world.faults.effect(m,id,at,success);}
        }
        if stop==Stop::Exited {if let Some(id)=world.control.pending.as_ref().and_then(|p|p.fault) {world.faults.target_exited(id,m,world.machines[m].time);}}
        for (at, line) in world.machines[m]
            .control_requests
            .drain(..)
            .collect::<Vec<_>>()
        {
            if until_barrier {
                return Err(format!(
                    "{}: runtime control request at {at} ns, before the fork point; fork earlier (boot.fork_at_ns)",
                    world.machines[m].name
                ));
            }
            let max_time = world.machines.iter().map(|m| m.time).max().unwrap_or(0);
            if let Err(e) = world.control.request(m, at, &line, max_time) {
                world.control.log.push(json!({"status":"rejected","machine":world.machines[m].name,"at_ns":at,"frame":String::from_utf8_lossy(&line),"error":e}));
                world.control.error = Some(e);
            }
        }
        world
            .control
            .observe_faults(&world.faults, world.machines[m].time)?;
        if (world.control.error.is_some()
            || (world.faults.error.is_some() && world.control.pending.is_none()))
            && !world.control.has_undelivered_reply()
        {
            outcome = "fault_failed";
            break;
        }
        let t = world.machines[m].time;
        for f in frames.drain(..) {
            world.route(m, t, f);
        }
        if stop == Stop::Exited && end_on.as_deref() == Some(world.machines[m].name.as_str()) {
            outcome = "end_machine_exited";
            break;
        }
    }
    if until_barrier && outcome != "barrier" {
        return Err(format!("launch barrier not reached: {outcome}"));
    }
    Ok(outcome)
}

/// Finish a run: write consoles and build the engine report.
#[allow(clippy::too_many_arguments)]
fn report(
    world: &mut World,
    outcome: &str,
    seed: u64,
    kills: &[Value],
    host_seconds: f64,
    digest: bool,
    out: &str,
    duration: u64,
    end_on: &Option<String>,
) -> Result<Value, String> {
    let slices = world.slices;
    let cut_off_at=if outcome=="duration" {duration} else if outcome=="end_machine_exited" {end_on.as_ref().and_then(|name|world.machines.iter().find(|m|&m.name==name)).map(|m|m.time).unwrap_or(0)} else {world.machines.iter().map(|m|m.time).max().unwrap_or(0)};
    world.control.cut_off(&world.faults,cut_off_at);
    let fault_error=world.faults.error.clone().or(world.control.error.clone());
    let mut world_witness = Sha256::new();
    if world.control.controller.is_some() {
        world_witness.update(serde_json::to_vec(&world.control.log).map_err(|e| e.to_string())?);
    }
    if !world.faults.log.is_empty() {
        world_witness.update(serde_json::to_vec(&world.faults.log).map_err(|e| e.to_string())?);
        world_witness.update(world.delivered.to_le_bytes());
        world_witness.update(world.dropped.to_le_bytes());
        world_witness.update(serde_json::to_vec(&world.network_snapshot()).map_err(|e| e.to_string())?);
    }
    let mut report = Vec::new();
    for (index, mach) in world.machines.iter_mut().enumerate() {
        if let Some(t) = mach.trace.as_mut() {
            use std::io::Write;
            let _ = t.flush();
        }
        let w = mach.exit_witness();
        world_witness.update(w.as_bytes());
        let (state_digest, state_parts) = if digest {
            let (d, parts) = mach.state_digest();
            if std::env::var("SEEDBOX_PAGE_HASHES").is_ok() {
                std::fs::write(format!("{out}/{}.pages", mach.name), mach.page_hashes())
                    .map_err(|e| e.to_string())?;
            }
            let parts: serde_json::Map<String, Value> = parts
                .into_iter()
                .map(|(k, v)| (k.to_string(), json!(v)))
                .collect();
            (Some(d), Some(parts))
        } else {
            (None, None)
        };
        if let Some(d) = &state_digest {
            world_witness.update(d.as_bytes());
        }
        // A disk outlives its machine's crashes: what it holds is part of
        // the outcome even without --digest.
        if let Some(b) = &mach.blk {
            world_witness.update(b.disk.digest().as_bytes());
        }
        std::fs::write(format!("{out}/{}.console", mach.name), &mach.com1.out)
            .map_err(|e| e.to_string())?;
        std::fs::write(format!("{out}/{}.mgmt", mach.name), &mach.com2.out)
            .map_err(|e| e.to_string())?;
        let s = &mach.stats;
        let mut sk = s.skids.clone();
        sk.sort();
        let skid_p50 = sk.get(sk.len() / 2).copied();
        let skid_p99 = sk.get(sk.len() * 99 / 100).copied();
        let mut row = json!({
            "name": mach.name,
            "state": if mach.down {"Down".into()} else if mach.paused {"Paused".into()} else {format!("{:?}", mach.state)},
            "guest_exit_code": mach.guest_exit_code,
            "guest_init_error": mach.guest_init_error,
            "infrastructure_error": matches!(mach.state, State::Exited(_)) && mach.guest_exit_code.is_none(),
            "time_ns": mach.time,
            "branches": mach.branches,
            "pending_io": mach.pending_io.as_ref().map(|p| json!({"remaining_ns":p.remaining_ns,"direction":if p.write {"out"} else {"in"},"port":p.port,"width":p.width,"count":p.count,"bytes":p.bytes})),
            "witness": w,
            "state_digest": state_digest,
            "state_digest_parts": state_parts,
            "exits": s.exits, "pio": s.pio, "mmio": s.mmio, "halts": s.halts, "irqs": s.irqs,
            "window_exits": s.window_exits, "eintr": s.eintr,
            "precise_stops": s.precise_stops, "steps": s.steps,
            "step_ms": s.step_ns as f64 / 1e6, "run_ms": s.run_ns as f64 / 1e6,
            "skid_max": s.skid_max, "skid_count": s.skids.len(), "final_margin": mach.margin,
            "skid_p50": skid_p50, "skid_p99": skid_p99, "yields": s.yields, "yield_stops": s.yield_stops,
            "soft_misses": s.soft_misses, "soft_misses_clean": s.soft_misses_clean, "soft_undos": s.soft_undos,
            "undo": mach.undo.as_ref().map(|u| json!({
                "checkpoints": u.stats.checkpoints, "rollbacks": u.stats.rollbacks,
                "synced_pages": u.stats.synced_pages, "restored_pages": u.stats.restored_pages,
                "checkpoint_ms": u.stats.checkpoint_ns as f64 / 1e6,
                "rollback_ms": u.stats.rollback_ns as f64 / 1e6,
            })),
            "kvm_ms_by_exit": {"eintr": s.kvm_ns[0] as f64/1e6, "debug": s.kvm_ns[1] as f64/1e6, "pio": s.kvm_ns[2] as f64/1e6, "mmio": s.kvm_ns[3] as f64/1e6, "hlt": s.kvm_ns[4] as f64/1e6, "window": s.kvm_ns[5] as f64/1e6, "other": s.kvm_ns[6] as f64/1e6},
            "kvm_n_by_exit": s.kvm_n,
            "control_channel":mach.channel.as_ref().map(|_|json!({"transport":"virtio-console-v1","tx_bytes":mach.control_tx_bytes,"rx_dma_bytes":mach.control_dma_bytes})),
            "net": mach.net.as_ref().map(|n| json!({"tx": n.tx_frames, "rx": n.rx_frames, "rx_dropped": n.rx_dropped})),
        });
        row["disk"] = json!(mach.blk.as_ref().map(|b| json!({
                "model": b.disk.model(), "capacity_sectors": b.disk.capacity(), "base_sha256": b.disk.base_sha256(),
                "digest": b.disk.digest(), "stable_sectors": b.disk.stable_sectors(), "volatile_sectors": b.disk.volatile_sectors(),
                "reads": b.stats.reads, "writes": b.stats.writes, "flushes": b.stats.flushes,
                "sectors_read": b.stats.sectors_read, "sectors_written": b.stats.sectors_written,
                "sectors_flushed": b.stats.sectors_flushed, "errors": b.stats.errors,
            })));
        row["incarnation"] = json!(mach.incarnation);
        row["booted_at_ns"] = json!(mach.booted_at);
        row["crashed_incarnations"] = json!(world.incarnations[index]);
        report.push(row);
    }
    let sim_ns = world.machines.iter().map(|m| m.time).max().unwrap_or(0);
    Ok(json!({
        "report_version": 1,
        "seed": seed,
        "counter_profile": profile::host()?.name,
        "outcome": outcome,
        "sim_seconds": sim_ns as f64 / 1e9,
        "host_seconds": host_seconds,
        "speed": (sim_ns as f64 / 1e9) / host_seconds,
        "slices": slices,
        "frames_delivered": world.delivered,
        "frames_dropped": world.dropped, "network_counters": world.network_snapshot(),
        "world_witness": format!("{:x}", world_witness.finalize()),
        "kills": kills,
        "faults": world.faults.log, "fault_error":fault_error,
        "control_history":world.control.log,"control_latency_ns":world.control.latency,
        "rejected_fault_effects":world.faults.rejected_effects,"management_readiness":world.faults.readiness,
        "in_flight_policy":"retain_already_routed; drop_new_sends_while_any_handle_active",
        "machines": report,
    }))
}

fn main() {
    match run() {
        Ok(v) => println!("{}", serde_json::to_string_pretty(&v).unwrap()),
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
    }
}


#[cfg(test)]
mod progress_tests {
    use super::*;

    #[test]
    fn a_forked_world_rejects_plan_times_before_the_release() {
        let cfg=json!({
            "faults":[{"id":1,"at_ns":1_000_000_000}],
            "scenario":[{"at_ns":1_500_000_000,"serial":"x\n","machine":"a"}],
            "kill_restart":{"start_ns":2_000_000_000},
        });
        assert_eq!(check_plan_after(&cfg,1_400_000_000).unwrap_err(),
            "faults[0].at_ns 1000000000 is before the fork point 1400000000; in a forked world it must be at or after the release (boot.fork_at_ns or the launch barrier, plus boot.release_skew_ns)");
        assert!(check_plan_after(&cfg,1_000_000_000).is_ok());
        let later=|key:&str| {let mut c=cfg.clone();c.as_object_mut().unwrap().remove(key);c};
        assert!(check_plan_after(&later("faults"),1_600_000_000).unwrap_err().starts_with("scenario[0].at_ns 1500000000 "));
        let kills=json!({"kill_restart":cfg["kill_restart"]});
        assert!(check_plan_after(&kills,2_000_000_001).unwrap_err().starts_with("kill_restart.start_ns 2000000000 "));
        assert!(check_plan_after(&json!({}),u64::MAX).is_ok());
    }

    /// A Compose guest reports management readiness while it boots; with a
    /// time fork that is before the release. The boot world accepts it, and
    /// the seed's plan starts with that machine already ready.
    #[test]
    fn readiness_signalled_before_the_fork_carries_over_to_the_seed() {
        let names=vec!["a".to_string(),"b".to_string()];
        let mut boot=boot_faults(&names,&[],1000).unwrap();
        boot.management_ready(1,700,1000).unwrap();
        let restored=boot_faults(&names,&boot.readiness,1000).unwrap();
        assert_eq!(restored.readiness,boot.readiness);
        let mut seed=faults::Faults::new(Vec::new(),names.clone()).unwrap();
        seed.require_readiness();
        replay_readiness(&mut seed,&names,&restored.readiness,1000).unwrap();
        assert_eq!(seed.readiness,vec![json!({"machine":"b","at_ns":700})]);
        assert!(seed.management_ready(1,2000,1000).is_err(),"b is already ready");
        assert!(seed.management_ready(0,2000,1000).is_ok());
    }
    #[test]
    fn equal_time_delivery_can_replan_but_an_unchanged_plan_cannot_loop() {
        let mut previous=None;
        let plan=(0,1000,1000,1000,1000,1000,1000,2000,500,1);
        assert!(stop_progress(&mut previous,plan,false).is_ok());
        let delivered=(0,1000,1000,1000,2000,2000,2000,2000,500,2);
        assert!(stop_progress(&mut previous,delivered,false).is_ok());
        assert!(stop_progress(&mut previous,delivered,false).is_err());
        assert!(stop_progress(&mut previous,delivered,true).is_ok());
        assert_eq!(previous,None);
        assert!(stop_progress(&mut previous,delivered,false).is_ok());
    }
}
