//! Jepsen's NATS JetStream queue workload as a Seedbox oracle.
//!
//! Five servers n1..n5 hold one file-backed stream `jepsen-stream` (subjects
//! `jepsen.*`, R5 by default, discard new). Publisher threads, two per server
//! and each bound to its server, publish unique values `P-C` (publisher,
//! counter) to `jepsen.0` and wait for the JetStream publish ack, recording
//! its stream sequence. A timeout or error is indeterminate and never
//! retried; a late ack still counts. After the fault schedule every fault is
//! healed, any server whose process exited is started again (as Jepsen's
//! final generator does), the oracle waits for the replicas to agree, and
//! reads the stream from each server's own replica through an ephemeral R1
//! consumer led by that server. [`crate::checker::queue`] checks the result.
//!
//! Fault modes: `coordinated` (#7564: crash every machine together, reboot),
//! `pause-crash` and `part-crash` (#7567: Jepsen's pause-kill and part-kill),
//! `kill` (#6888: Jepsen's kill nemesis), `kill-all` (#6888 as nats-server#5946
//! reproduced it) and `none`.
use super::nats_proto::{random_id, Msg, Nats, NatsError, CLIENT_PORT, HTTP_PORT};
use crate::checker::queue::{self, Read};
use crate::finalread;
use crate::kit::{self, evidence, history_line, now, verdict, Ctl, Rng};
use crate::nemesis::{self, Pace, Sleep};
use serde_json::{json, Map, Value};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    thread,
    time::{Duration, Instant},
};

pub const NODES: [&str; 5] = ["n1", "n2", "n3", "n4", "n5"];
const STREAM: &str = "jepsen-stream";
const SUBJECT: &str = "jepsen.0";

#[derive(clap::Args, Debug, Clone)]
pub struct Args {
    /// Fault schedule.
    #[arg(long, value_enum)]
    pub mode: Mode,
    /// Label for the server configuration in use.
    #[arg(long, default_value = "")]
    pub config: String,
    #[arg(long, default_value_t = 5)]
    pub replicas: u64,
    #[arg(long, default_value_t = 10)]
    pub publishers: usize,
    /// Seconds between one publisher's publishes.
    #[arg(long, default_value_t = 0.05)]
    pub interval: f64,
    #[arg(long, default_value_t = 30.0)]
    pub warmup: f64,
    #[arg(long, default_value_t = 30.0)]
    pub hold: f64,
    #[arg(long, default_value_t = 10.0)]
    pub after: f64,
    /// Seconds to wait for replicas to agree before reading.
    #[arg(long, default_value_t = 30.0)]
    pub settle: f64,
    /// Docker-compatible engine socket: run against plain containers, no Seedbox.
    #[arg(long)]
    pub native: Option<std::path::PathBuf>,
}

#[derive(clap::ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// No faults.
    None,
    /// nats-server#7564: crash every machine together, then reboot them.
    Coordinated,
    /// nats-server#7567: Jepsen's pause-kill nemesis.
    PauseCrash,
    /// nats-server#7567: Jepsen's part-kill nemesis.
    PartCrash,
    /// nats-server#6888: Jepsen's kill nemesis.
    Kill,
    /// nats-server#6888: SIGKILL every server soon after the stream is created.
    KillAll,
}

impl Mode {
    fn name(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Coordinated => "coordinated",
            Self::PauseCrash => "pause-crash",
            Self::PartCrash => "part-crash",
            Self::Kill => "kill",
            Self::KillAll => "kill-all",
        }
    }
}

// ------------------------------------------------------------ history

#[derive(Default)]
struct HistoryState {
    next: u64,
    attempted: BTreeSet<String>,
    acked: BTreeMap<String, (u64, f64)>,
    failed: BTreeMap<String, String>,
    indeterminate: BTreeMap<String, String>,
    late_acks: Vec<String>,
    errors: BTreeMap<String, u64>,
}

/// Publishes, each printed as `H` lines: invoke, then ok (with the stream
/// sequence), fail or info, and possibly a later `late` acknowledgement.
#[derive(Default)]
pub struct History(Mutex<HistoryState>);

impl History {
    fn line(state: &mut HistoryState, kind: &str, process: usize, value: &str, extra: Value) {
        let mut entry = extra.as_object().cloned().unwrap_or_default();
        entry.insert("index".into(), state.next.into());
        entry.insert("type".into(), kind.into());
        entry.insert("process".into(), process.into());
        entry.insert("f".into(), "publish".into());
        entry.insert("value".into(), value.into());
        entry.insert("time".into(), kit::now_us().into());
        state.next += 1;
        history_line(&Value::Object(entry));
    }

    fn invoke(&self, value: &str, publisher: usize, node: &str) {
        let mut state = self.0.lock().unwrap();
        state.attempted.insert(value.into());
        Self::line(
            &mut state,
            "invoke",
            publisher,
            value,
            json!({"node": node}),
        );
    }

    fn complete(&self, value: &str, publisher: usize, code: Option<u16>, body: &[u8]) {
        let mut state = self.0.lock().unwrap();
        if code == Some(503) {
            state.failed.insert(value.into(), "no-responders".into());
            *state.errors.entry("no-responders".into()).or_default() += 1;
            Self::line(
                &mut state,
                "fail",
                publisher,
                value,
                json!({"error": "no-responders"}),
            );
            return;
        }
        let Ok(ack) = serde_json::from_slice::<Value>(body) else {
            state.indeterminate.insert(value.into(), "bad-ack".into());
            Self::line(
                &mut state,
                "info",
                publisher,
                value,
                json!({"error": "bad-ack"}),
            );
            return;
        };
        if let Some(error) = ack.get("error") {
            let description: String = error["description"]
                .as_str()
                .unwrap_or("")
                .chars()
                .take(60)
                .collect();
            state.failed.insert(value.into(), description.clone());
            *state.errors.entry(description.clone()).or_default() += 1;
            Self::line(
                &mut state,
                "fail",
                publisher,
                value,
                json!({"error": description}),
            );
        } else if let (Some(STREAM), Some(seq)) = (ack["stream"].as_str(), ack["seq"].as_u64()) {
            state.acked.insert(value.into(), (seq, now()));
            Self::line(&mut state, "ok", publisher, value, json!({"seq": seq}));
        } else {
            state.indeterminate.insert(value.into(), "odd-ack".into());
            Self::line(
                &mut state,
                "info",
                publisher,
                value,
                json!({"error": "odd-ack"}),
            );
        }
    }

    fn info(&self, value: &str, publisher: usize, reason: &str) {
        let mut state = self.0.lock().unwrap();
        state.indeterminate.insert(value.into(), reason.into());
        *state
            .errors
            .entry(reason.split(':').next().unwrap_or("").into())
            .or_default() += 1;
        Self::line(
            &mut state,
            "info",
            publisher,
            value,
            json!({"error": reason}),
        );
    }

    /// A reply that arrived after its request timed out is still an ack.
    fn late(&self, value: &str, publisher: usize, body: &[u8]) {
        let Ok(ack) = serde_json::from_slice::<Value>(body) else {
            return;
        };
        let mut state = self.0.lock().unwrap();
        if let (Some(STREAM), Some(seq)) = (ack["stream"].as_str(), ack["seq"].as_u64()) {
            state.acked.insert(value.into(), (seq, now()));
            state.indeterminate.remove(value);
            state.late_acks.push(value.into());
            Self::line(&mut state, "late", publisher, value, json!({"seq": seq}));
        }
    }

    fn summary(&self) -> Value {
        let state = self.0.lock().unwrap();
        let mut errors: Vec<(&String, &u64)> = state.errors.iter().collect();
        errors.sort_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));
        json!({
            "attempted": state.attempted.len(), "acked": state.acked.len(), "failed": state.failed.len(),
            "indeterminate": state.indeterminate.len(),
            "errors": errors.into_iter().take(6).map(|(k, v)| (k.clone(), json!(v))).collect::<Map<_, _>>(),
        })
    }
}

fn publisher(
    index: usize,
    node: &'static str,
    interval: f64,
    history: Arc<History>,
    stop: Arc<AtomicBool>,
) {
    let mut connection: Option<Nats> = None;
    let mut counter = 0u64;
    let mut timed_out: BTreeMap<String, String> = BTreeMap::new();
    let stale = |connection: &mut Nats, timed_out: &mut BTreeMap<String, String>| {
        while let Some(message) = connection.queue.pop_front() {
            if let Some(value) = timed_out.remove(&message.subject) {
                history.late(&value, index, &message.body);
            }
        }
    };
    while !stop.load(Ordering::SeqCst) {
        let Some(nats) = connection.as_mut() else {
            match Nats::connect(node, 2.0) {
                Ok(nats) => connection = Some(nats),
                Err(_) => kit::pause(1.0),
            }
            continue;
        };
        let value = format!("{index}-{counter}");
        counter += 1;
        history.invoke(&value, index, node);
        match nats.request(SUBJECT, value.as_bytes(), 2.0) {
            Ok((code, body)) => {
                stale(nats, &mut timed_out);
                history.complete(&value, index, code, &body);
            }
            Err(NatsError::Timeout(_)) => {
                history.info(&value, index, "timeout");
                timed_out.insert(format!("{}.{}", nats.inbox, nats.token), value);
                stale(nats, &mut timed_out);
            }
            Err(NatsError::Other(error)) => {
                let reason: String = format!("conn: {error}").chars().take(86).collect();
                history.info(&value, index, &reason);
                connection = None;
                kit::pause(1.0);
                continue;
            }
        }
        kit::pause(interval);
    }
}

// ------------------------------------------------------------ cluster operations

fn healthz(host: &str) -> (bool, String) {
    match kit::http::get(host, HTTP_PORT, "/healthz", Duration::from_secs(3)) {
        Ok((code, body)) => (
            code == 200,
            String::from_utf8_lossy(&body).chars().take(200).collect(),
        ),
        Err(error) => (false, error.0.chars().take(200).collect()),
    }
}

/// True when nothing listens on the client port: the server process exited.
fn unreachable(host: &str) -> bool {
    match crate::kit::http::connect(host, CLIENT_PORT, Duration::from_secs(3)) {
        Ok(_) => false,
        Err(error) => {
            error.0.contains("refused")
                || error.0.contains("unreachable")
                || error.0.contains("No route")
        }
    }
}

/// This server's own view of the meta layer and its stream replica.
fn jsz(host: &str) -> Value {
    let document = match kit::http::get(
        host,
        HTTP_PORT,
        "/jsz?accounts=true&streams=true",
        Duration::from_secs(3),
    )
    .and_then(|(_, body)| Ok(serde_json::from_slice::<Value>(&body)?))
    {
        Ok(document) => document,
        Err(error) => return json!({"error": error.0.chars().take(200).collect::<String>()}),
    };
    let mut view = json!({
        "messages": document["messages"],
        "meta_leader": document["meta_cluster"]["leader"],
        "streams": document["streams"],
    });
    for account in document["account_details"].as_array().into_iter().flatten() {
        for stream in account["stream_detail"].as_array().into_iter().flatten() {
            if stream["name"] == STREAM {
                let state = &stream["state"];
                view["stream"] = json!({"messages": state["messages"], "first_seq": state["first_seq"],
                                        "last_seq": state["last_seq"], "num_deleted": state["num_deleted"]});
                view["stream_leader"] = stream["cluster"]["leader"].clone();
            }
        }
    }
    view
}

fn any_node<T>(
    attempts: usize,
    pause: f64,
    mut call: impl FnMut(&mut Nats) -> super::nats_proto::Result<T>,
) -> Result<(String, T), String> {
    let mut last = String::new();
    for attempt in 0..attempts {
        let node = NODES[attempt % NODES.len()];
        match Nats::connect(node, 3.0) {
            Ok(mut nats) => match call(&mut nats) {
                Ok(value) => return Ok((node.into(), value)),
                Err(error) => last = error.to_string(),
            },
            Err(error) => last = error.to_string(),
        }
        kit::pause(pause);
    }
    Err(format!("no server answered: {last}"))
}

fn wait_healthy(nodes: &[&str], seconds: f64, label: &str) -> bool {
    let deadline = Instant::now() + Duration::from_secs_f64(seconds);
    let mut status: Vec<(&str, (bool, String))> = Vec::new();
    while Instant::now() < deadline {
        status = nodes.iter().map(|n| (*n, healthz(n))).collect();
        if status.iter().all(|(_, (ok, _))| *ok) {
            evidence("healthy", json!({"label": label, "nodes": nodes}));
            return true;
        }
        kit::pause(1.0);
    }
    let failing: Map<String, Value> = status
        .into_iter()
        .filter(|(_, (ok, _))| !ok)
        .map(|(n, (_, text))| (n.to_string(), Value::from(text)))
        .collect();
    evidence("unhealthy", json!({"label": label, "status": failing}));
    false
}

fn create_stream(replicas: u64) -> Result<(), String> {
    let config = json!({"name": STREAM, "subjects": ["jepsen.*"], "storage": "file", "retention": "limits",
                        "discard": "new", "num_replicas": replicas});
    let (node, reply) = any_node(60, 1.0, |nats| {
        let reply = nats.js(&format!("STREAM.CREATE.{STREAM}"), Some(&config), 10.0)?;
        // 10058: stream name already in use.
        if reply.get("error").is_some() && reply["error"]["err_code"] != 10058 {
            return Err(NatsError::Other(reply["error"].to_string()));
        }
        Ok(reply)
    })?;
    evidence(
        "stream-created",
        json!({"via": node, "cluster": reply["cluster"]}),
    );
    Ok(())
}

fn wait_stream_current(replicas: u64, seconds: f64) -> bool {
    let deadline = Instant::now() + Duration::from_secs_f64(seconds);
    while Instant::now() < deadline {
        if let Ok((_, info)) = any_node(1, 1.0, |nats| {
            nats.js(&format!("STREAM.INFO.{STREAM}"), None, 3.0)
        }) {
            let cluster = &info["cluster"];
            let replicas_info = cluster["replicas"].as_array().cloned().unwrap_or_default();
            if !cluster["leader"].is_null()
                && replicas_info.len() as u64 == replicas.saturating_sub(1)
                && replicas_info
                    .iter()
                    .all(|r| r["current"].as_bool().unwrap_or(false))
            {
                evidence("stream-current", json!({"leader": cluster["leader"]}));
                return true;
            }
        }
        kit::pause(1.0);
    }
    false
}

fn meta_leaders() -> Value {
    Value::Object(
        NODES
            .iter()
            .map(|n| (n.to_string(), jsz(n)["meta_leader"].clone()))
            .collect(),
    )
}

// ------------------------------------------------------------ fault schedules

fn schedule(ctl: &mut Ctl, args: &Args, rng: &mut Rng) -> kit::Result<()> {
    let pace = &mut Sleep;
    match args.mode {
        Mode::None => pace.wait(ctl, args.warmup + args.hold + args.after),
        Mode::Coordinated => {
            pace.wait(ctl, args.warmup);
            nemesis::crash_all(ctl, pace, &NODES, 1.0, json!({}))?;
            wait_healthy(&NODES, 120.0, "after-reboot");
            pace.wait(ctl, args.after);
        }
        Mode::PauseCrash => {
            pace.wait(ctl, args.warmup);
            nemesis::step(ctl, "pause_process", &["n1", "n2"])?;
            pace.wait(ctl, args.hold);
            nemesis::step(ctl, "crash", &["n3"])?;
            nemesis::step(ctl, "pause_process", &["n4", "n5"])?;
            nemesis::step(ctl, "resume_process", &["n1", "n2"])?;
            nemesis::step(ctl, "reboot", &["n3"])?;
            pace.wait(ctl, args.hold);
            nemesis::step(ctl, "resume_process", &["n4", "n5"])?;
            pace.wait(ctl, args.after);
        }
        Mode::PartCrash => {
            pace.wait(ctl, args.warmup);
            let first = nemesis::cut(ctl, &["n1", "n2"], &["n3", "n4", "n5"], json!({}))?;
            pace.wait(ctl, args.hold);
            nemesis::step(ctl, "crash", &["n3"])?;
            ctl.heal(&first)?;
            let second = ctl.partition(&["n4", "n5"], &["n1", "n2", "n3"])?;
            evidence(
                "fault",
                json!({"action": "heal+partition", "targets": [["n4", "n5"], ["n1", "n2", "n3"]]}),
            );
            nemesis::step(ctl, "reboot", &["n3"])?;
            pace.wait(ctl, args.hold);
            nemesis::heal(ctl, &second)?;
            pace.wait(ctl, args.after);
        }
        Mode::Kill => {
            pace.wait(ctl, args.warmup);
            nemesis::kill_restart(
                ctl,
                pace,
                rng,
                &NODES,
                args.hold,
                (1.0, 5.0),
                (5.0, 10.0),
                || evidence("meta-leader", json!({"views": meta_leaders()})),
            )?;
            pace.wait(ctl, args.after);
        }
        Mode::KillAll => {
            pace.wait(ctl, args.warmup);
            evidence("meta-leader", json!({"views": meta_leaders()}));
            nemesis::step(ctl, "stop", &NODES)?;
            pace.wait(ctl, 2.0);
            nemesis::step(ctl, "restart", &NODES)?;
            pace.wait(ctl, args.hold + args.after);
        }
    }
    Ok(())
}

// ------------------------------------------------------------ final reads

fn read_node(node: &str, seconds: f64) -> (Read, Value) {
    let deadline = Instant::now() + Duration::from_secs_f64(seconds);
    let mut attempts: Vec<String> = Vec::new();
    let short = |s: String| -> String { s.chars().take(100).collect() };
    while Instant::now() < deadline {
        let mut nats = match Nats::connect(node, 5.0) {
            Ok(nats) => nats,
            Err(error) => {
                attempts.push(short(format!("connect: {error}")));
                kit::pause(1.0);
                continue;
            }
        };
        let attempt = (|| -> super::nats_proto::Result<Option<(Read, Value)>> {
            let names = nats.js("STREAM.NAMES", Some(&json!({})), 5.0)?;
            if names.get("error").is_some() {
                attempts.push(short(names["error"].to_string()));
                kit::pause(1.0);
                return Ok(None);
            }
            let known = names["streams"]
                .as_array()
                .is_some_and(|s| s.iter().any(|n| n == STREAM));
            if !known {
                return Ok(Some((
                    Read::StreamMissing,
                    json!({"stream_missing": true, "names": names["streams"]}),
                )));
            }
            let deliver = format!("_DELIVER.{:016x}", random_id());
            nats.send(format!("SUB {deliver} 2\r\n").as_bytes())?;
            let config = json!({"deliver_policy": "all", "ack_policy": "none", "replay_policy": "instant",
                                "num_replicas": 1, "mem_storage": true, "deliver_subject": deliver,
                                "inactive_threshold": 120_000_000_000u64});
            let reply = nats.js(
                &format!("CONSUMER.CREATE.{STREAM}"),
                Some(&json!({"stream_name": STREAM, "config": config})),
                10.0,
            )?;
            if reply.get("error").is_some() {
                // 10059: stream not found.
                if reply["error"]["err_code"] == 10059 {
                    return Ok(Some((
                        Read::StreamMissing,
                        json!({"stream_missing": true, "error": reply["error"]}),
                    )));
                }
                attempts.push(short(reply["error"].to_string()));
                kit::pause(1.0);
                return Ok(None);
            }
            let name = reply["name"].as_str().unwrap_or("").to_string();
            let leader = reply["cluster"]["leader"]
                .as_str()
                .unwrap_or("")
                .to_string();
            if leader != node {
                // Placement is random among the stream's peers; try again.
                let _ = nats.js(&format!("CONSUMER.DELETE.{STREAM}.{name}"), None, 5.0);
                attempts.push(format!("placed on {leader}"));
                return Ok(None);
            }
            let messages = collect(&mut nats, &name, deadline)?;
            Ok(Some((
                Read::Messages(messages),
                json!({"placement_attempts": attempts.len()}),
            )))
        })();
        match attempt {
            Ok(Some(found)) => return found,
            Ok(None) => {}
            Err(error) => {
                attempts.push(short(error.to_string()));
                kit::pause(1.0);
            }
        }
    }
    let tail: Vec<String> = attempts.iter().rev().take(5).rev().cloned().collect();
    (Read::Unread, json!({"error": "no read", "attempts": tail}))
}

fn collect(
    nats: &mut Nats,
    name: &str,
    deadline: Instant,
) -> super::nats_proto::Result<BTreeMap<u64, String>> {
    let mut messages = BTreeMap::new();
    let mut quiet = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        let message: Msg = match nats.next_message(quiet.min(deadline)) {
            Ok(message) => message,
            Err(NatsError::Timeout(_)) => {
                let info = nats.js(&format!("CONSUMER.INFO.{STREAM}.{name}"), None, 5.0)?;
                if info["num_pending"] == 0 {
                    break;
                }
                quiet = Instant::now() + Duration::from_secs(3);
                continue;
            }
            Err(error) => return Err(error),
        };
        let Some(reply) = message
            .reply
            .as_deref()
            .filter(|r| message.sid == "2" && r.starts_with("$JS.ACK."))
        else {
            continue;
        };
        // v1: $JS.ACK.<stream>.<consumer>.<delivered>.<sseq>.<cseq>.<ts>.<pending>
        // v2 adds domain and account hash after ACK and a trailing token.
        let tokens: Vec<&str> = reply.split('.').collect();
        let index = if tokens.len() == 9 { 5 } else { 7 };
        if let Some(seq) = tokens.get(index).and_then(|t| t.parse().ok()) {
            messages.insert(seq, String::from_utf8_lossy(&message.body).to_string());
        }
        quiet = Instant::now() + Duration::from_secs(3);
    }
    let _ = nats.js(&format!("CONSUMER.DELETE.{STREAM}.{name}"), None, 5.0);
    Ok(messages)
}

// ------------------------------------------------------------ main

pub fn run(args: Args) -> u8 {
    kit::start_clock();
    let rng_seed = kit::rng::entropy_seed();
    let mut rng = Rng::new(rng_seed);
    evidence(
        "start",
        json!({"mode": args.mode.name(), "rng_seed": rng_seed, "config": args.config, "replicas": args.replicas,
               "publishers": args.publishers, "interval": args.interval, "warmup": args.warmup,
               "hold": args.hold, "after": args.after}),
    );
    let mut ctl = match kit::connect_world(args.native.clone()) {
        Ok(ctl) => ctl,
        Err(error) => return verdict(kit::INCONCLUSIVE, &format!("no controller: {error}")),
    };
    if !wait_healthy(&NODES, 180.0, "boot") {
        return verdict(kit::INCONCLUSIVE, "cluster never became healthy");
    }
    match any_node(60, 1.0, |nats| nats.js("INFO", None, 5.0)) {
        Ok((_, info)) => evidence(
            "jetstream",
            json!({"meta": info.get("meta_cluster").or(info.get("cluster"))}),
        ),
        Err(error) => {
            return verdict(
                kit::INCONCLUSIVE,
                &format!("JetStream never answered: {error}"),
            )
        }
    }
    if let Err(error) = create_stream(args.replicas) {
        return verdict(kit::INCONCLUSIVE, &format!("stream not created: {error}"));
    }
    wait_stream_current(args.replicas, 60.0);
    let history = Arc::new(History::default());
    let stop = Arc::new(AtomicBool::new(false));
    let publishers: Vec<_> = (0..args.publishers)
        .map(|index| {
            let (history, stop) = (history.clone(), stop.clone());
            let interval = args.interval;
            thread::spawn(move || {
                publisher(index, NODES[index % NODES.len()], interval, history, stop)
            })
        })
        .collect();
    let progress_stop = Arc::new(AtomicBool::new(false));
    let progress = {
        let (history, progress_stop) = (history.clone(), progress_stop.clone());
        thread::spawn(move || {
            let mut next = Instant::now() + Duration::from_secs(5);
            while !progress_stop.load(Ordering::SeqCst) {
                kit::pause(0.1);
                if Instant::now() >= next {
                    evidence("progress", history.summary());
                    next += Duration::from_secs(5);
                }
            }
        })
    };
    if let Err(error) = schedule(&mut ctl, &args, &mut rng) {
        evidence("schedule-failed", json!({"error": error.0}));
    }
    stop.store(true, Ordering::SeqCst);
    for publisher in publishers {
        let _ = publisher.join();
    }
    progress_stop.store(true, Ordering::SeqCst);
    let _ = progress.join();
    let mut summary = history.summary();
    summary["late_acks"] = json!(history.0.lock().unwrap().late_acks.len());
    evidence("history", summary);
    if let Ok(Some(world)) = ctl.inspect() {
        evidence("inspect", json!({"world": world}));
    }
    final_reads(&mut ctl, &args, &history)
}

fn final_reads(ctl: &mut Ctl, args: &Args, history: &History) -> u8 {
    let healthy = finalread::recover(
        ctl,
        &NODES,
        |label| wait_healthy(&NODES, 120.0, label),
        unreachable,
    );
    // Let replicas converge: read once every server reports the same stream
    // state for five seconds, or after --settle seconds regardless.
    let (states, _) = finalread::settle(&NODES, args.settle, 5.0, jsz, |v| v["stream"].clone());
    let views: Map<String, Value> = NODES.iter().map(|n| n.to_string()).zip(states).collect();
    evidence("jsz", json!({"healthy": healthy, "views": views}));
    let mut reads = Vec::new();
    for node in NODES {
        let (read, mut fields) = read_node(node, 60.0);
        let count = match &read {
            Read::Messages(messages) => messages.len(),
            _ => 0,
        };
        if let Read::Messages(messages) = &read {
            history_line(&json!({"type": "read", "node": node,
                                 "messages": messages.iter().map(|(s, v)| json!([s, v])).collect::<Vec<_>>()}));
        } else {
            history_line(
                &json!({"type": "read", "node": node, "stream_missing": read == Read::StreamMissing}),
            );
        }
        fields["node"] = node.into();
        fields["messages"] = count.into();
        evidence("read", fields);
        reads.push((node.to_string(), read));
    }
    let (attempted, acked) = {
        let state = history.0.lock().unwrap();
        (state.attempted.clone(), state.acked.clone())
    };
    conclude(&attempted, &acked, &reads)
}

fn conclude(
    attempted: &BTreeSet<String>,
    acked: &BTreeMap<String, (u64, f64)>,
    reads: &[(String, Read)],
) -> u8 {
    if acked.is_empty() {
        return verdict(kit::INCONCLUSIVE, "no publish was acknowledged");
    }
    let checked = queue::check(attempted, acked, reads);
    evidence("check", checked.result.clone());
    evidence("details", checked.details.clone());
    let result = &checked.result;
    if checked.violation {
        let mut problems: Vec<String> = [
            "lost",
            "divergent",
            "unexpected",
            "seq_conflicts",
            "acked_seq_moved",
            "acked_seq_reused",
        ]
        .iter()
        .filter(|p| result[**p].as_u64().unwrap_or(0) > 0)
        .map(|p| format!("{p}={}", result[*p]))
        .collect();
        if result["stream_missing"]
            .as_array()
            .is_some_and(|a| !a.is_empty())
        {
            problems.push(format!("stream_missing={}", result["stream_missing"]));
        }
        return verdict(kit::VIOLATION, &problems.join(", "));
    }
    if result["unread"].as_array().is_some_and(|a| !a.is_empty()) {
        return verdict(
            kit::INCONCLUSIVE,
            &format!("unread nodes {}", result["unread"]),
        );
    }
    verdict(
        kit::OK,
        &format!("{} acked values present on every node", result["acked"]),
    )
}

/// Re-check a saved oracle log from its `H` lines.
pub fn recheck(text: &str) -> (u8, Value) {
    let mut attempted = BTreeSet::new();
    let mut acked = BTreeMap::new();
    let mut reads = Vec::new();
    for line in kit::tagged_lines(text, "H ") {
        let value = line["value"].as_str().unwrap_or("").to_string();
        match line["type"].as_str() {
            Some("invoke") => {
                attempted.insert(value);
            }
            Some("ok" | "late") => {
                acked.insert(
                    value,
                    (
                        line["seq"].as_u64().unwrap_or(0),
                        line["time"].as_f64().unwrap_or(0.0),
                    ),
                );
            }
            Some("read") => {
                let node = line["node"].as_str().unwrap_or("").to_string();
                let read = match line["messages"].as_array() {
                    Some(messages) => Read::Messages(
                        messages
                            .iter()
                            .filter_map(|m| Some((m[0].as_u64()?, m[1].as_str()?.to_string())))
                            .collect(),
                    ),
                    None if line["stream_missing"] == true => Read::StreamMissing,
                    None => Read::Unread,
                };
                reads.push((node, read));
            }
            _ => {}
        }
    }
    let checked = queue::check(&attempted, &acked, &reads);
    let code = if checked.violation {
        kit::VIOLATION
    } else {
        kit::OK
    };
    (
        code,
        json!({"result": checked.result, "details": checked.details, "violation": checked.violation}),
    )
}
