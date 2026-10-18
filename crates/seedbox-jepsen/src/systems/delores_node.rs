//! The delores jepsen-node oracle (delores's `examples/jepsen-node`): nodes
//! running SMR over a virtual log whose metadata register is CASPaxos over
//! three S3 servers (each one acceptor and one log-drive replica), slots
//! from the in-memory demo sequencer. Requests go to random nodes with no
//! retries; every answer carries an `outcome`: 200 completed, 400/412/500
//! failed with no effect (412 is a compare-and-set mismatch), 503 unknown. A
//! request that never connected failed; one whose answer was lost is unknown.
//!
//! `--workload cas-register` reads, writes and compare-and-sets a few
//! CASPaxos registers (`/cas-register/{id}`, values 1-5, absent as null);
//! each register's history, a final read of it from every node included,
//! must be linearizable ([`RegisterOf`]) and the final reads must agree.
//!
//! `--workload list-append` runs Jepsen's list-append transactions through
//! the log (`POST /txn`). The history must be serializable (the Elle-style
//! checker) with no real-time stale read ([`realtime::stale_reads`]), which
//! together check strict serializability; once faults end, every node's
//! synced state (`GET /state`) must hold every acknowledged append, and all
//! nodes must agree.
use crate::checker::elle::{self, Atom, Mop, TxnOp};
use crate::checker::models::{
    verdict_word, RegisterOf, RegisterOp, RegisterRet, Timed, STEP_BUDGET,
};
use crate::checker::realtime;
use crate::kit::{self, http, Rng};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

#[derive(clap::ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum Workload {
    /// CASPaxos registers: read, write, compare-and-set.
    CasRegister,
    /// List-append transactions through the replicated log.
    ListAppend,
}

#[derive(clap::Args, Debug, Clone)]
pub struct Args {
    #[arg(long, value_enum, default_value_t = Workload::CasRegister)]
    pub workload: Workload,
    /// The nodes, in order.
    #[arg(long, value_delimiter = ',', default_value = "n1,n2,n3")]
    pub nodes: Vec<String>,
    #[arg(long, default_value_t = 8080)]
    pub port: u16,
    /// cas-register: the number of registers.
    #[arg(long, default_value_t = 5)]
    pub registers: usize,
    /// list-append: keys active at once.
    #[arg(long, default_value_t = 8)]
    pub key_count: usize,
    /// list-append: appends to a key before it is retired.
    #[arg(long, default_value_t = 32)]
    pub max_writes_per_key: i64,
    #[arg(long, default_value_t = 4)]
    pub max_txn_length: u64,
    #[arg(long, default_value_t = 0.5)]
    pub read_fraction: f64,
    #[arg(long, default_value_t = 6)]
    pub clients: usize,
    /// Seconds of operations.
    #[arg(long, default_value_t = 60)]
    pub duration: u64,
    /// Pause between a client's requests, in milliseconds.
    #[arg(long, default_value_t = 20)]
    pub think_ms: u64,
    /// Seconds a request may take (the node's own deadline is 10 s).
    #[arg(long, default_value_t = 15)]
    pub timeout: u64,
    /// Seconds to wait after the operations before the final reads.
    #[arg(long, default_value_t = 20)]
    pub quiet: u64,
    /// Seconds after the quiet period that every node has to answer the
    /// final reads; one that never does fails the run.
    #[arg(long, default_value_t = 120)]
    pub recover: u64,
    /// Seconds the cluster has to serve a first request.
    #[arg(long, default_value_t = 240)]
    pub setup: u64,
    /// Anomaly examples printed per type.
    #[arg(long, default_value_t = 5)]
    pub examples: usize,
}

/// One request's outcome.
enum Answer {
    Got(u16, Value),
    /// No connection: the request cannot have taken effect.
    NotSent(String),
    /// Sent, but no answer came back (or an unparseable one).
    Lost(String),
}

impl Answer {
    fn label(&self) -> String {
        match self {
            Self::Got(status, body) => match body["error"].as_str() {
                Some(error) if *status != 200 => {
                    format!("{status} {}", error.chars().take(60).collect::<String>())
                }
                _ => status.to_string(),
            },
            Self::NotSent(_) => "not sent".into(),
            Self::Lost(error) if error.starts_with("HTTP") => "unparseable".into(),
            Self::Lost(_) => "lost".into(),
        }
    }
}

fn call(args: &Args, node: &str, method: &str, path: &str, body: Option<&Value>) -> Answer {
    let timeout = Duration::from_secs(args.timeout);
    let stream = match http::connect(node, args.port, timeout) {
        Ok(stream) => stream,
        Err(error) => return Answer::NotSent(error.0),
    };
    let host = format!("{node}:{}", args.port);
    let data = body.map(Value::to_string);
    let body = data.as_deref().map(|d| ("application/json", d.as_bytes()));
    match http::exchange(stream, method, &host, path, body) {
        Ok((status, data)) => match serde_json::from_slice(&data) {
            Ok(value) => Answer::Got(status, value),
            Err(_) => Answer::Lost(format!(
                "HTTP {status} unparseable {:?}",
                String::from_utf8_lossy(&data[..data.len().min(80)])
            )),
        },
        Err(error) => Answer::Lost(error.0),
    }
}

/// Outcome counts by request kind and answer.
type Outcomes = Mutex<BTreeMap<String, u64>>;

fn count(outcomes: &Outcomes, kind: &str, answer: &Answer) {
    *outcomes
        .lock()
        .unwrap()
        .entry(format!("{kind} {}", answer.label()))
        .or_default() += 1;
}

/// Wait until every node has answered `probe` with 200.
fn setup(args: &Args, probe: &dyn Fn(&str) -> Answer) -> Result<(), String> {
    let deadline = Instant::now() + Duration::from_secs(args.setup);
    for node in &args.nodes {
        let mut last;
        loop {
            match probe(node) {
                Answer::Got(200, _) => break,
                other => last = format!("{node}: {}", other.label()),
            }
            if Instant::now() > deadline {
                return Err(format!("the cluster never served a first request: {last}"));
            }
            std::thread::sleep(Duration::from_millis(500));
        }
    }
    Ok(())
}

pub fn run(args: Args) -> u8 {
    kit::start_clock();
    let probe = |node: &str| match args.workload {
        Workload::CasRegister => call(&args, node, "GET", "/cas-register/setup-probe", None),
        Workload::ListAppend => call(&args, node, "POST", "/txn", Some(&json!([["r", -1, null]]))),
    };
    if let Err(error) = setup(&args, &probe) {
        kit::evidence("setup", json!({"error": error}));
        return kit::verdict(kit::INCONCLUSIVE, &error);
    }
    seedbox_sdk::setup_complete(
        json!({"nodes": args.nodes, "workload": format!("{:?}", args.workload)}),
    );
    match args.workload {
        Workload::CasRegister => registers(Arc::new(args)),
        Workload::ListAppend => list_append(Arc::new(args)),
    }
}

// ------------------------------------------------------------ cas-register

type KeyHistory = Timed<RegisterOp<u32>, RegisterRet<u32>>;

/// A register value as JSON: 0 is absent (null).
fn json_value(value: u32) -> Value {
    if value == 0 {
        Value::Null
    } else {
        value.into()
    }
}

/// One register operation through `node`; returns a read's value (0 for
/// absent), if it got one.
fn register_op(
    args: &Args,
    id: &str,
    node: &str,
    op: RegisterOp<u32>,
    history: &Mutex<KeyHistory>,
    outcomes: &Outcomes,
) -> Option<u32> {
    let handle = history.lock().unwrap().invoke(op.clone());
    let path = format!("/cas-register/{id}");
    let (kind, answer) = match &op {
        RegisterOp::Read => ("GET", call(args, node, "GET", &path, None)),
        RegisterOp::Write(v) => (
            "PUT",
            call(args, node, "PUT", &path, Some(&json!({"value": v}))),
        ),
        RegisterOp::Cas(from, to) => (
            "CAS",
            call(
                args,
                node,
                "POST",
                &path,
                Some(&json!({"from": json_value(*from), "to": to})),
            ),
        ),
    };
    count(outcomes, kind, &answer);
    let mut history = history.lock().unwrap();
    match (&op, answer) {
        (RegisterOp::Read, Answer::Got(200, body)) => {
            let value = match &body["value"] {
                Value::Null => Some(0),
                v => v.as_u64().and_then(|v| u32::try_from(v).ok()),
            };
            match value {
                Some(value) => {
                    history.complete(handle, RegisterRet::Value(Some(value)));
                    return Some(value);
                }
                None => {
                    // No client writes anything else; the checker flags it.
                    history.complete(handle, RegisterRet::Value(Some(u32::MAX)));
                }
            }
        }
        (RegisterOp::Read, _) => history.fail(handle),
        (RegisterOp::Write(_), Answer::Got(200, _)) => history.complete(handle, RegisterRet::Wrote),
        (RegisterOp::Cas(..), Answer::Got(200, _)) => {
            history.complete(handle, RegisterRet::Cas(true))
        }
        (RegisterOp::Cas(..), Answer::Got(412, _)) => {
            history.complete(handle, RegisterRet::Cas(false))
        }
        (_, Answer::NotSent(_) | Answer::Got(400 | 412 | 500, _)) => history.fail(handle),
        // 503 and lost answers may or may not have taken effect.
        _ => {}
    }
    None
}

fn registers(args: Arc<Args>) -> u8 {
    let ids: Vec<String> = (0..args.registers).map(|r| format!("r{r}")).collect();
    let histories: Arc<Vec<Mutex<KeyHistory>>> =
        Arc::new(ids.iter().map(|_| Mutex::new(KeyHistory::new())).collect());
    // Every register starts absent (0), so the model's `None` is never seen.
    for history in histories.iter() {
        let mut history = history.lock().unwrap();
        let handle = history.invoke(RegisterOp::Write(0));
        history.complete(handle, RegisterRet::Wrote);
    }
    let outcomes: Arc<Outcomes> = Arc::default();
    let seed = kit::rng::entropy_seed();
    kit::evidence(
        "start",
        json!({"workload": "cas-register", "registers": args.registers, "rng_seed": seed}),
    );
    let start = Instant::now();
    let workers: Vec<_> = (0..args.clients)
        .map(|client| {
            let (args, ids, histories, outcomes) = (
                args.clone(),
                ids.clone(),
                histories.clone(),
                outcomes.clone(),
            );
            std::thread::spawn(move || {
                let mut rng = Rng::stream(seed, client as u64);
                while start.elapsed() < Duration::from_secs(args.duration) {
                    let r = rng.below(ids.len() as u64) as usize;
                    let node = rng.choice(&args.nodes).clone();
                    let value = rng.range(1, 6) as u32;
                    let op = match rng.weighted(&[4.0, 2.0, 4.0]) {
                        0 => RegisterOp::Read,
                        1 => RegisterOp::Write(value),
                        _ => RegisterOp::Cas(rng.range(0, 6) as u32, value),
                    };
                    register_op(&args, &ids[r], &node, op, &histories[r], &outcomes);
                    std::thread::sleep(Duration::from_millis(args.think_ms));
                }
            })
        })
        .collect();
    for worker in workers {
        let _ = worker.join();
    }
    kit::evidence("progress", json!({"outcomes": *outcomes.lock().unwrap()}));
    kit::pause(args.quiet as f64);
    let deadline = Instant::now() + Duration::from_secs(args.recover);
    let mut finals: BTreeMap<&str, BTreeMap<&str, u32>> = BTreeMap::new();
    for node in &args.nodes {
        for (r, id) in ids.iter().enumerate() {
            loop {
                if let Some(value) =
                    register_op(&args, id, node, RegisterOp::Read, &histories[r], &outcomes)
                {
                    finals.entry(id).or_default().insert(node, value);
                    break;
                }
                if Instant::now() > deadline {
                    break;
                }
                std::thread::sleep(Duration::from_secs(1));
            }
        }
    }
    let down: Vec<&String> = args
        .nodes
        .iter()
        .filter(|n| !finals.values().any(|seen| seen.contains_key(n.as_str())))
        .collect();
    let divergent: Vec<Value> = finals
        .iter()
        .filter(|(_, seen)| {
            let mut values = seen.values();
            let first = values.next();
            values.any(|v| Some(v) != first)
        })
        .map(|(id, seen)| json!({"register": id, "values": seen}))
        .collect();
    let mut violations = Vec::new();
    let mut exhausted = Vec::new();
    let mut counts = json!({});
    let mut acknowledged = 0;
    for (id, history) in ids.iter().zip(histories.iter()) {
        let history = history.lock().unwrap();
        let operations = history.operations();
        acknowledged += operations
            .iter()
            .filter(|o| !matches!(o.op, RegisterOp::Read))
            .filter(|o| {
                matches!(
                    o.outcome,
                    wgl::OpOutcome::Completed {
                        ret: RegisterRet::Wrote | RegisterRet::Cas(true),
                        ..
                    }
                )
            })
            .count()
            - 1;
        let pending = operations
            .iter()
            .filter(|o| matches!(o.outcome, wgl::OpOutcome::Pending))
            .count();
        let verdict = wgl::check_history(&RegisterOf::<u32>::default(), operations, STEP_BUDGET);
        counts[id] =
            json!({"ops": operations.len(), "pending": pending, "verdict": verdict_word(&verdict)});
        match verdict {
            wgl::Verdict::Linearizable => {}
            wgl::Verdict::NotLinearizable(e) => {
                let blocked = e.blocked_on.map(|i| {
                    let o = &operations[i];
                    format!("{:?} -> {:?}", o.op, o.outcome)
                });
                kit::evidence(
                    "nonlinearizable",
                    json!({"register": id, "blocked_on": blocked, "linearized": e.longest_prefix.len()}),
                );
                history.print(id);
                violations.push(id.clone());
            }
            wgl::Verdict::BudgetExhausted(_) => exhausted.push(id.clone()),
        }
    }
    kit::evidence(
        "details",
        json!({"registers": counts, "finals": finals, "divergent": divergent, "down": down}),
    );
    let summary = format!(
        "{} of {} registers not linearizable, {} undecided; {} acknowledged writes and compare-and-sets; {} registers divergent across nodes{}",
        violations.len(),
        ids.len(),
        exhausted.len(),
        acknowledged,
        divergent.len(),
        never_answered(&down)
    );
    if !violations.is_empty() || !divergent.is_empty() || !down.is_empty() {
        kit::verdict(kit::VIOLATION, &summary)
    } else if !exhausted.is_empty() || acknowledged == 0 {
        kit::verdict(kit::INCONCLUSIVE, &summary)
    } else {
        kit::verdict(kit::OK, &summary)
    }
}

fn never_answered(down: &[&String]) -> String {
    if down.is_empty() {
        String::new()
    } else {
        format!(
            "; {} never answered again after the faults ended",
            down.iter()
                .map(|s| s.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )
    }
}

// ------------------------------------------------------------ list-append

/// A transaction's micro-operations as the node takes them.
fn txn_body(mops: &[Mop]) -> Value {
    Value::Array(mops.iter().map(Mop::to_json).collect())
}

struct Appends {
    args: Arc<Args>,
    txns: Mutex<Vec<TxnOp>>,
    outcomes: Outcomes,
}

impl Appends {
    fn record(&self, process: Atom, kind: &str, value: Vec<Mop>, t0: f64, error: Option<String>) {
        let t1 = kit::now();
        let mut txns = self.txns.lock().unwrap();
        let index = txns.len() as i64;
        let op = elle::txn(
            index,
            kind,
            value,
            Some(process),
            elle::seconds_to_ns(t0),
            elle::seconds_to_ns(t1),
            error,
        )
        .expect("known transaction type");
        kit::history_line(&json!({
            "i": index, "p": op.op.process.as_ref().map(Atom::to_json), "type": kind,
            "t0": t0, "t1": t1, "value": elle::h_line(&op)["value"], "error": op.op.error,
        }));
        txns.push(op);
    }

    fn client(
        &self,
        client: usize,
        keys: &Mutex<crate::systems::postgres::Keys>,
        seed: u64,
        start: Instant,
    ) {
        let mut rng = Rng::stream(seed, client as u64);
        // A client with an unknown transaction continues as a new process.
        let mut process = client;
        while start.elapsed() < Duration::from_secs(self.args.duration) {
            let mops = keys.lock().unwrap().transaction();
            let node = rng.choice(&self.args.nodes).clone();
            let t0 = kit::now();
            let answer = call(&self.args, &node, "POST", "/txn", Some(&txn_body(&mops)));
            count(&self.outcomes, "TXN", &answer);
            let (kind, value, error) = match answer {
                Answer::Got(200, body) => {
                    let value: Option<Vec<Mop>> = body["txn"]
                        .as_array()
                        .and_then(|mops| mops.iter().map(Mop::from_json).collect());
                    match value {
                        Some(value) => ("ok", value, None),
                        None => ("info", mops.clone(), Some(format!("unparseable {body}"))),
                    }
                }
                Answer::Got(400 | 412 | 500, body) => (
                    "fail",
                    mops.clone(),
                    body["error"].as_str().map(str::to_owned),
                ),
                Answer::NotSent(error) => ("fail", mops.clone(), Some(error)),
                other => ("info", mops.clone(), Some(other.label())),
            };
            self.record(Atom::Int(process as i64), kind, value, t0, error);
            if kind == "info" {
                process += self.args.clients;
            }
            std::thread::sleep(Duration::from_millis(self.args.think_ms));
        }
    }
}

/// `GET /state` from `node`: every key's list.
fn state(args: &Args, node: &str) -> Result<BTreeMap<Atom, Vec<i64>>, String> {
    match call(args, node, "GET", "/state", None) {
        Answer::Got(200, body) => {
            let mut lists = BTreeMap::new();
            for entry in body["lists"].as_array().ok_or("state without lists")? {
                let key = Atom::from_json(&entry[0]).ok_or("state key")?;
                let values = entry[1]
                    .as_array()
                    .and_then(|vs| vs.iter().map(Value::as_i64).collect::<Option<Vec<_>>>())
                    .ok_or("state list")?;
                lists.insert(key, values);
            }
            Ok(lists)
        }
        other => Err(other.label()),
    }
}

fn list_append(args: Arc<Args>) -> u8 {
    let seed = kit::rng::entropy_seed();
    kit::evidence(
        "start",
        json!({"workload": "list-append", "keys": args.key_count, "rng_seed": seed}),
    );
    let keys = Arc::new(Mutex::new(crate::systems::postgres::Keys::new(
        seed,
        args.key_count,
        args.max_writes_per_key,
        args.max_txn_length,
        args.read_fraction,
    )));
    let run = Arc::new(Appends {
        args: args.clone(),
        txns: Mutex::new(Vec::new()),
        outcomes: Mutex::default(),
    });
    let start = Instant::now();
    let workers: Vec<_> = (0..args.clients)
        .map(|client| {
            let (run, keys) = (run.clone(), keys.clone());
            std::thread::spawn(move || run.client(client, &keys, seed, start))
        })
        .collect();
    for worker in workers {
        let _ = worker.join();
    }
    kit::evidence(
        "progress",
        json!({"outcomes": *run.outcomes.lock().unwrap()}),
    );
    kit::pause(args.quiet as f64);
    // Every node's synced state, retried until the recovery window closes;
    // each becomes a final read-only transaction of every touched key.
    let touched: BTreeSet<i64> = keys.lock().unwrap().touched.clone();
    let deadline = Instant::now() + Duration::from_secs(args.recover);
    let mut finals: BTreeMap<String, BTreeMap<Atom, Vec<i64>>> = BTreeMap::new();
    for node in &args.nodes {
        loop {
            let t0 = kit::now();
            match state(&args, node) {
                Ok(lists) => {
                    let reads: Vec<Mop> = touched
                        .iter()
                        .map(|k| {
                            let key = Atom::Int(*k);
                            let list = lists.get(&key).cloned().unwrap_or_default();
                            Mop::Read(key, Some(list))
                        })
                        .collect();
                    run.record(Atom::Str(format!("final-{node}")), "ok", reads, t0, None);
                    finals.insert(node.clone(), lists);
                    break;
                }
                Err(error) => {
                    if Instant::now() > deadline {
                        kit::evidence("final-read-error", json!({"node": node, "error": error}));
                        break;
                    }
                    std::thread::sleep(Duration::from_secs(1));
                }
            }
        }
    }
    let txns = run.txns.lock().unwrap().clone();
    let is_final =
        |t: &TxnOp| matches!(&t.op.process, Some(Atom::Str(p)) if p.starts_with("final"));
    // Acknowledged appends a node's final state lacks: lost.
    let mut lost: Vec<Value> = Vec::new();
    for (node, lists) in &finals {
        for t in txns.iter().filter(|t| !is_final(t)) {
            if let wgl::OpOutcome::Completed { ret, .. } = &t.outcome {
                for mop in ret {
                    if let Mop::Append(k, v) = mop {
                        if !lists.get(k).is_some_and(|l| l.contains(v)) {
                            lost.push(json!({"node": node, "key": k.to_json(), "value": v}));
                        }
                    }
                }
            }
        }
    }
    let gone: BTreeSet<(String, i64)> = lost
        .iter()
        .filter_map(|l| Some((l["key"].to_string(), l["value"].as_i64()?)))
        .collect();
    let stale = realtime::stale_reads(
        &txns,
        |t| !is_final(t),
        |k, v| gone.contains(&(k.to_json().to_string(), v)),
    );
    let mut values = finals.values();
    let first = values.next();
    let divergent = values.any(|l| Some(l) != first);
    let down: Vec<&String> = args
        .nodes
        .iter()
        .filter(|n| !finals.contains_key(*n))
        .collect();
    let committed = txns
        .iter()
        .filter(|t| !is_final(t) && elle::kind_of(t) == "ok")
        .count();
    let result = match crate::systems::postgres::report(&txns, "serializable", args.examples) {
        Ok(result) => result,
        Err(error) => return kit::verdict(kit::INCONCLUSIVE, &format!("checker: {error}")),
    };
    kit::evidence(
        "details",
        json!({"stale": stale.iter().take(args.examples).collect::<Vec<_>>(), "stale_count": stale.len(),
               "lost": lost.iter().take(10).collect::<Vec<_>>(), "lost_count": lost.len(),
               "divergent": divergent, "down": down}),
    );
    let valid = result.valid && stale.is_empty() && lost.is_empty();
    let summary = format!(
        "{} checked as strict serializable: {} {} stale reads, {} lost appends, nodes {}; txns {}{}",
        if valid { "valid" } else { "INVALID" },
        json!(result.counts),
        stale.len(),
        lost.len(),
        if divergent { "diverge" } else { "agree" },
        json!(result.txns),
        never_answered(&down)
    );
    if !valid || divergent || !down.is_empty() {
        kit::verdict(kit::VIOLATION, &summary)
    } else if committed == 0 {
        kit::verdict(kit::INCONCLUSIVE, &summary)
    } else {
        kit::verdict(kit::OK, &summary)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transactions_encode_as_the_node_takes_them() {
        let mops = vec![Mop::Append(Atom::Int(1), 3), Mop::Read(Atom::Int(2), None)];
        assert_eq!(txn_body(&mops), json!([["append", 1, 3], ["r", 2, null]]));
        assert_eq!(json_value(0), Value::Null);
        assert_eq!(json_value(4), json!(4));
    }
}
