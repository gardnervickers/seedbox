//! Seedbox world: several Linux machines, a network fabric and a scenario,
//! all executed by one host thread. A seed and the config fully determine the run.

mod boundary;
mod control;
mod counter;
mod devices;
mod events;
mod faults;
mod machine;
mod network_counters;
mod rng;
mod stepper;
mod undo;
mod virtio_net;
mod virtio_console;

use counter::Counter;
use machine::{BootSpec, Machine, State, Stop, NET_IRQ, NET_MMIO, CHANNEL_IRQ, CHANNEL_MMIO};
use rng::Rng;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::time::Instant;

thread_local! {
    pub static START: Instant = Instant::now();
}

enum Payload {
    Frame { from: usize, bytes: Vec<u8> },
    Serial(Vec<u8>),
    Partition { from: usize, to: usize, on: bool },
}

struct World {
    machines: Vec<Machine>,
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
        let now = self.machines[m].time + self.early;
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

    let kvm = kvm_ioctls::Kvm::new().map_err(|e| format!("kvm: {e}"))?;
    let mut machines = Vec::new();
    let mut macs = Vec::new();
    let mut initrds: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    for (i, mc) in cfg["machines"].as_array().unwrap().iter().enumerate() {
        let ipath = path(&mc["initrd"]).to_string_lossy().to_string();
        if !initrds.contains_key(&ipath) {
            initrds.insert(
                ipath.clone(),
                std::fs::read(&ipath).map_err(|e| format!("{ipath}: {e}"))?,
            );
        }
        let mac = [0x52, 0x54, 0x00, 0x00, 0x00, i as u8 + 1];
        macs.push(mac);
        let mut rng = Rng::new(seed, &format!("boot-entropy/{}", names[i]));
        let mut rng_seed = [0u8; 32];
        for c in rng_seed.chunks_mut(8) {
            c.copy_from_slice(&rng.next().to_le_bytes());
        }
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
        if names.get(i).is_some_and(|name|cfg["control"]["machine"].as_str()==Some(name.as_str())) {cmdline+=" seedbox.control=1";}
        if let Some(extra) = mc["cmdline"].as_str() {
            cmdline += " ";
            cmdline += extra;
        }
        let spec = BootSpec {
            kernel: &kernel,
            initrd: &initrds[&ipath],
            cmdline,
            mem,
            rng_seed,
            mac: networked.then_some(mac),
            control_channel,
            undo_margin,
        };
        let mut mach = Machine::new(&kvm, &names[i], &spec)?;
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
        if let Some(limit) = arg(&args, "--console-limit") {
            let limit = limit
                .parse::<usize>()
                .map_err(|error| format!("console limit: {error}"))?;
            mach.com1.output_limit = Some(limit);
            mach.com2.output_limit = Some(limit);
        }
        mach.tolerance = cfg["stop_tolerance_ns"].as_u64().unwrap_or(0);
        // Performance-only experiment knobs for the stop approach.
        if let Ok(v) = std::env::var("SEEDBOX_CHECKPOINT_AT") {
            mach.checkpoint_at = v.parse().map_err(|e| format!("SEEDBOX_CHECKPOINT_AT: {e}"))?;
        }
        mach.fm_learn = std::env::var("SEEDBOX_FM_LEARN").is_ok_and(|v| v == "1");
        mach.timer_jitter = cfg["timer_jitter_ns"].as_u64().unwrap_or(0);
        mach.exact_timer_ppm = cfg["exact_timer_ppm"].as_u64().unwrap_or(0);
        mach.timer_rng = Some(Rng::new(seed, &format!("timer-jitter/{}", names[i])));
        if args.iter().any(|a| a == "--trace") {
            let f = std::fs::File::create(format!("{out}/{}.trace", names[i]))
                .map_err(|e| e.to_string())?;
            mach.trace = Some(std::io::BufWriter::new(f));
        }
        match std::env::var("SEEDBOX_MARGIN").as_deref() {
            Ok("adaptive") | Err(_) => mach.adaptive_margin = true,
            Ok(m) => mach.margin = m.parse().map_err(|e| format!("margin: {e}"))?,
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
    let mut world = World {
        counter: Counter::new()?,
        events: (0..n).map(|_| BTreeMap::new()).collect(),
        seq: 0,
        lookahead: net["latency_ns"].as_u64().unwrap_or(1_000_000),
        early: (net["latency_ns"].as_u64().unwrap_or(1_000_000) / 2)
            .min(cfg["stop_tolerance_ns"].as_u64().unwrap_or(0)),
        latency: net["latency_ns"].as_u64().unwrap_or(1_000_000),
        jitter: net["jitter_ns"].as_u64().unwrap_or(0),
        link_rng: BTreeMap::new(),
        link_last: BTreeMap::new(),
        partitioned: BTreeMap::new(),
        faults: faults::Faults::new(
            cfg["faults"].as_array().cloned().unwrap_or_default(),
            names.clone(),
        )?,
        control: control::Control::new(
            &cfg["control"],
            names.clone(),
            net["latency_ns"].as_u64().unwrap_or(1_000_000),
            (net["latency_ns"].as_u64().unwrap_or(1_000_000) / 2)
                .min(cfg["stop_tolerance_ns"].as_u64().unwrap_or(0)),
        )?,
        strict_events: (0..n).map(|_| BTreeMap::new()).collect(),
        macs,
        dropped: 0,
        delivered: 0,
        link_counts: BTreeMap::new(),
        semantic_counts:network_counters::NetworkCounters::default(),
        machines,
    };
    world.faults.require_readiness();
    for a in 0..n {
        for b in 0..n {
            if a != b {
                world.link_rng.insert(
                    (a, b),
                    Rng::new(seed, &format!("link/{}/{}", names[a], names[b])),
                );
            }
        }
    }
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

    let started = Instant::now();
    let mut slices = 0u64;
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
                if world.machines[target].state == State::Halted {
                    world.machines[target].wake(fault_boundary);
                }
                world.seq += 1;
                world.strict_events[target].insert((fault_boundary, world.seq), command);
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
            outcome = "quiescent";
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
        let limit = horizon
            .min(world.next_event(m))
            .min(duration)
            .min(fault_boundary)
            .min(control_boundary);
        slices += 1;
        // A slice ending at the horizon or an event may stop a little early
        // (at a cheap deterministic point); the run's end is exact.
        let early = if limit == duration
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
    let host_seconds = started.elapsed().as_secs_f64();

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
    for mach in world.machines.iter_mut() {
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
        std::fs::write(format!("{out}/{}.console", mach.name), &mach.com1.out)
            .map_err(|e| e.to_string())?;
        std::fs::write(format!("{out}/{}.mgmt", mach.name), &mach.com2.out)
            .map_err(|e| e.to_string())?;
        let s = &mach.stats;
        let mut sk = s.skids.clone();
        sk.sort();
        let skid_p50 = sk.get(sk.len() / 2).copied();
        let skid_p99 = sk.get(sk.len() * 99 / 100).copied();
        let approach = json!({
            "mid_approaches": s.mid_approaches, "final_overflows": s.final_overflows,
            "final_skid_mean": if s.final_overflows > 0 { s.final_skid_sum / s.final_overflows } else { 0 },
            "checkpoint_at": mach.checkpoint_at, "fm_learn": mach.fm_learn,
        });
        let mut entry = json!({
            "name": mach.name,
            "state": format!("{:?}", mach.state),
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
        entry["approach"] = approach;
        report.push(entry);
    }
    let sim_ns = world.machines.iter().map(|m| m.time).max().unwrap_or(0);
    Ok(json!({
        "report_version": 1,
        "seed": seed,
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
