//! What a run directory records about machines, links and faults, and the world
//! state those records imply at a simulated time. Pure file reading: nothing here
//! touches a terminal, and nothing here is part of any run's identity.
use serde_json::Value;
use std::{
    collections::BTreeMap,
    fs,
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
};

/// A directed (from, to) machine pair, by index into `Run::machines`.
pub type Pair = (usize, usize);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Policy {
    pub latency_ns: u64,
    pub jitter_ns: u64,
    pub loss_ppm: u64,
    pub bandwidth_bps: Option<u64>,
}

impl Policy {
    fn parse(v: &Value, latency_default: u64) -> Self {
        Self {
            latency_ns: v["latency_ns"].as_u64().unwrap_or(latency_default),
            jitter_ns: v["jitter_ns"].as_u64().unwrap_or(0),
            loss_ppm: v["loss_ppm"].as_u64().unwrap_or(0),
            bandwidth_bps: v["bandwidth_bps"].as_u64(),
        }
    }
}

/// Frame outcomes for one directed pair, as the engine counts them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Counts {
    pub delivered: u64,
    pub partition_dropped: u64,
    pub receiver_dropped: u64,
    pub loss_dropped: u64,
    pub queue_dropped: u64,
}

impl Counts {
    fn parse(v: &Value) -> Self {
        let n = |k: &str| v[k].as_u64().unwrap_or(0);
        Self {
            delivered: n("delivered"),
            partition_dropped: n("partition_dropped"),
            receiver_dropped: n("receiver_dropped"),
            loss_dropped: n("loss_dropped"),
            queue_dropped: n("queue_dropped"),
        }
    }
    pub fn dropped(&self) -> u64 {
        self.partition_dropped + self.receiver_dropped + self.loss_dropped + self.queue_dropped
    }
    fn since(&self, earlier: &Counts) -> Counts {
        Counts {
            delivered: self.delivered.saturating_sub(earlier.delivered),
            partition_dropped: self
                .partition_dropped
                .saturating_sub(earlier.partition_dropped),
            receiver_dropped: self
                .receiver_dropped
                .saturating_sub(earlier.receiver_dropped),
            loss_dropped: self.loss_dropped.saturating_sub(earlier.loss_dropped),
            queue_dropped: self.queue_dropped.saturating_sub(earlier.queue_dropped),
        }
    }
}

/// One fault row from the engine report (or, before the report exists, one
/// planned fault from `world.json`, which is never applied).
#[derive(Clone, Debug)]
pub struct Fault {
    pub id: u64,
    pub action: String,
    pub status: String,
    pub at_ns: Option<u64>,
    pub applied_at_ns: Option<u64>,
    pub request: Value,
    /// Per-pair counts of frames sent strictly before this fault's boundary.
    pub counts: Option<BTreeMap<Pair, Counts>>,
    pub offset_ns: Option<i64>,
}

impl Fault {
    pub fn handle(&self) -> Option<&str> {
        self.request["handle"].as_str()
    }
    pub fn machine(&self) -> Option<&str> {
        self.request["machine"].as_str()
    }
    pub fn time_ns(&self) -> Option<u64> {
        self.applied_at_ns.or(self.at_ns)
    }
    /// A short description of what the fault does.
    pub fn summary(&self) -> String {
        let r = &self.request;
        let names = |k: &str| {
            r[k].as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(Value::as_str)
                        .collect::<Vec<_>>()
                        .join(",")
                })
                .unwrap_or_default()
        };
        let handle = self.handle().map(|h| format!(" [{h}]")).unwrap_or_default();
        match self.action.as_str() {
            "shape" => {
                let p = Policy::parse(r, 0);
                let extra = if r.get("network_faults").is_some() {
                    " +faults"
                } else {
                    ""
                };
                format!(
                    "shape {}→{} {}{extra}{handle}",
                    names("from"),
                    names("to"),
                    policy_text(&p)
                )
            }
            "partition" => format!("partition {} | {}{handle}", names("left"), names("right")),
            "heal" => format!("heal{handle}"),
            "clock_skew" => {
                let offset = self.offset_ns.or(r["offset_ns"].as_i64());
                let by = match (offset, r["max_offset_ns"].as_u64()) {
                    (Some(o), _) => signed_duration(o),
                    (None, Some(m)) => format!("±{}", duration(m)),
                    _ => String::new(),
                };
                format!("clock_skew {} {by}{handle}", self.machine().unwrap_or("?"))
            }
            "throttle" => format!(
                "throttle {} x{:.2}{handle}",
                self.machine().unwrap_or("?"),
                r["factor_permille"].as_u64().unwrap_or(1000) as f64 / 1000.0
            ),
            "drop_connection" => format!("drop_connection{handle}"),
            action => match self.machine() {
                Some(m) => format!("{action} {m}"),
                None => action.to_owned(),
            },
        }
    }
}

/// A process kill and restart from the world's `kill_restart` plan.
#[derive(Clone, Debug)]
pub struct Kill {
    pub machine: usize,
    pub stop_ns: u64,
    pub start_ns: u64,
}

/// One committed-time console chunk: bytes [offset, offset+length) of the raw
/// stream, first committed at `time_ns`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Chunk {
    pub time_ns: u64,
    pub incarnation: u64,
    pub offset: u64,
    pub length: u64,
}

#[derive(Clone, Debug)]
pub struct Console {
    pub path: PathBuf,
    /// None when the stream has no committed-time index (older or raw runs).
    pub chunks: Option<Vec<Chunk>>,
}

const CHUNK_MAGIC: &[u8; 8] = b"SBLOG001";

impl Console {
    fn open(machines: &Path, name: &str) -> Option<Self> {
        let path = machines.join(format!("{name}.console"));
        if !path.is_file() {
            return None;
        }
        let chunks = fs::read(machines.join(format!("{name}.console.chunks")))
            .ok()
            .and_then(|b| parse_chunks(&b));
        Some(Self { path, chunks })
    }

    /// The last `max_bytes` of console output committed at or before `at_ns`.
    pub fn tail(&self, at_ns: u64, max_bytes: u64) -> std::io::Result<Vec<u8>> {
        let end = match &self.chunks {
            Some(chunks) => chunks
                .iter()
                .take_while(|c| c.time_ns <= at_ns)
                .last()
                .map_or(0, |c| c.offset + c.length),
            None => fs::metadata(&self.path)?.len(),
        };
        let start = end.saturating_sub(max_bytes);
        let mut file = fs::File::open(&self.path)?;
        file.seek(SeekFrom::Start(start))?;
        let mut bytes = Vec::new();
        file.take(end - start).read_to_end(&mut bytes)?;
        Ok(bytes)
    }
}

/// Parses a `.chunks` index: an 8-byte magic, then 40-byte little-endian
/// records of (time, incarnation, order, offset, length).
pub fn parse_chunks(bytes: &[u8]) -> Option<Vec<Chunk>> {
    let body = bytes.strip_prefix(CHUNK_MAGIC)?;
    let (records, rest) = body.as_chunks::<40>();
    if !rest.is_empty() {
        return None;
    }
    let mut chunks: Vec<Chunk> = records
        .iter()
        .map(|r| {
            let words = r.as_chunks::<8>().0;
            let f = |i: usize| u64::from_le_bytes(words[i]);
            Chunk {
                time_ns: f(0),
                incarnation: f(1),
                offset: f(3),
                length: f(4),
            }
        })
        .collect();
    chunks.sort_by_key(|c| c.offset);
    Some(chunks)
}

pub struct Run {
    /// The run's `output` directory.
    pub output: PathBuf,
    pub machines: Vec<String>,
    pub seed: Option<u64>,
    pub outcome: Option<String>,
    /// Simulated end of the run, or the latest recorded time while it runs.
    pub end_ns: u64,
    pub global: Policy,
    pub baselines: BTreeMap<Pair, Policy>,
    pub faults: Vec<Fault>,
    pub kills: Vec<Kill>,
    pub final_counts: Option<BTreeMap<Pair, Counts>>,
    pub final_states: Vec<Option<String>>,
    pub consoles: Vec<Option<Console>>,
    /// Things the run directory does not (yet) record.
    pub notes: Vec<String>,
}

fn read_json(path: &Path) -> Result<Option<Value>, String> {
    match fs::read(path) {
        Ok(b) if b.is_empty() => Ok(None),
        Ok(b) => serde_json::from_slice(&b)
            .map(Some)
            .map_err(|e| format!("{}: {e}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}

/// The `output` directory for a run directory, its `output` directory, or a
/// `vmm.json` inside it.
pub fn output_directory(path: &Path) -> PathBuf {
    if path.is_file() {
        return path.parent().unwrap_or(Path::new(".")).to_owned();
    }
    let nested = path.join("output");
    if nested.is_dir() {
        nested
    } else {
        path.to_owned()
    }
}

impl Run {
    pub fn load(path: &Path) -> Result<Self, String> {
        let output = output_directory(path);
        let world = read_json(&output.join("world.json"))?.unwrap_or(Value::Null);
        let report = read_json(&output.join("vmm.json"))?;
        let mut notes = Vec::new();

        let names_in = |v: &Value| -> Vec<String> {
            v["machines"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|m| m["name"].as_str().map(str::to_owned))
                        .collect()
                })
                .unwrap_or_default()
        };
        let mut machines = names_in(&world);
        if machines.is_empty() {
            machines = report.as_ref().map(names_in).unwrap_or_default();
        }
        if machines.is_empty() {
            return Err(format!(
                "{}: no world.json or vmm.json naming machines; pass a run directory",
                output.display()
            ));
        }
        let index = |name: &str| machines.iter().position(|m| m == name);
        let pairs = |v: &Value| -> BTreeMap<Pair, Counts> {
            v.as_object()
                .map(|o| {
                    o.iter()
                        .filter_map(|(k, c)| {
                            let (from, to) = k.split_once("->")?;
                            Some(((index(from)?, index(to)?), Counts::parse(c)))
                        })
                        .collect()
                })
                .unwrap_or_default()
        };

        let net = &world["network"];
        let global = Policy::parse(net, 1_000_000);
        let baselines = net["links"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|l| {
                let pair = (index(l["from"].as_str()?)?, index(l["to"].as_str()?)?);
                Some((pair, Policy::parse(l, global.latency_ns)))
            })
            .collect();

        let mut faults: Vec<Fault> = match &report {
            Some(r) => r["faults"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|row| Fault {
                    id: row["id"].as_u64().unwrap_or(0),
                    action: row["action"].as_str().unwrap_or("?").to_owned(),
                    status: row["status"].as_str().unwrap_or("?").to_owned(),
                    at_ns: row["requested_at_ns"]
                        .as_u64()
                        .or(row["request"]["at_ns"].as_u64()),
                    applied_at_ns: row["applied_at_ns"].as_u64(),
                    request: row["request"].clone(),
                    counts: row.get("network_counters").map(&pairs),
                    offset_ns: row["offset_ns"].as_i64(),
                })
                .collect(),
            None => world["faults"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|plan| Fault {
                    id: plan["id"].as_u64().unwrap_or(0),
                    action: plan["action"].as_str().unwrap_or("?").to_owned(),
                    status: "planned".into(),
                    at_ns: plan["at_ns"].as_u64(),
                    applied_at_ns: None,
                    request: plan.clone(),
                    counts: None,
                    offset_ns: None,
                })
                .collect(),
        };
        faults.sort_by_key(|f| (f.time_ns().unwrap_or(u64::MAX), f.id));

        let kills = report
            .as_ref()
            .and_then(|r| r["kills"].as_array())
            .into_iter()
            .flatten()
            .filter_map(|k| {
                Some(Kill {
                    machine: index(k["member"].as_str()?)?,
                    stop_ns: k["stop_ns"].as_u64()?,
                    start_ns: k["start_ns"].as_u64()?,
                })
            })
            .collect();

        let machine_dir = output.join("machines");
        let consoles: Vec<_> = machines
            .iter()
            .map(|m| Console::open(&machine_dir, m))
            .collect();

        let mut final_states = vec![None; machines.len()];
        let (seed, outcome, final_counts, mut end_ns);
        match &report {
            Some(r) => {
                seed = r["seed"].as_u64();
                outcome = r["outcome"].as_str().map(str::to_owned);
                final_counts = r.get("network_counters").map(&pairs);
                end_ns = 0;
                for m in r["machines"].as_array().into_iter().flatten() {
                    if let Some(i) = m["name"].as_str().and_then(index) {
                        final_states[i] = m["state"].as_str().map(str::to_owned);
                    }
                    end_ns = end_ns.max(m["time_ns"].as_u64().unwrap_or(0));
                }
                if end_ns == 0 {
                    end_ns = (r["sim_seconds"].as_f64().unwrap_or(0.0) * 1e9) as u64;
                }
            }
            None => {
                notes.push("no engine report yet: run in progress, cancelled or failed".into());
                seed = None;
                outcome = None;
                final_counts = None;
                end_ns = consoles
                    .iter()
                    .flatten()
                    .filter_map(|c| c.chunks.as_ref()?.last().map(|c| c.time_ns))
                    .max()
                    .unwrap_or(0);
            }
        }
        end_ns = end_ns.max(
            faults
                .iter()
                .filter_map(|f| f.applied_at_ns)
                .max()
                .unwrap_or(0),
        );
        if report.is_some() {
            notes.push(
                "traffic is averaged between fault boundaries; per-packet latency is not recorded"
                    .into(),
            );
        }
        if consoles.iter().flatten().any(|c| c.chunks.is_none()) {
            notes.push("some consoles have no committed-time index; they show all output".into());
        }

        Ok(Self {
            output,
            machines,
            seed,
            outcome,
            end_ns,
            global,
            baselines,
            faults,
            kills,
            final_counts,
            final_states,
            consoles,
            notes,
        })
    }

    /// Times worth stopping at: the start, every applied fault, kill edges and the end.
    pub fn stops(&self) -> Vec<u64> {
        let mut stops = vec![0, self.end_ns];
        stops.extend(self.faults.iter().filter_map(|f| f.applied_at_ns));
        for k in &self.kills {
            stops.extend([k.stop_ns, k.start_ns]);
        }
        stops.retain(|&t| t <= self.end_ns);
        stops.sort_unstable();
        stops.dedup();
        stops
    }

    /// Cumulative counter snapshots in time order, starting from zero at 0.
    fn boundaries(&self) -> Vec<(u64, &BTreeMap<Pair, Counts>)> {
        static EMPTY: BTreeMap<Pair, Counts> = BTreeMap::new();
        let mut out = vec![(0, &EMPTY)];
        for f in &self.faults {
            if let (Some(t), Some(c)) = (f.applied_at_ns, &f.counts) {
                if out.last().is_some_and(|&(last, _)| t > last) {
                    out.push((t, c));
                }
            }
        }
        if let Some(c) = &self.final_counts {
            if self.end_ns > out.last().map_or(0, |b| b.0) {
                out.push((self.end_ns, c));
            }
        }
        out
    }

    pub fn snapshot(&self, at_ns: u64) -> Snapshot {
        let n = self.machines.len();
        let index = |name: &str| self.machines.iter().position(|m| m == name);
        let group = |v: &Value| -> Vec<usize> {
            v.as_array()
                .into_iter()
                .flatten()
                .filter_map(|s| index(s.as_str()?))
                .collect()
        };
        // Shapes and cuts live by handle until healed, in fault-ID order.
        let mut shapes: Vec<(u64, String, Vec<Pair>, Policy, bool)> = Vec::new();
        let mut cuts: Vec<(String, Vec<Pair>)> = Vec::new();
        let mut machines: Vec<MachineState> = (0..n).map(|_| MachineState::default()).collect();
        let mut machine_effects: Vec<(String, usize, String)> = Vec::new();
        for f in &self.faults {
            let Some(t) = f.applied_at_ns else { continue };
            if t > at_ns || f.status == "failed" {
                continue;
            }
            let r = &f.request;
            let handle = f.handle().unwrap_or("").to_owned();
            let target = f.machine().and_then(index);
            match f.action.as_str() {
                "shape" => {
                    let pairs = cross(&group(&r["from"]), &group(&r["to"]));
                    let extra = r.get("network_faults").is_some();
                    shapes.push((f.id, handle, pairs, Policy::parse(r, 0), extra));
                }
                "partition" => {
                    let (l, rr) = (group(&r["left"]), group(&r["right"]));
                    let mut pairs = cross(&l, &rr);
                    pairs.extend(cross(&rr, &l));
                    cuts.push((handle, pairs));
                }
                "heal" => {
                    shapes.retain(|s| s.1 != handle);
                    cuts.retain(|c| c.0 != handle);
                    machine_effects.retain(|e| e.0 != handle);
                }
                "clock_skew" | "throttle" => {
                    if let Some(m) = target {
                        let what = f.summary();
                        let what = what.split(" [").next().unwrap_or("").to_owned();
                        machine_effects.push((handle, m, what));
                    }
                }
                action => {
                    if let Some(m) = target {
                        let s = &mut machines[m];
                        s.life = match action {
                            "crash" => "down",
                            "reboot" => {
                                s.incarnation += 1;
                                "running"
                            }
                            "pause" => "paused",
                            "stop" => "stopped",
                            "pause_process" => "frozen",
                            "resume" | "restart" | "resume_process" => "running",
                            _ => s.life,
                        };
                    }
                }
            }
        }
        for k in &self.kills {
            if k.stop_ns <= at_ns && at_ns < k.start_ns {
                machines[k.machine].life = "killed";
            }
        }
        for (_, m, what) in machine_effects {
            machines[m].effects.push(what);
        }
        if at_ns >= self.end_ns {
            for (s, last) in machines.iter_mut().zip(&self.final_states) {
                s.final_state = last.clone();
            }
        }

        let mut links = BTreeMap::new();
        for from in 0..n {
            for to in 0..n {
                if from == to {
                    continue;
                }
                let pair = (from, to);
                let state = if let Some((h, _)) = cuts.iter().find(|c| c.1.contains(&pair)) {
                    LinkState::Cut { handle: h.clone() }
                } else if let Some(s) = shapes
                    .iter()
                    .filter(|s| s.2.contains(&pair))
                    .max_by_key(|s| s.0)
                {
                    LinkState::Shaped {
                        policy: s.3,
                        handle: s.1.clone(),
                        network_faults: s.4,
                    }
                } else {
                    LinkState::Normal(self.baselines.get(&pair).copied().unwrap_or(self.global))
                };
                links.insert(pair, state);
            }
        }

        let bounds = self.boundaries();
        let probe = at_ns.min(self.end_ns.saturating_sub(1));
        let interval = bounds
            .windows(2)
            .find(|w| w[0].0 <= probe && probe < w[1].0)
            .map(|w| {
                let traffic = w[1]
                    .1
                    .iter()
                    .map(|(p, c)| (*p, c.since(w[0].1.get(p).unwrap_or(&Counts::default()))))
                    .collect();
                Interval {
                    start_ns: w[0].0,
                    end_ns: w[1].0,
                    traffic,
                }
            });

        Snapshot {
            links,
            machines,
            interval,
        }
    }
}

fn cross(a: &[usize], b: &[usize]) -> Vec<Pair> {
    a.iter()
        .flat_map(|&x| b.iter().map(move |&y| (x, y)))
        .filter(|(x, y)| x != y)
        .collect()
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LinkState {
    Normal(Policy),
    Shaped {
        policy: Policy,
        handle: String,
        network_faults: bool,
    },
    Cut {
        handle: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MachineState {
    pub life: &'static str,
    pub incarnation: u32,
    /// Active clock skews and throttles.
    pub effects: Vec<String>,
    /// The engine's final state, once the cursor reaches the end.
    pub final_state: Option<String>,
}

impl Default for MachineState {
    fn default() -> Self {
        Self {
            life: "running",
            incarnation: 0,
            effects: Vec::new(),
            final_state: None,
        }
    }
}

/// Frames counted between two counter snapshots around the cursor.
#[derive(Clone, Debug)]
pub struct Interval {
    pub start_ns: u64,
    pub end_ns: u64,
    pub traffic: BTreeMap<Pair, Counts>,
}

impl Interval {
    /// Frames per simulated second.
    pub fn rate(&self, frames: u64) -> f64 {
        let secs = (self.end_ns - self.start_ns) as f64 / 1e9;
        if secs > 0.0 {
            frames as f64 / secs
        } else {
            0.0
        }
    }
}

#[derive(Clone, Debug)]
pub struct Snapshot {
    pub links: BTreeMap<Pair, LinkState>,
    pub machines: Vec<MachineState>,
    pub interval: Option<Interval>,
}

pub fn duration(ns: u64) -> String {
    let (v, unit) = match ns {
        0 => return "0".into(),
        n if n >= 1_000_000_000 => (n as f64 / 1e9, "s"),
        n if n >= 1_000_000 => (n as f64 / 1e6, "ms"),
        n if n >= 1_000 => (n as f64 / 1e3, "us"),
        n => (n as f64, "ns"),
    };
    let s = format!("{v:.2}");
    let s = s.trim_end_matches('0').trim_end_matches('.');
    format!("{s}{unit}")
}

fn signed_duration(ns: i64) -> String {
    let sign = if ns < 0 { "-" } else { "+" };
    format!("{sign}{}", duration(ns.unsigned_abs()))
}

pub fn policy_text(p: &Policy) -> String {
    let mut s = duration(p.latency_ns);
    if p.jitter_ns > 0 {
        s += &format!("±{}", duration(p.jitter_ns));
    }
    if p.loss_ppm > 0 {
        let pct = p.loss_ppm as f64 / 10_000.0;
        s += &format!(" {}%", format!("{pct:.1}").trim_end_matches(".0"));
    }
    if let Some(bps) = p.bandwidth_bps {
        s += &format!(" {}", rate_text(bps as f64, "b/s"));
    }
    s
}

pub fn rate_text(v: f64, unit: &str) -> String {
    match v {
        v if v >= 1e9 => format!("{:.1}G{unit}", v / 1e9),
        v if v >= 1e6 => format!("{:.1}M{unit}", v / 1e6),
        v if v >= 1e3 => format!("{:.1}k{unit}", v / 1e3),
        v => format!("{v:.0}{unit}"),
    }
}
