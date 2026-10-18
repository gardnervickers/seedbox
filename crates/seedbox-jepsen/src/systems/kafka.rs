//! Jepsen's Kafka queue workload under Seedbox: KAFKA-17754 and acks=all
//! durability.
//!
//! Brokers n1..nN run Apache Kafka in KRaft mode; client machine c1 runs
//! `JepsenQueue.java` (Kafka's own Java client, as Jepsen used), which streams
//! every invocation and completion to this oracle as JSON lines on port 7100.
//! The oracle owns the history, the fault schedule and the checker, and runs
//! on one thread, serving the client connection whenever it waits.
//!
//! Workloads: `txn` (Jepsen's transactional queue), `produce` (an idempotent
//! acks=all producer) and `queue` (`produce` plus a consumer group that
//! commits after every poll). Nemeses: `none` (or a Seedbox fault profile),
//! `end-partition` (KAFKA-17754: a console trigger on the client's `@@END`
//! marker, printed just before each commit/abort, cuts c1 off from every
//! broker, so the EndTxn is lost in flight and retransmitted later),
//! `jepsen` (Jepsen's untargeted mix of broker pauses and client-broker
//! cuts), `crash-all` (power-fail every Kafka node at once, after
//! `--crash-after` acknowledgements or about every `--crash-all-every`
//! seconds) and `controller` (crash, kill, pause or isolate the active KRaft
//! controller, now and then another controller). With `--recover`, every
//! broker must serve the topic again (all brokers live, every partition led
//! with a full ISR) within that many seconds of the faults ending.
//! [`crate::checker::kafka`] checks the history.
use super::kraft_proto as proto;
use crate::checker::kafka;
use crate::kit::{
    self, evidence, history_line, now,
    serve::{take_lines, Loop, Outcome, Service},
    verdict, Ctl, Rng,
};
use crate::nemesis::{self, NotFired, Pace};
use serde_json::{json, Value};
use std::time::{Duration, Instant};

#[derive(clap::Args, Debug, Clone)]
pub struct Args {
    #[arg(long, value_enum, default_value = "txn")]
    pub workload: Workload,
    #[arg(long, value_enum, default_value = "none")]
    pub nemesis: Nemesis,
    /// Comma-separated broker machines.
    #[arg(long, default_value = "n1")]
    pub brokers: String,
    #[arg(long, default_value = "c1")]
    pub client: String,
    /// Seconds of workload.
    #[arg(long, default_value_t = 60.0)]
    pub duration: f64,
    #[arg(long, default_value_t = 5.0)]
    pub warmup: f64,
    /// Partition length range in seconds, `LOW,HIGH`.
    #[arg(long, default_value = "3.5,5.5", value_parser = parse_range)]
    pub cut: (f64, f64),
    /// Quiet time range after a heal in seconds, `LOW,HIGH`.
    #[arg(long, default_value = "2.0,4.0", value_parser = parse_range)]
    pub gap: (f64, f64),
    /// Jepsen nemesis interval in seconds.
    #[arg(long, default_value_t = 10.0)]
    pub interval: f64,
    /// Acknowledgements before crash-all.
    #[arg(long, default_value_t = 1000)]
    pub crash_after: u64,
    /// Power-fail every Kafka node together about this often (0.5-1.5 times
    /// it) during the workload instead; 0 crashes once after --crash-after.
    #[arg(long, default_value_t = 0.0)]
    pub crash_all_every: f64,
    /// Seconds every node stays down after a coordinated power failure.
    #[arg(long, default_value_t = 2.0)]
    pub crash_all_down: f64,
    /// Comma-separated KRaft controllers, when they are separate machines
    /// [default: the brokers].
    #[arg(long, default_value = "")]
    pub controllers: String,
    /// Seconds after the faults end that every broker has to serve the topic
    /// again; one that never does fails the run. 0 skips the check.
    #[arg(long, default_value_t = 0)]
    pub recover: u64,
    /// The client's topic.
    #[arg(long, default_value = "jq")]
    pub topic: String,
    #[arg(long, default_value_t = 7100)]
    pub port: u16,
    /// No Seedbox: no faults.
    #[arg(long)]
    pub native: bool,
}

fn parse_range(text: &str) -> Result<(f64, f64), String> {
    let (low, high) = text.split_once(',').unwrap_or((text, text));
    let low: f64 = low
        .trim()
        .parse()
        .map_err(|_| format!("{text:?}: expected LOW[,HIGH] seconds"))?;
    let high: f64 = high
        .trim()
        .parse()
        .map_err(|_| format!("{text:?}: expected LOW[,HIGH] seconds"))?;
    if low > high || low < 0.0 {
        return Err(format!("{text:?}: expected 0 <= LOW <= HIGH"));
    }
    Ok((low, high))
}

#[derive(clap::ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum Workload {
    /// Jepsen's transactional queue workload.
    Txn,
    /// An idempotent acks=all producer.
    Produce,
    /// The producer plus a consumer group committing after every poll.
    Queue,
}

#[derive(clap::ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum Nemesis {
    /// No faults.
    None,
    /// KAFKA-17754: cut c1 off from the brokers on its EndTxn console marker.
    EndPartition,
    /// Jepsen's untargeted nemesis: broker pauses and client-broker cuts.
    Jepsen,
    /// Power-fail every Kafka node at once, after --crash-after
    /// acknowledgements or about every --crash-all-every seconds.
    CrashAll,
    /// Crash, kill, pause or isolate the active controller (or another).
    Controller,
}

impl Workload {
    pub fn name(self) -> &'static str {
        match self {
            Self::Txn => "txn",
            Self::Produce => "produce",
            Self::Queue => "queue",
        }
    }
}

impl Nemesis {
    pub fn name(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::EndPartition => "end-partition",
            Self::Jepsen => "jepsen",
            Self::CrashAll => "crash-all",
            Self::Controller => "controller",
        }
    }
}

/// The client's event stream.
#[derive(Default)]
pub struct History {
    pub events: Vec<Value>,
    pub hello: Option<Value>,
    pub topic_ready: Option<Value>,
    pub final_read: Option<Value>,
    pub done: bool,
    pub acks: u64,
    pub ends: Vec<(f64, Value, Value)>,
}

impl History {
    pub fn handle(&mut self, mut event: Value) {
        event["t"] = now().into();
        history_line(&event);
        match event["type"].as_str() {
            Some("hello") => self.hello = Some(event.clone()),
            Some("topic-ready") => self.topic_ready = Some(event.clone()),
            Some("final") => self.final_read = Some(event.clone()),
            Some("done") => self.done = true,
            Some("ok") if event["f"] == "send" => self.acks += 1,
            Some("end") => self
                .ends
                .push((now(), event["op"].clone(), event["kind"].clone())),
            _ => {}
        }
        self.events.push(event);
    }
}

impl Service for History {
    fn receive(&mut self, buffer: &mut Vec<u8>) -> Outcome {
        for line in take_lines(buffer) {
            if line.iter().all(u8::is_ascii_whitespace) {
                continue;
            }
            match serde_json::from_slice::<Value>(&line) {
                Ok(event) => self.handle(event),
                Err(error) => evidence("bad-event", json!({"error": error.to_string()})),
            }
        }
        Outcome::Wait
    }
}

struct Serving<'a> {
    server: &'a mut Loop,
    history: &'a mut History,
}

impl Pace for Serving<'_> {
    fn wait(&mut self, _: &mut Ctl, seconds: f64) {
        self.server.serve(self.history, seconds);
    }
}

fn end_partition(
    ctl: &mut Ctl,
    pace: &mut Serving,
    args: &Args,
    rng: &mut Rng,
    end: Instant,
    brokers: &[&str],
) -> kit::Result<u64> {
    let mut faults = 0;
    while Instant::now() < end && faults < 60 {
        let fired = nemesis::console_partition(
            ctl,
            pace,
            &args.client,
            b"@@END ",
            &[args.client.as_str()],
            brokers,
            end,
            0.1,
        )?;
        let fired = match fired {
            Ok(fired) => fired,
            Err(NotFired::Cancelled) => {
                evidence("trigger", json!({"status": "cancelled"}));
                break;
            }
            Err(NotFired::Other(status)) => {
                evidence("trigger", json!({"status": status}));
                break;
            }
        };
        let left = vec![args.client.clone()];
        let right: Vec<String> = brokers.iter().map(|b| b.to_string()).collect();
        ctl.in_force.cuts.push(kit::world::Cut {
            partition: fired.partition.clone(),
            left: left.clone(),
            right: right.clone(),
        });
        rng.refresh();
        let cut = rng.uniform(args.cut.0, args.cut.1);
        let recent = pace
            .history
            .ends
            .last()
            .map(|(t, op, kind)| json!([t, op, kind]));
        evidence(
            "fault",
            json!({"action": "partition", "targets": [left, right], "cut": (cut * 1000.0).round() / 1000.0,
                   "matched_at_ns": fired.status["matched_at_ns"], "effect_at_ns": fired.status["effect_at_ns"],
                   "last_end": recent}),
        );
        pace.wait(ctl, cut);
        ctl.heal_partition(&fired.partition)?;
        evidence(
            "fault",
            json!({"action": "heal", "targets": [[args.client], brokers]}),
        );
        faults += 1;
        rng.refresh();
        pace.wait(ctl, rng.uniform(args.gap.0, args.gap.1));
    }
    Ok(faults)
}

fn jepsen_mix(
    ctl: &mut Ctl,
    pace: &mut Serving,
    args: &Args,
    rng: &mut Rng,
    end: Instant,
    brokers: &[&str],
) -> kit::Result<u64> {
    let mut faults = 0;
    while Instant::now() < end {
        pace.wait(ctl, args.interval);
        if Instant::now() >= end {
            break;
        }
        rng.refresh();
        let length = rng.uniform(2.0, 10.0);
        let target = *rng.choice(brokers);
        let rounded = (length * 1000.0).round() / 1000.0;
        if rng.chance(0.5) {
            nemesis::step_with(ctl, "pause_process", &[target], json!({"length": rounded}))?;
            pace.wait(ctl, length);
            nemesis::step(ctl, "resume_process", &[target])?;
        } else {
            let cut = nemesis::cut(
                ctl,
                &[args.client.as_str()],
                &[target],
                json!({"length": rounded}),
            )?;
            pace.wait(ctl, length);
            nemesis::heal(ctl, &cut)?;
        }
        faults += 1;
    }
    Ok(faults)
}

fn crash_all(ctl: &mut Ctl, pace: &mut Serving, args: &Args, brokers: &[&str]) -> kit::Result<u64> {
    let target = args.crash_after;
    if !pace
        .server
        .wait_until(pace.history, args.duration, |h| h.acks >= target)
    {
        evidence("crash", json!({"skipped": true, "acks": pace.history.acks}));
        return Ok(0);
    }
    let acked = pace.history.acks;
    nemesis::crash_all(ctl, pace, brokers, 2.0, json!({"acked_before": acked}))?;
    let before = pace.history.acks;
    pace.server
        .wait_until(pace.history, 300.0, |h| h.acks >= before + 100);
    evidence(
        "recovered",
        json!({"acks_after_reboot": pace.history.acks - before}),
    );
    Ok(1)
}

/// Power-fail every node together about every `--crash-all-every` seconds
/// until the workload ends, rebooting them after `--crash-all-down`.
fn crash_all_every(
    ctl: &mut Ctl,
    pace: &mut Serving,
    args: &Args,
    rng: &mut Rng,
    end: Instant,
    nodes: &[&str],
) -> kit::Result<u64> {
    let mut crashes = 0;
    loop {
        rng.refresh();
        let gap = rng.uniform(0.5, 1.5) * args.crash_all_every;
        let tail = Duration::from_secs_f64(gap + args.crash_all_down + 5.0);
        if Instant::now() + tail > end {
            return Ok(crashes);
        }
        pace.wait(ctl, gap);
        let acked = pace.history.acks;
        nemesis::crash_all(
            ctl,
            pace,
            nodes,
            args.crash_all_down,
            json!({"acked_before": acked}),
        )?;
        crashes += 1;
    }
}

/// The active controller's machine, as the first controller that answers
/// DescribeQuorum with a leader names it.
fn active_controller(controllers: &[&str]) -> Option<String> {
    for node in controllers {
        let Ok(mut conn) =
            proto::Conn::connect(node, 9093, Duration::from_secs(1), "jepsen-nemesis")
        else {
            continue;
        };
        let Ok(quorum) = proto::describe_quorum(&mut conn) else {
            continue;
        };
        if quorum.error != proto::NONE || quorum.leader < 0 {
            continue;
        }
        if let Some((_, host, _)) = quorum
            .endpoints
            .iter()
            .find(|(id, _, _)| *id == quorum.leader)
        {
            if controllers.contains(&host.as_str()) {
                return Some(host.clone());
            }
        }
    }
    None
}

/// Controller-targeted faults: every interval (0.5-1.5 times it), crash,
/// kill, pause or isolate the active controller for 2-8 s, or, a quarter of
/// the time (or with no leader), a random controller.
fn controller_faults(
    ctl: &mut Ctl,
    pace: &mut Serving,
    args: &Args,
    rng: &mut Rng,
    end: Instant,
    controllers: &[&str],
    everyone: &[&str],
) -> kit::Result<u64> {
    let mut faults = 0;
    while Instant::now() < end {
        rng.refresh();
        pace.wait(ctl, rng.uniform(0.5, 1.5) * args.interval);
        if Instant::now() >= end {
            break;
        }
        rng.refresh();
        let leader = active_controller(controllers);
        let (target, role) = match &leader {
            Some(leader) if rng.chance(0.75) => (leader.clone(), "leader"),
            _ => (rng.choice(controllers).to_string(), "random"),
        };
        let length = rng.uniform(2.0, 8.0);
        let rounded = (length * 1000.0).round() / 1000.0;
        let context = json!({"length": rounded, "role": role, "leader": leader});
        let target = target.as_str();
        match *rng.choice(&["crash", "stop", "pause_process", "isolate"]) {
            "crash" => {
                nemesis::step_with(ctl, "crash", &[target], context)?;
                pace.wait(ctl, length);
                nemesis::step(ctl, "reboot", &[target])?;
            }
            "stop" => {
                nemesis::step_with(ctl, "stop", &[target], context)?;
                pace.wait(ctl, length);
                nemesis::step(ctl, "restart", &[target])?;
            }
            "pause_process" => {
                nemesis::step_with(ctl, "pause_process", &[target], context)?;
                pace.wait(ctl, length);
                nemesis::step(ctl, "resume_process", &[target])?;
            }
            _ => {
                let (left, right) = nemesis::isolate(target, everyone);
                let cut = nemesis::cut(ctl, &left, &right, context)?;
                pace.wait(ctl, length);
                nemesis::heal(ctl, &cut)?;
            }
        }
        faults += 1;
    }
    Ok(faults)
}

/// Whether a broker serves the topic: it answers Metadata listing every
/// broker as live, and every partition has a leader and a full ISR.
fn serving(node: &str, topic: &str, brokers: usize) -> Result<(), String> {
    let mut conn = proto::Conn::connect(node, 9092, Duration::from_secs(2), "jepsen-recover")
        .map_err(|e| e.0)?;
    let meta = proto::metadata(&mut conn, topic).map_err(|e| e.0)?;
    if meta.brokers.len() < brokers {
        return Err(format!("{} of {brokers} brokers live", meta.brokers.len()));
    }
    if meta.topic_error != proto::NONE {
        return Err(format!("topic {}", proto::error_name(meta.topic_error)));
    }
    if meta.partitions.is_empty() {
        return Err("no partitions".into());
    }
    for p in &meta.partitions {
        if p.error != proto::NONE || p.leader < 0 {
            return Err(format!(
                "partition {} leader {} {}",
                p.index,
                p.leader,
                proto::error_name(p.error)
            ));
        }
        if p.isr.len() < p.replicas.len() {
            return Err(format!(
                "partition {} ISR {:?} of replicas {:?}",
                p.index, p.isr, p.replicas
            ));
        }
    }
    Ok(())
}

/// Wait up to `--recover` seconds, serving the client, for every broker to
/// serve the topic; returns the brokers that never did.
fn recover(server: &mut Loop, history: &mut History, args: &Args, brokers: &[&str]) -> Vec<String> {
    let start = Instant::now();
    let mut waiting: Vec<String> = brokers.iter().map(|b| b.to_string()).collect();
    let mut last = std::collections::BTreeMap::new();
    loop {
        waiting.retain(|node| match serving(node, &args.topic, brokers.len()) {
            Ok(()) => {
                evidence(
                    "recovered",
                    json!({"node": node, "after": start.elapsed().as_secs_f64()}),
                );
                false
            }
            Err(error) => {
                last.insert(node.clone(), error);
                true
            }
        });
        if waiting.is_empty() || start.elapsed() >= Duration::from_secs(args.recover) {
            break;
        }
        server.serve(history, 1.0);
    }
    for node in &waiting {
        evidence(
            "unrecovered",
            json!({"node": node, "waited": start.elapsed().as_secs_f64(), "last": last.get(node)}),
        );
    }
    waiting
}

pub fn run(args: Args) -> u8 {
    kit::start_clock();
    let brokers: Vec<&str> = args.brokers.split(',').collect();
    let controllers: Vec<&str> = if args.controllers.is_empty() {
        brokers.clone()
    } else {
        args.controllers.split(',').collect()
    };
    let mut nodes: Vec<&str> = brokers.clone();
    nodes.extend(controllers.iter().filter(|c| !brokers.contains(c)));
    let mut everyone = nodes.clone();
    everyone.push(args.client.as_str());
    let rng_seed = kit::rng::entropy_seed();
    let mut rng = Rng::new(rng_seed);
    evidence(
        "start",
        json!({"workload": args.workload.name(), "nemesis": args.nemesis.name(), "brokers": brokers,
               "rng_seed": rng_seed, "duration": args.duration, "cut": [args.cut.0, args.cut.1],
               "gap": [args.gap.0, args.gap.1]}),
    );
    let mut history = History::default();
    let mut server = match Loop::bind(args.port) {
        Ok(server) => server,
        Err(error) => return verdict(kit::INCONCLUSIVE, &error.0),
    };
    let mut ctl = if args.native {
        None
    } else {
        match kit::connect_world(None) {
            Ok(ctl) => Some(ctl),
            Err(error) => return verdict(kit::INCONCLUSIVE, &format!("no controller: {error}")),
        }
    };
    if !server.wait_until(&mut history, 600.0, |h| h.topic_ready.is_some()) {
        return verdict(kit::INCONCLUSIVE, "the client never created its topic");
    }
    evidence(
        "ready",
        json!({"hello": history.hello, "topic": history.topic_ready}),
    );
    if !args.native {
        seedbox_sdk::setup_complete(json!({"brokers": brokers, "workload": args.workload.name()}));
    }
    server.broadcast(b"start\n");
    server.serve(&mut history, args.warmup);
    let mut end = Instant::now() + Duration::from_secs_f64(args.duration);
    let mut faults = 0;
    if let Some(ctl) = ctl.as_mut() {
        let mut pace = Serving {
            server: &mut server,
            history: &mut history,
        };
        let result = match args.nemesis {
            Nemesis::None => Ok(0),
            Nemesis::EndPartition => end_partition(ctl, &mut pace, &args, &mut rng, end, &brokers),
            Nemesis::Jepsen => jepsen_mix(ctl, &mut pace, &args, &mut rng, end, &brokers),
            Nemesis::CrashAll if args.crash_all_every > 0.0 => {
                crash_all_every(ctl, &mut pace, &args, &mut rng, end, &nodes)
            }
            Nemesis::CrashAll => {
                let result = crash_all(ctl, &mut pace, &args, &nodes);
                // Stop once the cluster has recovered.
                end = Instant::now();
                result
            }
            Nemesis::Controller => controller_faults(
                ctl,
                &mut pace,
                &args,
                &mut rng,
                end,
                &controllers,
                &everyone,
            ),
        };
        faults = result.unwrap_or_else(|error| {
            evidence("nemesis-failed", json!({"error": error.0}));
            0
        });
        ctl.heal_everything();
    }
    server.serve(
        &mut history,
        end.saturating_duration_since(Instant::now()).as_secs_f64(),
    );
    evidence("nemesis", json!({"faults": faults}));
    let mut down = Vec::new();
    if args.recover > 0 {
        // A fault profile for these scenarios ends before the workload does.
        // (The SDK's `quiet` needs a second control connection, which waits
        // forever behind this oracle's open one.)
        down = recover(&mut server, &mut history, &args, &brokers);
    }
    server.serve(&mut history, 5.0);
    server.broadcast(b"stop\n");
    if !server.wait_until(&mut history, 600.0, |h| h.done) {
        evidence(
            "client",
            json!({"done": false, "final": history.final_read.is_some()}),
        );
    }
    if let Some(ctl) = ctl.as_mut() {
        if let Ok(Some(state)) = ctl.inspect() {
            evidence(
                "inspect",
                json!({"triggers": state.get("triggers"), "machines": state.get("machines")}),
            );
        }
    }
    conclude(
        &history.events,
        args.workload.name(),
        faults,
        args.nemesis != Nemesis::None,
        &down,
    )
}

fn conclude(events: &[Value], workload: &str, faults: u64, faulted: bool, down: &[String]) -> u8 {
    let checked = kafka::check(events, workload);
    let mut result = checked.result.clone();
    result["faults"] = faults.into();
    result["unrecovered"] = json!(down);
    evidence("check", result.clone());
    evidence("details", checked.details.clone());
    let mut summary = kafka::summarize(&result, faults);
    if !down.is_empty() {
        summary.push_str(&format!(
            "; {} never served the topic again after the faults ended",
            down.join(", ")
        ));
    }
    if checked.violation || !down.is_empty() {
        return verdict(kit::VIOLATION, &summary);
    }
    if result["final"] != "complete" {
        let status = match &result["final"] {
            Value::String(s) => s.clone(),
            Value::Null => "None".into(),
            other => other.to_string(),
        };
        return verdict(
            kit::INCONCLUSIVE,
            &format!("final read {status}; {summary}"),
        );
    }
    if workload == "queue" && result["queue"]["drained"] != "complete" {
        return verdict(
            kit::INCONCLUSIVE,
            &format!("the consumer group never drained; {summary}"),
        );
    }
    if result["unseen"].as_u64().unwrap_or(0) > 0 {
        return verdict(
            kit::INCONCLUSIVE,
            &format!("committed writes beyond an open transaction; {summary}"),
        );
    }
    if faulted && faults == 0 {
        return verdict(
            kit::INCONCLUSIVE,
            &format!("no fault was applied; {summary}"),
        );
    }
    verdict(kit::OK, &summary)
}

/// Re-check a saved oracle log from its `H` lines.
pub fn recheck(text: &str, workload: &str) -> (u8, Value) {
    let events = kit::tagged_lines(text, "H ");
    let checked = kafka::check(&events, workload);
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
