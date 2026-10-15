//! Jepsen's Raft testing approach applied to Kafka's KRaft metadata quorum.
//!
//! Controllers k1..kN (node ids 1..N) form the KRaft quorum; brokers
//! b1..bM (ids 101..) hold no workload data but serve metadata reads and
//! forward writes. Client machine `w1` runs `seedbox-jepsen kraft-client`
//! and streams every invocation and completion here (port 7100); this oracle
//! owns the history, the fault schedule, the final reads and the checker
//! ([`crate::checker::kraft`]), on one thread.
//!
//! Nemeses, combined as Jepsen combines them (`--nemesis partition,kill,...`):
//! every `--interval` seconds (jittered) one fault is chosen, held for an
//! interval, then undone.
//! * `partition`: random halves of all nodes, an isolated active controller,
//!   a majorities ring or a bridge over the controllers.
//! * `kill`: SIGKILL one, a minority, a majority or all controllers, or the
//!   leader (sometimes a broker), restarted when healed.
//! * `pause`: SIGSTOP one, a minority or the leader.
//! * `crash`: power loss (unflushed writes lost) on one, a minority, a
//!   majority or all controllers, or the leader; rebooted when healed.
//! * `clock`: step CLOCK_REALTIME of one or more nodes by up to ±`--skew`
//!   seconds (Kafka's timers read wall-clock milliseconds).
//! * `member` (dynamic quorum only): remove a voter (keeping at least
//!   three) or add an observer controller back, through the active
//!   controller (KIP-853).
use super::kraft_client::{key_topic, parse_list, Workload, LIST_CONFIG};
use super::kraft_proto::{self as proto, Conn, Quorum};
use crate::checker::kraft as checker;
use crate::kit::{
    self, evidence, history_line, now,
    serve::{take_lines, Loop, Outcome, Service},
    verdict, Ctl, Rng,
};
use crate::nemesis::{self, Pace, Subset};
use seedbox_control_client::{Amount, ClockSkew};
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    time::{Duration, Instant},
};

#[derive(clap::Args, Debug, Clone)]
pub struct Args {
    #[arg(long, value_enum, default_value = "append")]
    pub workload: Workload,
    /// Comma-separated faults: none, partition, kill, pause, crash, clock, member.
    #[arg(long, default_value = "none")]
    pub nemesis: String,
    #[arg(long, default_value = "k1,k2,k3,k4,k5")]
    pub controllers: String,
    #[arg(long, default_value = "b1,b2")]
    pub brokers: String,
    /// Seconds of workload.
    #[arg(long, default_value_t = 120.0)]
    pub duration: f64,
    /// Seconds of workload before the first fault.
    #[arg(long, default_value_t = 10.0)]
    pub warmup: f64,
    /// Mean seconds between faults and mean seconds each is held.
    #[arg(long, default_value_t = 5.0)]
    pub interval: f64,
    /// Largest clock step in seconds.
    #[arg(long, default_value_t = 30.0)]
    pub skew: f64,
    /// Seconds after the last heal before the final reads.
    #[arg(long, default_value_t = 20.0)]
    pub settle: f64,
    /// Seconds the final reads may take to converge.
    #[arg(long, default_value_t = 90.0)]
    pub converge: f64,
    /// Append workload: topics a0.. to create before the run.
    #[arg(long, default_value_t = 128)]
    pub key_topics: u64,
    /// Least number of voters the member nemesis keeps.
    #[arg(long, default_value_t = 3)]
    pub min_voters: usize,
    #[arg(long, default_value_t = 7100)]
    pub port: u16,
    /// Docker-compatible engine socket: run against plain containers, no
    /// Seedbox (kill, pause and member faults only).
    #[arg(long)]
    pub native: Option<std::path::PathBuf>,
}

// ---------------------------------------------------------------- history

#[derive(Default)]
pub struct History {
    pub hello: Option<Value>,
    pub done: bool,
    seq: u64,
    pending: BTreeMap<u64, (Value, f64, u64)>,
    pub entries: Vec<Value>,
    pub keys: std::collections::BTreeSet<i64>,
}

impl History {
    fn handle(&mut self, event: Value) {
        let position = self.seq;
        self.seq += 1;
        match event["type"].as_str() {
            Some("hello") => {
                evidence("hello", event.clone());
                self.hello = Some(event);
            }
            Some("done") => {
                evidence("client-done", event);
                self.done = true;
            }
            Some("invoke") => {
                let id = event["id"].as_u64().unwrap_or(u64::MAX);
                for mop in event["value"].as_array().into_iter().flatten() {
                    if let Some(k) = mop[1].as_i64() {
                        self.keys.insert(k);
                    }
                }
                self.pending.insert(id, (event, now(), position));
            }
            Some(kind @ ("ok" | "fail" | "info")) => {
                let id = event["id"].as_u64().unwrap_or(u64::MAX);
                let Some((invoke, t0, s0)) = self.pending.remove(&id) else {
                    evidence("orphan-completion", event);
                    return;
                };
                let entry = json!({"i": id, "p": invoke["p"], "type": kind, "t0": t0, "t1": now(),
                                   "s0": s0, "s1": position, "value": event["value"],
                                   "error": event["error"], "node": event["node"], "bound": invoke["bound"]});
                history_line(&entry);
                self.entries.push(entry);
            }
            _ => evidence("bad-event", event),
        }
    }

    /// Operations still open at the end are indeterminate.
    fn close(&mut self) {
        for (id, (invoke, t0, s0)) in std::mem::take(&mut self.pending) {
            let entry = json!({"i": id, "p": invoke["p"], "type": "info", "t0": t0, "t1": now(),
                               "s0": s0, "s1": u64::MAX, "value": invoke["value"],
                               "error": "never completed", "node": null, "bound": invoke["bound"]});
            history_line(&entry);
            self.entries.push(entry);
        }
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

// ---------------------------------------------------------------- cluster

pub struct Cluster {
    pub controllers: Vec<String>,
    pub brokers: Vec<String>,
}

impl Cluster {
    fn nodes(&self) -> Vec<String> {
        self.controllers
            .iter()
            .chain(&self.brokers)
            .cloned()
            .collect()
    }
    fn port(&self, node: &str) -> u16 {
        if self.controllers.iter().any(|c| c == node) {
            9093
        } else {
            9092
        }
    }
    fn name_of(&self, id: i32) -> Option<String> {
        if (1..=self.controllers.len() as i32).contains(&id) {
            Some(self.controllers[id as usize - 1].clone())
        } else if id > 100 && id <= 100 + self.brokers.len() as i32 {
            Some(self.brokers[id as usize - 101].clone())
        } else {
            None
        }
    }
    fn conn(&self, node: &str, timeout: f64) -> Result<Conn, proto::NoReply> {
        Conn::connect(
            node,
            self.port(node),
            Duration::from_secs_f64(timeout),
            "oracle",
        )
    }

    /// Every controller's DescribeQuorum answer (only a node that believes it
    /// leads answers without error).
    fn quorum_views(&self, timeout: f64) -> Vec<(String, Result<Quorum, String>)> {
        self.controllers
            .iter()
            .map(|node| {
                let view = self
                    .conn(node, timeout)
                    .and_then(|mut c| proto::describe_quorum(&mut c))
                    .map_err(|e| e.0);
                (node.clone(), view)
            })
            .collect()
    }

    /// The node that answers DescribeQuorum as leader with the highest epoch.
    fn leader(&self, views: &[(String, Result<Quorum, String>)]) -> Option<(String, Quorum)> {
        views
            .iter()
            .filter_map(|(node, view)| match view {
                Ok(q) if q.error == 0 && self.name_of(q.leader).as_deref() == Some(node) => {
                    Some((node.clone(), q.clone()))
                }
                _ => None,
            })
            .max_by_key(|(_, q)| q.epoch)
    }
}

fn refs(v: &[String]) -> Vec<&str> {
    v.iter().map(String::as_str).collect()
}

fn summarize_views(views: &[(String, Result<Quorum, String>)]) -> Value {
    Value::Object(
        views
            .iter()
            .map(|(node, view)| {
                let v = match view {
                    Ok(q) if q.error == 0 => json!({"leader": q.leader, "epoch": q.epoch, "hw": q.high_watermark,
                        "voters": q.voters.iter().map(|r| json!([r.id, r.end_offset])).collect::<Vec<_>>(),
                        "observers": q.observers.iter().map(|r| json!([r.id, r.end_offset])).collect::<Vec<_>>()}),
                    Ok(q) => json!({"error": proto::error_name(q.error)}),
                    Err(e) => json!({"unreachable": e}),
                };
                (node.clone(), v)
            })
            .collect(),
    )
}

// ---------------------------------------------------------------- nemesis

enum Undo {
    Heal(Vec<kit::world::Cut>),
    Restart(Vec<String>),
    Resume(Vec<String>),
    Reboot(Vec<String>),
    Clock(Vec<ClockSkew>),
    Nothing,
}

struct Nemesis<'a> {
    cluster: &'a Cluster,
    args: &'a Args,
    rng: Rng,
    kinds: Vec<String>,
}

impl Nemesis<'_> {
    fn leader_name(&self) -> Option<String> {
        let views = self.cluster.quorum_views(0.5);
        self.cluster.leader(&views).map(|(n, _)| n)
    }

    /// Controllers, or now and then a broker, for process faults.
    fn targets(&mut self, allow_all: bool) -> (Vec<String>, String) {
        if !self.cluster.brokers.is_empty() && self.rng.chance(0.15) {
            let b = self.rng.choice(&self.cluster.brokers).clone();
            return (vec![b], "broker".into());
        }
        let mut subsets = vec!["one", "minority", "leader"];
        if allow_all {
            subsets.extend(["majority", "all"]);
        }
        let which = *self.rng.choice(&subsets);
        let controllers: Vec<&str> = self
            .cluster
            .controllers
            .iter()
            .map(String::as_str)
            .collect();
        let chosen = match which {
            "leader" => match self.leader_name() {
                Some(l) => vec![l],
                None => vec![self.rng.choice(&self.cluster.controllers).clone()],
            },
            "one" => nemesis::pick(&mut self.rng, &controllers, Subset::One)
                .iter()
                .map(|s| s.to_string())
                .collect(),
            "minority" => nemesis::pick(&mut self.rng, &controllers, Subset::Minority)
                .iter()
                .map(|s| s.to_string())
                .collect(),
            "majority" => nemesis::pick(&mut self.rng, &controllers, Subset::Majority)
                .iter()
                .map(|s| s.to_string())
                .collect(),
            _ => self.cluster.controllers.clone(),
        };
        (chosen, which.into())
    }

    fn partition(&mut self, ctl: &mut Ctl) -> kit::Result<Undo> {
        let nodes = self.cluster.nodes();
        let controllers = &self.cluster.controllers;
        let kinds = ["halves", "leader", "ring", "bridge"];
        let kind = *self.rng.choice(&kinds);
        let mut cuts = Vec::new();
        match kind {
            "halves" => {
                let all = refs(&nodes);
                let (left, right) = nemesis::random_halves(&mut self.rng, &all);
                cuts.push(nemesis::cut(ctl, &left, &right, json!({"kind": kind}))?);
            }
            "leader" => {
                let leader = self
                    .leader_name()
                    .unwrap_or_else(|| self.rng.choice(controllers).clone());
                let rest: Vec<String> = nodes.iter().filter(|n| **n != leader).cloned().collect();
                cuts.push(nemesis::cut(
                    ctl,
                    &[leader.as_str()],
                    &refs(&rest),
                    json!({"kind": kind}),
                )?);
            }
            "ring" if controllers.len() >= 5 => {
                // Each controller keeps its two ring neighbours: everyone sees
                // a majority, no two see the same one.
                let mut ring = controllers.clone();
                self.rng.shuffle(&mut ring);
                let n = ring.len();
                for i in 0..n {
                    for j in (i + 1)..n {
                        let gap = (j - i).min(n - (j - i));
                        if gap > 1 {
                            cuts.push(nemesis::cut(
                                ctl,
                                &[ring[i].as_str()],
                                &[ring[j].as_str()],
                                json!({"kind": kind, "ring": ring}),
                            )?);
                        }
                    }
                }
            }
            _ => {
                // Bridge: two halves of the controllers joined by one node.
                let mut shuffled = controllers.clone();
                self.rng.shuffle(&mut shuffled);
                let bridge = shuffled.remove(0);
                let half = shuffled.len() / 2;
                let (left, right) = shuffled.split_at(half);
                cuts.push(nemesis::cut(
                    ctl,
                    &refs(left),
                    &refs(right),
                    json!({"kind": "bridge", "bridge": bridge}),
                )?);
            }
        }
        Ok(Undo::Heal(cuts))
    }

    fn member(&mut self, _ctl: &mut Ctl) -> kit::Result<Undo> {
        let views = self.cluster.quorum_views(1.0);
        let Some((leader, quorum)) = self.cluster.leader(&views) else {
            evidence(
                "member",
                json!({"skipped": "no leader", "views": summarize_views(&views)}),
            );
            return Ok(Undo::Nothing);
        };
        let spare: Vec<&proto::Replica> = quorum
            .observers
            .iter()
            .filter(|r| (1..=self.cluster.controllers.len() as i32).contains(&r.id))
            .collect();
        let remove = quorum.voters.len() > self.args.min_voters
            && (spare.is_empty() || self.rng.chance(0.5));
        let mut conn = self
            .cluster
            .conn(&leader, 15.0)
            .map_err(|e| kit::Error(e.0))?;
        if remove {
            let victim = self.rng.choice(&quorum.voters).clone();
            let directory = proto::uuid_bytes(&victim.directory).unwrap_or_default();
            let result = proto::remove_voter(&mut conn, victim.id, directory);
            evidence(
                "fault",
                json!({"action": "remove-voter", "voter": victim.id, "directory": victim.directory,
                                     "via": leader, "epoch": quorum.epoch,
                                     "result": result.map(|(c, m)| json!([proto::error_name(c), m])).unwrap_or_else(|e| json!(e.0))}),
            );
        } else if let Some(&joiner) = spare.first() {
            let joiner = if spare.len() > 1 {
                (*self.rng.choice(&spare)).clone()
            } else {
                joiner.clone()
            };
            let host = self.cluster.controllers[joiner.id as usize - 1].clone();
            let directory = proto::uuid_bytes(&joiner.directory).unwrap_or_default();
            let result = proto::add_voter(&mut conn, joiner.id, directory, &host, 9093, 10_000);
            evidence(
                "fault",
                json!({"action": "add-voter", "voter": joiner.id, "directory": joiner.directory,
                                     "via": leader, "epoch": quorum.epoch,
                                     "result": result.map(|(c, m)| json!([proto::error_name(c), m])).unwrap_or_else(|e| json!(e.0))}),
            );
        } else {
            evidence(
                "member",
                json!({"skipped": "nothing to change", "voters": quorum.voters.len()}),
            );
        }
        Ok(Undo::Nothing)
    }

    fn apply(&mut self, ctl: &mut Ctl, kind: &str) -> kit::Result<Undo> {
        match kind {
            "partition" => self.partition(ctl),
            "kill" => {
                let (targets, subset) = self.targets(true);
                let refs: Vec<&str> = targets.iter().map(String::as_str).collect();
                nemesis::step_with(ctl, "stop", &refs, json!({"subset": subset}))?;
                Ok(Undo::Restart(targets))
            }
            "pause" => {
                let (targets, subset) = self.targets(false);
                let refs: Vec<&str> = targets.iter().map(String::as_str).collect();
                nemesis::step_with(ctl, "pause_process", &refs, json!({"subset": subset}))?;
                Ok(Undo::Resume(targets))
            }
            "crash" => {
                let (targets, subset) = self.targets(true);
                let refs: Vec<&str> = targets.iter().map(String::as_str).collect();
                nemesis::step_with(ctl, "crash", &refs, json!({"subset": subset}))?;
                Ok(Undo::Reboot(targets))
            }
            "clock" => {
                let nodes = self.cluster.nodes();
                let count = self.rng.range(1, nodes.len().min(3) as u64) as usize;
                let targets = self.rng.sample(&nodes, count);
                let mut skews = Vec::new();
                let mut applied = Vec::new();
                let world = ctl.world().ok_or("native mode has no clocks")?;
                for target in &targets {
                    let seconds = self.rng.uniform(0.1, self.args.skew)
                        * if self.rng.chance(0.5) { -1.0 } else { 1.0 };
                    let skew =
                        world.clock_skew(target.as_str(), Amount::Fixed((seconds * 1e9) as i64))?;
                    applied.push(json!([target, skew.offset_ns as f64 / 1e9]));
                    skews.push(skew);
                }
                evidence("fault", json!({"action": "clock-skew", "targets": applied}));
                Ok(Undo::Clock(skews))
            }
            "member" => self.member(ctl),
            other => crate::bail!("unknown nemesis {other}"),
        }
    }

    fn undo(&mut self, ctl: &mut Ctl, undo: Undo) -> kit::Result<()> {
        match undo {
            Undo::Heal(cuts) => {
                for cut in &cuts {
                    ctl.heal(cut)?;
                }
                evidence("fault", json!({"action": "heal", "cuts": cuts.len()}));
            }
            Undo::Restart(t) => nemesis::step(
                ctl,
                "restart",
                &t.iter().map(String::as_str).collect::<Vec<_>>(),
            )?,
            Undo::Resume(t) => nemesis::step(
                ctl,
                "resume_process",
                &t.iter().map(String::as_str).collect::<Vec<_>>(),
            )?,
            Undo::Reboot(t) => nemesis::step(
                ctl,
                "reboot",
                &t.iter().map(String::as_str).collect::<Vec<_>>(),
            )?,
            Undo::Clock(skews) => {
                let world = ctl.world().ok_or("native mode has no clocks")?;
                for skew in &skews {
                    world.heal(skew)?;
                }
                evidence(
                    "fault",
                    json!({"action": "clock-reset", "count": skews.len()}),
                );
            }
            Undo::Nothing => {}
        }
        Ok(())
    }
}

fn run_nemesis(
    ctl: &mut Ctl,
    pace: &mut Serving,
    nemesis: &mut Nemesis,
    end: Instant,
) -> kit::Result<u64> {
    let mut faults = 0;
    loop {
        let gap = nemesis.rng.uniform(0.5, 1.5) * nemesis.args.interval;
        pace.wait(ctl, gap);
        if Instant::now() >= end {
            break;
        }
        let kind = nemesis.rng.choice(&nemesis.kinds).clone();
        let undo = nemesis.apply(ctl, &kind)?;
        faults += 1;
        let views = nemesis.cluster.quorum_views(0.5);
        evidence(
            "quorum",
            json!({"after": kind, "views": summarize_views(&views)}),
        );
        let hold = nemesis.rng.uniform(0.5, 1.5) * nemesis.args.interval;
        let until = Instant::now() + Duration::from_secs_f64(hold);
        pace.wait(
            ctl,
            until
                .min(end.max(Instant::now()))
                .saturating_duration_since(Instant::now())
                .as_secs_f64()
                .max(1.0),
        );
        nemesis.undo(ctl, undo)?;
    }
    Ok(faults)
}

// ---------------------------------------------------------------- final reads

/// Read every key's list from every node until they all agree or `budget`
/// seconds pass. Returns node -> key -> list from the last round in which
/// the node answered.
fn final_append_reads(
    cluster: &Cluster,
    pace: &mut Serving,
    keys: &[i64],
    budget: f64,
) -> BTreeMap<String, BTreeMap<i64, Vec<i64>>> {
    let deadline = Instant::now() + Duration::from_secs_f64(budget);
    let mut latest: BTreeMap<String, BTreeMap<i64, Vec<i64>>> = BTreeMap::new();
    let mut round = 0;
    loop {
        round += 1;
        let mut errors = BTreeMap::new();
        for node in cluster.nodes() {
            let mut values = BTreeMap::new();
            let mut ok = true;
            for chunk in keys.chunks(100) {
                let names: Vec<String> = chunk.iter().map(|&k| key_topic(k as u64)).collect();
                let answer = cluster.conn(&node, 5.0).and_then(|mut c| {
                    proto::describe_configs(&mut c, proto::TOPIC, LIST_CONFIG, &names)
                });
                match answer {
                    Ok(results) => {
                        for r in results {
                            let k: i64 = r.resource.trim_start_matches('a').parse().unwrap_or(-1);
                            match (r.error, parse_list(r.value.as_deref())) {
                                (0, Ok(list)) => {
                                    values.insert(k, list);
                                }
                                (code, parsed) => {
                                    ok = false;
                                    errors.insert(
                                        node.clone(),
                                        json!([proto::error_name(code), parsed.err()]),
                                    );
                                }
                            }
                        }
                    }
                    Err(e) => {
                        ok = false;
                        errors.insert(node.clone(), json!(e.0));
                        break;
                    }
                }
            }
            if ok {
                latest.insert(node, values);
            }
        }
        let distinct: std::collections::BTreeSet<_> = latest.values().collect();
        let complete = latest.len() == cluster.nodes().len();
        evidence(
            "final-round",
            json!({"round": round, "answered": latest.len(), "distinct": distinct.len(),
                                        "errors": errors}),
        );
        if (complete && distinct.len() <= 1) || Instant::now() >= deadline {
            return latest;
        }
        pace.server.serve(pace.history, 5.0);
    }
}

/// Probe every topic through each broker and controller until the probes agree.
fn final_topic_probes(cluster: &Cluster, keys: &[i64]) -> Value {
    let mut out = serde_json::Map::new();
    for node in cluster.nodes() {
        let mut seen = serde_json::Map::new();
        for k in keys {
            // DescribeConfigs of a TOPIC is answered from the node's own image.
            let name = format!("t{k}");
            let answer = cluster.conn(&node, 5.0).and_then(|mut c| {
                proto::describe_configs(&mut c, proto::TOPIC, "cleanup.policy", &[name])
            });
            let v = match answer {
                Ok(r) => r
                    .first()
                    .map(|r| json!(r.error == 0))
                    .unwrap_or(Value::Null),
                Err(e) => json!(e.0),
            };
            seen.insert(k.to_string(), v);
        }
        out.insert(node, Value::Object(seen));
    }
    Value::Object(out)
}

/// Create the append workload's key topics through the first broker.
fn create_key_topics(
    cluster: &Cluster,
    server: &mut Loop,
    history: &mut History,
    count: u64,
) -> Result<(), String> {
    let broker = cluster.brokers.first().ok_or("no broker")?;
    let deadline = Instant::now() + Duration::from_secs(600);
    let mut key = 0;
    let mut retries = 0;
    while key < count {
        let answer = cluster
            .conn(broker, 10.0)
            .and_then(|mut c| proto::create_topic(&mut c, &key_topic(key), 1, 10_000, false));
        match answer {
            Ok((0 | proto::TOPIC_ALREADY_EXISTS, _, _)) => key += 1,
            other => {
                retries += 1;
                if Instant::now() >= deadline {
                    return Err(format!(
                        "creating key topic a{key}: {:?}",
                        other.map(|r| proto::error_name(r.0)).map_err(|e| e.0)
                    ));
                }
                server.serve(history, 1.0);
            }
        }
    }
    evidence("key-topics", json!({"created": count, "retries": retries}));
    Ok(())
}

// ---------------------------------------------------------------- run

pub fn run(args: Args) -> u8 {
    kit::start_clock();
    let cluster = Cluster {
        controllers: args
            .controllers
            .split(',')
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .collect(),
        brokers: args
            .brokers
            .split(',')
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .collect(),
    };
    let kinds: Vec<String> = args
        .nemesis
        .split(',')
        .map(str::trim)
        .filter(|k| !k.is_empty() && *k != "none")
        .map(str::to_owned)
        .collect();
    let rng_seed = kit::rng::entropy_seed();
    evidence(
        "start",
        json!({"workload": args.workload.name(), "nemesis": kinds, "controllers": cluster.controllers,
               "brokers": cluster.brokers, "rng_seed": rng_seed, "duration": args.duration,
               "interval": args.interval}),
    );
    let mut history = History::default();
    let mut server = match Loop::bind(args.port) {
        Ok(server) => server,
        Err(error) => return verdict(kit::INCONCLUSIVE, &error.0),
    };
    let mut ctl = match kit::connect_world(args.native.clone()) {
        Ok(ctl) => Some(ctl),
        Err(error) => return verdict(kit::INCONCLUSIVE, &format!("no controller: {error}")),
    };
    // Wait for the client and a quorum with a leader that knows every broker.
    if !server.wait_until(&mut history, 900.0, |h| h.hello.is_some()) {
        return verdict(kit::INCONCLUSIVE, "the client never connected");
    }
    let ready_deadline = Instant::now() + Duration::from_secs(900);
    loop {
        let views = cluster.quorum_views(1.0);
        let leader = cluster.leader(&views);
        let brokers_up = leader.as_ref().is_some_and(|(_, q)| {
            cluster.brokers.iter().enumerate().all(|(i, _)| {
                q.observers
                    .iter()
                    .any(|o| o.id == 101 + i as i32 && o.end_offset >= 0)
            })
        });
        // Brokers answer once they have caught up and registered.
        let answering = cluster.brokers.iter().all(|b| {
            cluster
                .conn(b, 1.0)
                .and_then(|mut c| {
                    proto::describe_configs(&mut c, proto::TOPIC, LIST_CONFIG, &["a0".into()])
                })
                .is_ok()
        });
        if leader.is_some() && brokers_up && answering {
            evidence("ready", json!({"views": summarize_views(&views)}));
            break;
        }
        if Instant::now() >= ready_deadline {
            evidence("not-ready", json!({"views": summarize_views(&views)}));
            return verdict(kit::INCONCLUSIVE, "the cluster never became ready");
        }
        server.serve(&mut history, 2.0);
    }
    if args.workload == Workload::Append {
        if let Err(error) = create_key_topics(&cluster, &mut server, &mut history, args.key_topics)
        {
            return verdict(kit::INCONCLUSIVE, &error);
        }
    }
    server.broadcast(b"start\n");
    server.serve(&mut history, args.warmup);
    let end = Instant::now() + Duration::from_secs_f64(args.duration);
    let mut faults = 0;
    let mut pace = Serving {
        server: &mut server,
        history: &mut history,
    };
    if let (Some(ctl), false) = (ctl.as_mut(), kinds.is_empty()) {
        let mut nemesis = Nemesis {
            cluster: &cluster,
            args: &args,
            rng: Rng::stream(rng_seed, 1),
            kinds: kinds.clone(),
        };
        faults = run_nemesis(ctl, &mut pace, &mut nemesis, end).unwrap_or_else(|error| {
            evidence("nemesis-failed", json!({"error": error.0}));
            0
        });
        let failures = ctl.heal_everything();
        if !failures.is_empty() {
            evidence("heal-failures", json!(failures));
        }
    }
    let remaining = end.saturating_duration_since(Instant::now()).as_secs_f64();
    pace.server.serve(pace.history, remaining);
    evidence("nemesis", json!({"faults": faults}));
    // Let the cluster recover with the workload running, then stop it.
    pace.server.serve(pace.history, args.settle);
    pace.server.broadcast(b"stop\n");
    if !pace.server.wait_until(pace.history, 120.0, |h| h.done) {
        evidence("client", json!({"done": false}));
    }
    pace.history.close();
    let views = cluster.quorum_views(2.0);
    evidence("quorum-final", json!({"views": summarize_views(&views)}));
    let keys: Vec<i64> = pace.history.keys.iter().copied().collect();
    let checked = match args.workload {
        Workload::Append => {
            let finals = final_append_reads(&cluster, &mut pace, &keys, args.converge);
            for (node, values) in &finals {
                evidence("final-values", json!({"node": node, "values": values}));
            }
            checker::check_append(&pace.history.entries, &finals, &cluster.controllers)
        }
        Workload::Topics => {
            evidence("final-topics", final_topic_probes(&cluster, &keys));
            checker::check_topics(&pace.history.entries)
        }
        Workload::Ids => checker::check_ids(&pace.history.entries),
    };
    let views = cluster.quorum_views(2.0);
    evidence("quorum-end", json!({"views": summarize_views(&views)}));
    if let Some(ctl) = ctl.as_mut() {
        if let Ok(Some(state)) = ctl.inspect() {
            evidence("inspect", json!({"machines": state.get("machines")}));
        }
    }
    conclude(
        checked,
        faults,
        !kinds.is_empty(),
        pace.history.entries.len(),
    )
}

fn conclude(checked: checker::Checked, faults: u64, faulted: bool, ops: usize) -> u8 {
    let mut result = checked.result.clone();
    result["faults"] = faults.into();
    evidence("check", result.clone());
    evidence("details", checked.details.clone());
    let summary = result.to_string();
    if checked.violation {
        return verdict(kit::VIOLATION, &summary);
    }
    if ops == 0 {
        return verdict(kit::INCONCLUSIVE, &format!("no operations; {summary}"));
    }
    if faulted && faults == 0 {
        return verdict(
            kit::INCONCLUSIVE,
            &format!("no fault was applied; {summary}"),
        );
    }
    verdict(kit::OK, &summary)
}

/// Re-check a saved oracle log from its `H` and `EVIDENCE` lines.
pub fn recheck(text: &str) -> (u8, Value) {
    let entries = kit::tagged_lines(text, "H ");
    let start = kit::evidence_of(text, "start").unwrap_or_default();
    let controllers: Vec<String> = start["controllers"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();
    let checked = if start["workload"] == "topics" {
        checker::check_topics(&entries)
    } else if start["workload"] == "ids" {
        checker::check_ids(&entries)
    } else {
        let finals: BTreeMap<String, BTreeMap<i64, Vec<i64>>> =
            kit::tagged_lines(text, "EVIDENCE ")
                .into_iter()
                .filter(|e| e["kind"] == "final-values")
                .filter_map(|e| {
                    Some((
                        e["node"].as_str()?.to_owned(),
                        serde_json::from_value(e["values"].clone()).ok()?,
                    ))
                })
                .collect();
        checker::check_append(&entries, &finals, &controllers)
    };
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

/// `kraft-probe`: send one admin request to one node every `--every` seconds
/// and print each answer (a targeted reproducer, not a Jepsen run).
#[derive(clap::Args, Debug, Clone)]
pub struct ProbeArgs {
    /// Node to ask; port 9093 for controllers (`k*`), 9092 otherwise.
    #[arg(long)]
    pub node: String,
    /// create, delete or probe.
    #[arg(long, default_value = "delete")]
    pub op: String,
    #[arg(long, default_value = "t0")]
    pub topic: String,
    #[arg(long, default_value_t = 0.05)]
    pub every: f64,
    #[arg(long, default_value_t = 600)]
    pub count: u32,
}

pub fn probe(args: ProbeArgs) -> u8 {
    kit::start_clock();
    let port = if args.node.starts_with('k') {
        9093
    } else {
        9092
    };
    // Resolve once: the container DNS can stall while peers are down.
    use std::net::ToSocketAddrs;
    let address = loop {
        if let Some(a) = (args.node.as_str(), port)
            .to_socket_addrs()
            .ok()
            .and_then(|mut a| a.next())
        {
            break a;
        }
        kit::pause(0.5);
    };
    let host = address.ip().to_string();
    let mut last = String::new();
    let mut counts: BTreeMap<String, u32> = BTreeMap::new();
    for i in 0..args.count {
        let answer = Conn::connect(&host, port, Duration::from_secs(2), "probe").and_then(
            |mut c| match args.op.as_str() {
                "create" => {
                    proto::create_topic(&mut c, &args.topic, 1, 5000, false).map(|r| (r.0, r.1))
                }
                "probe" => {
                    proto::create_topic(&mut c, &args.topic, 1, 5000, true).map(|r| (r.0, r.1))
                }
                _ => proto::delete_topic(&mut c, &args.topic, 5000),
            },
        );
        let text = match answer {
            Ok((code, message)) => format!(
                "{} {}",
                proto::error_name(code),
                message.unwrap_or_default()
            ),
            Err(e) => format!("no-reply {}", e.0.split(':').next().unwrap_or("")),
        };
        *counts
            .entry(text.split(' ').next().unwrap_or("").to_string())
            .or_default() += 1;
        if text != last {
            println!(
                "PROBE t={:.3} i={i} {} {} -> {text}",
                now(),
                args.op,
                args.node
            );
            last = text.clone();
        }
        if text.starts_with("NONE") && args.op == "delete" {
            println!("PROBE deleted; stopping");
            break;
        }
        kit::pause(args.every);
    }
    println!("PROBE counts {}", json!(counts));
    0
}
