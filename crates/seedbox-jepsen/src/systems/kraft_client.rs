//! The KRaft workload client: `--workers` threads, each bound to one Kafka
//! node as Jepsen binds a client to a node, issuing admin requests through
//! the raw protocol in [`super::kraft_proto`] and streaming every invocation
//! and completion to the oracle as JSON lines (one connection, writes
//! serialized, so the oracle's receive order is a real-time order of the
//! history).
//!
//! A worker bound to a broker sends everything to that broker, which serves
//! reads from its metadata image and forwards writes to the active
//! controller. A worker bound to a controller reads from that controller's
//! own image and sends writes to the controller it believes is active (asked
//! through DescribeCluster, as Kafka's admin client does with
//! `bootstrap.controllers`), switching after an error.
//!
//! Workloads:
//! * `append`: Jepsen's list-append over topic configs. Key `k` is topic
//!   `a<k>` (the oracle creates them before the run), its list the topic's
//!   `leader.replication.throttled.replicas` config, a LIST that KRaft can
//!   APPEND to, with elements `<value>:0`. A write appends one unique element
//!   to 1-3 keys in one IncrementalAlterConfigs request (the controller
//!   applies it as one atomic batch); a read describes one key.
//! * `topics`: a linearizable existence register per topic name: create,
//!   delete, and a probe (CreateTopics with validateOnly), all decided by the
//!   active controller.
use super::kraft_proto::{self as proto, Conn, NoReply};
use crate::kit::{self, Rng};
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    io::{BufRead, BufReader, Write},
    net::TcpStream,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};

#[derive(clap::Args, Debug, Clone)]
pub struct Args {
    #[arg(long, default_value = "oracle:7100")]
    pub oracle: String,
    #[arg(long, value_enum, default_value = "append")]
    pub workload: Workload,
    /// Comma-separated controller machines.
    #[arg(long, default_value = "k1,k2,k3,k4,k5")]
    pub controllers: String,
    /// Comma-separated broker machines.
    #[arg(long, default_value = "b1,b2")]
    pub brokers: String,
    #[arg(long, default_value_t = 10)]
    pub workers: usize,
    /// Seconds before an unanswered request is indeterminate.
    #[arg(long, default_value_t = 5.0)]
    pub timeout: f64,
    /// Target operations per second over all workers.
    #[arg(long, default_value_t = 100.0)]
    pub rate: f64,
    /// Keys written at once (append) or topic names (topics).
    #[arg(long, default_value_t = 8)]
    pub keys: usize,
    /// Appends after which a key is retired for a fresh one.
    #[arg(long, default_value_t = 128)]
    pub max_appends: u64,
    /// Append keys the oracle created (topics a0..); once all are used,
    /// keys stop retiring.
    #[arg(long, default_value_t = 128)]
    pub key_topics: u64,
    /// Topic replication factor (topics).
    #[arg(long, default_value_t = 1)]
    pub replication: i16,
}

#[derive(clap::ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum Workload {
    /// List-append over topic list configs.
    Append,
    /// Create, delete and probe topics.
    Topics,
    /// Unique producer IDs from brokers (InitProducerId).
    Ids,
}

impl Workload {
    pub fn name(self) -> &'static str {
        match self {
            Self::Append => "append",
            Self::Topics => "topics",
            Self::Ids => "ids",
        }
    }
}

/// The client's single connection to the oracle.
struct Stream {
    out: Mutex<TcpStream>,
}

impl Stream {
    fn send(&self, event: &Value) {
        let mut line = event.to_string();
        line.push('\n');
        let mut out = self.out.lock().unwrap_or_else(|e| e.into_inner());
        if out.write_all(line.as_bytes()).is_err() {
            eprintln!("oracle stream closed");
        }
    }
}

/// Shared operation generator: ids, values and the key pool.
struct Gen {
    rng: Rng,
    next_op: u64,
    next_value: i64,
    next_key: u64,
    /// active key -> appends so far
    active: BTreeMap<u64, u64>,
}

impl Gen {
    fn key(&mut self, keys: usize, limit: u64) -> u64 {
        while self.active.len() < keys && self.next_key < limit {
            self.active.insert(self.next_key, 0);
            self.next_key += 1;
        }
        let active: Vec<u64> = self.active.keys().copied().collect();
        *self.rng.choice(&active)
    }
}

enum Op {
    /// Append (key, value) pairs.
    Append(Vec<(u64, i64)>),
    Read(u64),
    Create(u64),
    Delete(u64),
    Probe(u64),
    /// An idempotent producer ID.
    ProducerId,
}

/// The list config the append workload uses.
pub const LIST_CONFIG: &str = "leader.replication.throttled.replicas";

/// The topic that holds append key `key`.
pub fn key_topic(key: u64) -> String {
    format!("a{key}")
}

fn topic(key: u64) -> String {
    format!("t{key}")
}

struct Worker {
    id: usize,
    node: String,
    controllers: Vec<String>,
    is_controller: bool,
    timeout: Duration,
    /// Connections by node.
    conns: BTreeMap<String, Conn>,
    /// The controller this worker sends writes to.
    active: Option<String>,
    probe_turn: usize,
}

impl Worker {
    fn port(node: &str, controllers: &[String]) -> u16 {
        if controllers.iter().any(|c| c == node) {
            9093
        } else {
            9092
        }
    }

    fn conn(&mut self, node: &str) -> Result<&mut Conn, NoReply> {
        if !self.conns.contains_key(node) {
            let port = Self::port(node, &self.controllers);
            let conn = Conn::connect(node, port, self.timeout, &format!("w{}", self.id))?;
            self.conns.insert(node.into(), conn);
        }
        Ok(self.conns.get_mut(node).unwrap())
    }

    /// Run `f` against `node`, dropping the connection on a transport failure.
    fn with<T>(
        &mut self,
        node: &str,
        f: impl FnOnce(&mut Conn) -> Result<T, NoReply>,
    ) -> Result<T, NoReply> {
        let result = self.conn(node).and_then(f);
        if result.is_err() {
            self.conns.remove(node);
        }
        result
    }

    /// Where this worker's writes go.
    fn write_target(&mut self) -> String {
        if !self.is_controller {
            return self.node.clone();
        }
        if let Some(active) = &self.active {
            return active.clone();
        }
        // Ask the bound controller first, then the others in turn.
        let mut order = vec![self.node.clone()];
        let start = self.probe_turn;
        self.probe_turn += 1;
        for i in 0..self.controllers.len() {
            let c = self.controllers[(start + i) % self.controllers.len()].clone();
            if !order.contains(&c) {
                order.push(c);
            }
        }
        for node in order {
            if let Ok((0, leader, nodes)) = self.with(&node, proto::active_controller) {
                if let Some((_, host, _)) = nodes.iter().find(|(id, _, _)| *id == leader) {
                    self.active = Some(host.clone());
                    return host.clone();
                }
            }
        }
        // Nobody knows a leader: try the bound controller anyway.
        self.node.clone()
    }

    /// Perform `op`; returns (type, completed value, error, target node).
    fn perform(
        &mut self,
        op: &Op,
        replication: i16,
    ) -> (&'static str, Value, Option<String>, String) {
        let timeout_ms = self.timeout.as_millis() as i32;
        match op {
            Op::Read(key) => {
                let node = self.node.clone();
                let name = key_topic(*key);
                match self.with(&node, |c| {
                    proto::describe_configs(
                        c,
                        proto::TOPIC,
                        LIST_CONFIG,
                        std::slice::from_ref(&name),
                    )
                }) {
                    Ok(results) => match results.first() {
                        Some(r) if r.error == 0 => {
                            let list = parse_list(r.value.as_deref());
                            match list {
                                Ok(list) => ("ok", json!([["r", key, list]]), None, node),
                                Err(e) => ("fail", json!([["r", key, null]]), Some(e), node),
                            }
                        }
                        Some(r) => (
                            "fail",
                            json!([["r", key, null]]),
                            Some(proto::error_name(r.error)),
                            node,
                        ),
                        None => (
                            "fail",
                            json!([["r", key, null]]),
                            Some("no result".into()),
                            node,
                        ),
                    },
                    Err(e) => ("fail", json!([["r", key, null]]), Some(e.0), node),
                }
            }
            Op::Append(pairs) => {
                let node = self.write_target();
                let value = json!(pairs
                    .iter()
                    .map(|(k, v)| json!(["append", k, v]))
                    .collect::<Vec<_>>());
                let appends: Vec<(String, String)> = pairs
                    .iter()
                    .map(|(k, v)| (key_topic(*k), format!("{v}:0")))
                    .collect();
                match self.with(&node, |c| {
                    proto::append_configs(c, proto::TOPIC, LIST_CONFIG, &appends)
                }) {
                    Ok(results) => {
                        let codes: Vec<i16> = results.iter().map(|r| r.1).collect();
                        if results.len() == pairs.len() && codes.iter().all(|&c| c == 0) {
                            ("ok", value, None, node)
                        } else {
                            if codes.contains(&proto::NOT_CONTROLLER) {
                                self.active = None;
                            }
                            let error = results
                                .iter()
                                .map(|r| proto::error_name(r.1))
                                .collect::<Vec<_>>()
                                .join(",");
                            ("info", value, Some(error), node)
                        }
                    }
                    Err(e) => {
                        self.active = None;
                        (
                            if e.unsent() { "fail" } else { "info" },
                            value,
                            Some(e.0),
                            node,
                        )
                    }
                }
            }
            Op::Create(key) | Op::Probe(key) => {
                let probe = matches!(op, Op::Probe(_));
                let node = self.write_target();
                let name = topic(*key);
                let f = if probe { "probe" } else { "create" };
                match self.with(&node, |c| {
                    proto::create_topic(c, &name, replication, timeout_ms, probe)
                }) {
                    Ok((code, message, id)) => {
                        if code == proto::NOT_CONTROLLER {
                            self.active = None;
                        }
                        let value = json!({"f": f, "key": key, "code": proto::error_name(code),
                                           "id": (code == 0 && !probe).then(|| proto::uuid_text(id))});
                        let kind = match code {
                            0 | proto::TOPIC_ALREADY_EXISTS => "ok",
                            _ if probe => "fail",
                            proto::INVALID_REPLICATION_FACTOR => "fail",
                            _ => "info",
                        };
                        (
                            kind,
                            value,
                            message.or_else(|| (code != 0).then(|| proto::error_name(code))),
                            node,
                        )
                    }
                    Err(e) => {
                        self.active = None;
                        let kind = if probe || e.unsent() { "fail" } else { "info" };
                        (kind, json!({"f": f, "key": key}), Some(e.0), node)
                    }
                }
            }
            Op::ProducerId => {
                let node = self.node.clone();
                match self.with(&node, proto::init_producer_id) {
                    Ok((0, id, epoch)) => (
                        "ok",
                        json!({"f": "id", "id": id, "epoch": epoch}),
                        None,
                        node,
                    ),
                    Ok((code, _, _)) => (
                        "fail",
                        json!({"f": "id"}),
                        Some(proto::error_name(code)),
                        node,
                    ),
                    // An ID handed out but never received is never reused either.
                    Err(e) => (
                        if e.unsent() { "fail" } else { "info" },
                        json!({"f": "id"}),
                        Some(e.0),
                        node,
                    ),
                }
            }
            Op::Delete(key) => {
                let node = self.write_target();
                let name = topic(*key);
                match self.with(&node, |c| proto::delete_topic(c, &name, timeout_ms)) {
                    Ok((code, message)) => {
                        if code == proto::NOT_CONTROLLER {
                            self.active = None;
                        }
                        let value =
                            json!({"f": "delete", "key": key, "code": proto::error_name(code)});
                        let kind = match code {
                            0 | proto::UNKNOWN_TOPIC_OR_PARTITION => "ok",
                            // The name resolved to an ID that a concurrent delete
                            // removed: nothing was deleted.
                            proto::UNKNOWN_TOPIC_ID => "fail",
                            _ => "info",
                        };
                        (
                            kind,
                            value,
                            message.or_else(|| (code != 0).then(|| proto::error_name(code))),
                            node,
                        )
                    }
                    Err(e) => {
                        self.active = None;
                        (
                            if e.unsent() { "fail" } else { "info" },
                            json!({"f": "delete", "key": key}),
                            Some(e.0),
                            node,
                        )
                    }
                }
            }
        }
    }
}

/// A list config value as appended integers; elements are `<N>:0`.
pub fn parse_list(value: Option<&str>) -> Result<Vec<i64>, String> {
    let Some(value) = value else {
        return Ok(Vec::new());
    };
    value
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| {
            s.strip_suffix(":0")
                .and_then(|n| n.parse().ok())
                .ok_or_else(|| format!("unexpected element {s:?}"))
        })
        .collect()
}

fn next_op(gen: &Mutex<Gen>, args: &Args) -> (u64, Op) {
    let mut g = gen.lock().unwrap_or_else(|e| e.into_inner());
    let id = g.next_op;
    g.next_op += 1;
    let op = match args.workload {
        Workload::Append => {
            if g.rng.chance(0.5) {
                let key = g.key(args.keys, args.key_topics);
                Op::Read(key)
            } else {
                let count = g.rng.range(1, 3).min(args.keys as u64) as usize;
                let mut pairs = Vec::new();
                for _ in 0..count * 4 {
                    if pairs.len() == count {
                        break;
                    }
                    let key = g.key(args.keys, args.key_topics);
                    if pairs.iter().any(|(k, _)| *k == key) {
                        continue;
                    }
                    g.next_value += 1;
                    let value = g.next_value;
                    pairs.push((key, value));
                    let max = args.max_appends;
                    let used = g.active.get_mut(&key).unwrap();
                    *used += 1;
                    if *used >= max && g.next_key < args.key_topics {
                        g.active.remove(&key);
                    }
                }
                Op::Append(pairs)
            }
        }
        Workload::Ids => Op::ProducerId,
        Workload::Topics => {
            let key = g.rng.below(args.keys as u64);
            match g.rng.weighted(&[2.0, 2.0, 1.0]) {
                0 => Op::Create(key),
                1 => Op::Delete(key),
                _ => Op::Probe(key),
            }
        }
    };
    (id, op)
}

fn invocation(op: &Op) -> Value {
    match op {
        Op::Append(pairs) => json!(pairs
            .iter()
            .map(|(k, v)| json!(["append", k, v]))
            .collect::<Vec<_>>()),
        Op::Read(key) => json!([["r", key, null]]),
        Op::Create(key) => json!({"f": "create", "key": key}),
        Op::Delete(key) => json!({"f": "delete", "key": key}),
        Op::Probe(key) => json!({"f": "probe", "key": key}),
        Op::ProducerId => json!({"f": "id"}),
    }
}

pub fn run(args: Args) -> u8 {
    kit::start_clock();
    let controllers: Vec<String> = args
        .controllers
        .split(',')
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect();
    let brokers: Vec<String> = args
        .brokers
        .split(',')
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect();
    let nodes: Vec<String> = if args.workload == Workload::Ids {
        // Only brokers answer InitProducerId.
        brokers.clone()
    } else {
        controllers.iter().chain(brokers.iter()).cloned().collect()
    };
    let stream = loop {
        match TcpStream::connect(&args.oracle) {
            Ok(stream) => break stream,
            Err(error) => {
                eprintln!("oracle {}: {error}; retrying", args.oracle);
                kit::pause(1.0);
            }
        }
    };
    let reader = BufReader::new(stream.try_clone().expect("clone oracle stream"));
    let stream = Arc::new(Stream {
        out: Mutex::new(stream),
    });
    stream.send(
        &json!({"type": "hello", "workload": args.workload.name(), "workers": args.workers,
                        "nodes": nodes, "rate": args.rate}),
    );
    let started = Arc::new(AtomicBool::new(false));
    let stopped = Arc::new(AtomicBool::new(false));
    {
        let (started, stopped) = (started.clone(), stopped.clone());
        std::thread::spawn(move || {
            for line in reader.lines() {
                match line.as_deref().map(str::trim) {
                    Ok("start") => started.store(true, Ordering::SeqCst),
                    Ok("stop") => stopped.store(true, Ordering::SeqCst),
                    Ok(_) => {}
                    Err(_) => break,
                }
            }
            stopped.store(true, Ordering::SeqCst);
        });
    }
    while !started.load(Ordering::SeqCst) && !stopped.load(Ordering::SeqCst) {
        kit::pause(0.05);
    }
    let seed = kit::rng::entropy_seed();
    let gen = Arc::new(Mutex::new(Gen {
        rng: Rng::new(seed),
        next_op: 0,
        next_value: 0,
        next_key: 0,
        active: BTreeMap::new(),
    }));
    let completed = Arc::new(AtomicU64::new(0));
    let interval = Duration::from_secs_f64(args.workers as f64 / args.rate.max(0.01));
    let mut handles = Vec::new();
    for id in 0..args.workers {
        let node = nodes[id % nodes.len()].clone();
        let mut worker = Worker {
            id,
            is_controller: controllers.contains(&node),
            node,
            controllers: controllers.clone(),
            timeout: Duration::from_secs_f64(args.timeout),
            conns: BTreeMap::new(),
            active: None,
            probe_turn: id,
        };
        let (gen, stream, stopped, completed, args) = (
            gen.clone(),
            stream.clone(),
            stopped.clone(),
            completed.clone(),
            args.clone(),
        );
        let mut pacing = Rng::stream(seed, id as u64 + 1);
        handles.push(std::thread::spawn(move || {
            // Stagger the workers across one interval.
            kit::pause(pacing.uniform(0.0, interval.as_secs_f64()));
            while !stopped.load(Ordering::SeqCst) {
                let began = Instant::now();
                let (op_id, op) = next_op(&gen, &args);
                stream.send(&json!({"type": "invoke", "id": op_id, "p": worker.id,
                                    "value": invocation(&op), "bound": worker.node}));
                let (kind, value, error, target) = worker.perform(&op, args.replication);
                stream.send(
                    &json!({"type": kind, "id": op_id, "p": worker.id, "value": value,
                                    "error": error, "node": target}),
                );
                completed.fetch_add(1, Ordering::Relaxed);
                let jitter = pacing.uniform(0.5, 1.5);
                let wait = interval.mul_f64(jitter).saturating_sub(began.elapsed());
                if !wait.is_zero() {
                    std::thread::sleep(wait);
                }
            }
        }));
    }
    for handle in handles {
        let _ = handle.join();
    }
    stream.send(&json!({"type": "done", "ops": completed.load(Ordering::Relaxed)}));
    // Stay up: the oracle decides when the run ends.
    loop {
        kit::pause(3600.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metrics_lists_parse_to_appended_values() {
        assert_eq!(parse_list(None), Ok(vec![]));
        assert_eq!(parse_list(Some("")), Ok(vec![]));
        assert_eq!(parse_list(Some("1:0,12:0, 3:0")), Ok(vec![1, 12, 3]));
        assert!(parse_list(Some("1:0,x")).is_err());
    }
}
