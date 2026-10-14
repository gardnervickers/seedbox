//! Jepsen's Kafka queue workload under Seedbox: KAFKA-17754 and acks=all
//! durability.
//!
//! Brokers n1..nN run Apache Kafka in KRaft mode; client machine c1 runs
//! `JepsenQueue.java` (Kafka's own Java client, as Jepsen used), which streams
//! every invocation and completion to this oracle as JSON lines on port 7100.
//! The oracle owns the history, the fault schedule and the checker, and runs
//! on one thread, serving the client connection whenever it waits.
//!
//! Workloads: `txn` (Jepsen's transactional queue) with nemesis `none`,
//! `end-partition` (KAFKA-17754: a console trigger on the client's `@@END`
//! marker, printed just before each commit/abort, cuts c1 off from every
//! broker, so the EndTxn is lost in flight and retransmitted later) or
//! `jepsen` (Jepsen's untargeted mix of broker pauses and client-broker cuts);
//! and `produce` with `crash-all` (power-fail every broker after
//! `--crash-after` acknowledgements). [`crate::checker::kafka`] checks it.
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
}

#[derive(clap::ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum Nemesis {
    /// No faults.
    None,
    /// KAFKA-17754: cut c1 off from the brokers on its EndTxn console marker.
    EndPartition,
    /// Jepsen's untargeted nemesis: broker pauses and client-broker cuts.
    Jepsen,
    /// Power-fail every broker after --crash-after acknowledgements.
    CrashAll,
}

impl Workload {
    pub fn name(self) -> &'static str {
        match self {
            Self::Txn => "txn",
            Self::Produce => "produce",
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

pub fn run(args: Args) -> u8 {
    kit::start_clock();
    let brokers: Vec<&str> = args.brokers.split(',').collect();
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
            Nemesis::CrashAll => {
                let result = crash_all(ctl, &mut pace, &args, &brokers);
                // Stop once the cluster has recovered.
                end = Instant::now();
                result
            }
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
    server.serve(&mut history, 5.0);
    server.broadcast(b"stop\n");
    if !server.wait_until(&mut history, 400.0, |h| h.done) {
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
    )
}

fn conclude(events: &[Value], workload: &str, faults: u64, faulted: bool) -> u8 {
    let checked = kafka::check(events, workload);
    let mut result = checked.result.clone();
    result["faults"] = faults.into();
    evidence("check", result.clone());
    evidence("details", checked.details.clone());
    let summary = kafka::summarize(&result, faults);
    if checked.violation {
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
