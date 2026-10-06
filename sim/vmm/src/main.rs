//! Seedbox world: several Linux machines, a network fabric and a scenario,
//! all executed by one host thread. A seed and the config fully determine the run.

mod counter;
mod devices;
mod machine;
mod rng;
mod stepper;
mod virtio_net;

use counter::Counter;
use machine::{BootSpec, Machine, State, Stop, NET_IRQ, NET_MMIO};
use rng::Rng;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::time::Instant;

thread_local! {
    pub static START: Instant = Instant::now();
}

enum Payload {
    Frame(Vec<u8>),
    Serial(Vec<u8>),
    Partition { from: usize, to: usize, on: bool },
}

struct World {
    machines: Vec<Machine>,
    counter: Counter,
    events: Vec<BTreeMap<(u64, u64), Payload>>,
    seq: u64,
    lookahead: u64,
    latency: u64,
    jitter: u64,
    link_rng: BTreeMap<(usize, usize), Rng>,
    link_last: BTreeMap<(usize, usize), u64>,
    partitioned: BTreeMap<(usize, usize), bool>,
    macs: Vec<[u8; 6]>,
    dropped: u64,
    delivered: u64,
}

impl World {
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

    fn deliver_due(&mut self, m: usize) {
        let now = self.machines[m].time;
        while let Some((&k, _)) = self.events[m].iter().next() {
            if k.0 > now {
                break;
            }
            let p = self.events[m].remove(&k).unwrap();
            let mach = &mut self.machines[m];
            match p {
                Payload::Frame(f) => {
                    if !mach.deliver_frame(&f) {
                        self.dropped += 1;
                    } else {
                        self.delivered += 1;
                    }
                }
                Payload::Serial(b) => mach.deliver_serial(&b),
                Payload::Partition { from, to, on } => {
                    self.partitioned.insert((from, to), on);
                }
            }
        }
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
            if *self.partitioned.get(&(src, dst)).unwrap_or(&false) {
                self.dropped += 1;
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
            self.schedule(dst, t, Payload::Frame(frame.clone()));
        }
    }
}

fn arg(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1).cloned())
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
        };
        let mut mach = Machine::new(&kvm, &names[i], &spec)?;
        mach.tolerance = cfg["stop_tolerance_ns"].as_u64().unwrap_or(0);
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
    let n = machines.len();
    let net = &cfg["network"];
    let mut world = World {
        counter: Counter::new()?,
        events: (0..n).map(|_| BTreeMap::new()).collect(),
        seq: 0,
        lookahead: net["latency_ns"].as_u64().unwrap_or(1_000_000),
        latency: net["latency_ns"].as_u64().unwrap_or(1_000_000),
        jitter: net["jitter_ns"].as_u64().unwrap_or(0),
        link_rng: BTreeMap::new(),
        link_last: BTreeMap::new(),
        partitioned: BTreeMap::new(),
        macs,
        dropped: 0,
        delivered: 0,
        machines,
    };
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
    let mut frames = Vec::new();
    let outcome;
    loop {
        let keys: Vec<u64> = (0..n).map(|m| world.key(m)).collect();
        let (m, &key) = keys
            .iter()
            .enumerate()
            .min_by_key(|(i, k)| (**k, *i))
            .unwrap();
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
        world.deliver_due(m);
        let horizon = keys
            .iter()
            .enumerate()
            .filter(|(k, _)| *k != m)
            .map(|(_, &k)| k.saturating_add(world.lookahead))
            .min()
            .unwrap_or(u64::MAX);
        let limit = horizon.min(world.next_event(m)).min(duration);
        slices += 1;
        let stop = world.machines[m].run_until(&mut world.counter, limit, &mut frames)?;
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

    let mut world_witness = Sha256::new();
    let mut report = Vec::new();
    for mach in world.machines.iter_mut() {
        if let Some(t) = mach.trace.as_mut() {
            use std::io::Write;
            let _ = t.flush();
        }
        let w = format!("{:x}", mach.witness.clone().finalize());
        world_witness.update(w.as_bytes());
        let state_digest = if digest {
            Some(mach.state_digest())
        } else {
            None
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
        report.push(json!({
            "name": mach.name,
            "state": format!("{:?}", mach.state),
            "time_ns": mach.time,
            "branches": mach.branches,
            "witness": w,
            "state_digest": state_digest,
            "exits": s.exits, "pio": s.pio, "mmio": s.mmio, "halts": s.halts, "irqs": s.irqs,
            "window_exits": s.window_exits, "eintr": s.eintr,
            "precise_stops": s.precise_stops, "steps": s.steps,
            "step_ms": s.step_ns as f64 / 1e6, "run_ms": s.run_ns as f64 / 1e6,
            "skid_max": s.skid_max, "skid_count": s.skids.len(), "final_margin": mach.margin,
            "skid_p50": skid_p50, "skid_p99": skid_p99, "yields": s.yields, "yield_stops": s.yield_stops,
            "kvm_ms_by_exit": {"eintr": s.kvm_ns[0] as f64/1e6, "debug": s.kvm_ns[1] as f64/1e6, "pio": s.kvm_ns[2] as f64/1e6, "mmio": s.kvm_ns[3] as f64/1e6, "hlt": s.kvm_ns[4] as f64/1e6, "window": s.kvm_ns[5] as f64/1e6, "other": s.kvm_ns[6] as f64/1e6},
            "kvm_n_by_exit": s.kvm_n,
            "net": mach.net.as_ref().map(|n| json!({"tx": n.tx_frames, "rx": n.rx_frames, "rx_dropped": n.rx_dropped})),
        }));
    }
    let sim_ns = world.machines.iter().map(|m| m.time).max().unwrap_or(0);
    Ok(json!({
        "seed": seed,
        "outcome": outcome,
        "sim_seconds": sim_ns as f64 / 1e9,
        "host_seconds": host_seconds,
        "speed": (sim_ns as f64 / 1e9) / host_seconds,
        "slices": slices,
        "frames_delivered": world.delivered,
        "frames_dropped": world.dropped,
        "world_witness": format!("{:x}", world_witness.finalize()),
        "kills": kills,
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
