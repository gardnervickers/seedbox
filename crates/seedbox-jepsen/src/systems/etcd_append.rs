//! The etcd list-append oracle: Jepsen's etcd `append` workload, checked
//! with the Elle-style list-append checker. Each key holds a comma-separated
//! list. A transaction of reads and appends on a few keys runs in two steps
//! through one random member: a read-only txn of every key it touches (an
//! atomic snapshot), then, if it appends, a txn that requires each of those
//! keys' mod revisions to be unchanged and puts the extended lists. Its reads
//! come from the snapshot plus its own earlier appends. A failed snapshot or
//! a failed guard is a definite abort; a write txn without a reply may have
//! committed. Read-only transactions commit at their snapshot.
//!
//! The history must be serializable. Afterwards, as in the register oracle,
//! every member must serve reads again, a final read covers every key, and
//! the members' `HashKV` must agree.
use super::etcd_gateway::{b64, number, unb64};
use super::etcd_register::{call, compare_hashes, recover, Recovery, Reply};
use super::postgres::{report, Keys};
use crate::checker::elle::{self, Atom, Mop, TxnOp};
use crate::kit::{self, emit, Rng};
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

#[derive(clap::Args, Debug, Clone)]
pub struct Args {
    /// The etcd members, in order.
    #[arg(long, value_delimiter = ',', default_value = "n1,n2,n3,n4,n5")]
    pub members: Vec<String>,
    /// Seconds of transactions.
    #[arg(long, default_value_t = 60)]
    pub duration: u64,
    #[arg(long, default_value_t = 10)]
    pub clients: usize,
    #[arg(long, default_value_t = 8)]
    pub key_count: usize,
    #[arg(long, default_value_t = 32)]
    pub max_writes_per_key: i64,
    #[arg(long, default_value_t = 4)]
    pub max_txn_length: u64,
    #[arg(long, default_value_t = 0.5)]
    pub read_fraction: f64,
    /// Pause between a client's transactions, in milliseconds.
    #[arg(long, default_value_t = 10)]
    pub think_ms: u64,
    /// Seconds a request may take.
    #[arg(long, default_value_t = 5.0)]
    pub timeout: f64,
    /// Power-fail every member together about this often, in seconds; 0 never does.
    #[arg(long, default_value_t = 0.0)]
    pub crash_all_every: f64,
    #[arg(long, default_value_t = 2.0)]
    pub crash_all_down: f64,
    /// Seconds to wait after the transactions before the recovery check.
    #[arg(long, default_value_t = 15)]
    pub quiet: u64,
    /// Seconds after the quiet period that every member has to serve a read.
    #[arg(long, default_value_t = 120)]
    pub recover: u64,
    /// Anomaly examples printed per type.
    #[arg(long, default_value_t = 5)]
    pub examples: usize,
}

fn key_name(k: i64) -> String {
    format!("a{k}")
}

fn parse_list(text: &str) -> Option<Vec<i64>> {
    if text.is_empty() {
        return Some(Vec::new());
    }
    text.split(',').map(|v| v.parse().ok()).collect()
}

fn format_list(list: &[i64]) -> String {
    list.iter()
        .map(i64::to_string)
        .collect::<Vec<_>>()
        .join(",")
}

/// One key as a snapshot saw it: its list and mod revision (0 when absent).
type Snapshot = BTreeMap<i64, (Vec<i64>, i64)>;

enum Outcome {
    Ok(Vec<Mop>),
    Fail(String),
    Info(String),
}

/// A read-only txn of `keys`: one atomic snapshot.
fn snapshot(member: &str, keys: &[i64], timeout: f64) -> Result<Snapshot, String> {
    let ranges: Vec<Value> = keys
        .iter()
        .map(|k| json!({"request_range": {"key": b64(&key_name(*k))}}))
        .collect();
    match call(member, "/v3/kv/txn", &json!({"success": ranges}), timeout) {
        Reply::Ok(reply) => {
            let responses = reply["responses"].as_array().cloned().unwrap_or_default();
            if responses.len() != keys.len() {
                return Err(format!(
                    "{} responses for {} keys",
                    responses.len(),
                    keys.len()
                ));
            }
            let mut out = Snapshot::new();
            for (k, response) in keys.iter().zip(responses) {
                let kv = &response["response_range"]["kvs"][0];
                let entry = if kv.is_null() {
                    (Vec::new(), 0)
                } else {
                    let text = unb64(kv["value"].as_str().unwrap_or(""));
                    let list = parse_list(&text)
                        .ok_or_else(|| format!("{}: unparseable list {text:?}", key_name(*k)))?;
                    (list, number(&kv["mod_revision"]))
                };
                out.insert(*k, entry);
            }
            Ok(out)
        }
        other => Err(other.label()),
    }
}

fn execute(member: &str, mops: &[Mop], timeout: f64) -> Outcome {
    let mut keys: Vec<i64> = mops
        .iter()
        .filter_map(|m| match m.key() {
            Atom::Int(k) => Some(*k),
            Atom::Str(_) => None,
        })
        .collect();
    keys.sort_unstable();
    keys.dedup();
    let snap = match snapshot(member, &keys, timeout) {
        Ok(snap) => snap,
        // Counted as garbage by the verdict.
        Err(error) if error.contains("unparseable") => return Outcome::Fail(error),
        Err(error) => return Outcome::Fail(format!("snapshot: {error}")),
    };
    let mut lists: BTreeMap<i64, Vec<i64>> =
        snap.iter().map(|(k, (l, _))| (*k, l.clone())).collect();
    let mut value = Vec::new();
    let mut appended = false;
    for mop in mops {
        match mop {
            Mop::Read(Atom::Int(k), _) => {
                value.push(Mop::Read(Atom::Int(*k), Some(lists[k].clone())));
            }
            Mop::Append(Atom::Int(k), v) => {
                lists.get_mut(k).expect("snapshotted").push(*v);
                appended = true;
                value.push(mop.clone());
            }
            _ => {}
        }
    }
    if !appended {
        return Outcome::Ok(value);
    }
    let compare: Vec<Value> = snap
        .iter()
        .map(|(k, (_, revision))| {
            json!({"key": b64(&key_name(*k)), "target": "MOD", "result": "EQUAL",
                   "mod_revision": revision.to_string()})
        })
        .collect();
    let puts: Vec<Value> = lists
        .iter()
        .filter(|(k, list)| snap[k].0.len() != list.len())
        .map(|(k, list)| {
            json!({"request_put": {"key": b64(&key_name(*k)), "value": b64(&format_list(list))}})
        })
        .collect();
    match call(
        member,
        "/v3/kv/txn",
        &json!({"compare": compare, "success": puts}),
        timeout,
    ) {
        Reply::Ok(reply) if reply["succeeded"].as_bool().unwrap_or(false) => Outcome::Ok(value),
        Reply::Ok(_) => Outcome::Fail("guard failed".into()),
        Reply::NotSent(error) => Outcome::Fail(format!("not sent: {error}")),
        other => Outcome::Info(other.label()),
    }
}

struct Run {
    args: Args,
    keys: Mutex<Keys>,
    txns: Mutex<Vec<TxnOp>>,
    errors: Mutex<BTreeMap<String, u64>>,
}

impl Run {
    fn record(
        &self,
        process: usize,
        kind: &str,
        invoke: &[Mop],
        value: Option<Vec<Mop>>,
        t0: f64,
        error: Option<String>,
    ) {
        let t1 = kit::now_us();
        let mut txns = self.txns.lock().unwrap();
        let index = txns.len() as i64;
        let value = match value {
            Some(value) if kind == "ok" => value,
            _ => invoke.iter().map(Mop::invocation).collect(),
        };
        let op = elle::txn(
            index,
            kind,
            value.clone(),
            Some(Atom::Int(process as i64)),
            elle::seconds_to_ns(t0),
            elle::seconds_to_ns(t1),
            error.clone(),
        )
        .expect("known transaction type");
        txns.push(op);
        emit(&format!(
            "H {}",
            json!({"i": index, "p": process, "type": kind, "t0": t0, "t1": t1,
                   "value": value.iter().map(Mop::to_json).collect::<Vec<_>>(), "error": error})
        ));
    }

    fn client(&self, client: usize, seed: u64, start: Instant) {
        let mut rng = Rng::stream(seed, client as u64);
        // A client with an indeterminate transaction continues as a new process.
        let mut process = client;
        while start.elapsed() < Duration::from_secs(self.args.duration) {
            let mops = self.keys.lock().unwrap().transaction();
            let member = rng.choice(&self.args.members).clone();
            let t0 = kit::now_us();
            let (kind, value, error) = match execute(&member, &mops, self.args.timeout) {
                Outcome::Ok(value) => ("ok", Some(value), None),
                Outcome::Fail(error) => ("fail", None, Some(error)),
                Outcome::Info(error) => ("info", None, Some(error)),
            };
            if let Some(error) = &error {
                *self
                    .errors
                    .lock()
                    .unwrap()
                    .entry(format!("{kind} {error}"))
                    .or_default() += 1;
            }
            self.record(process, kind, &mops, value, t0, error);
            if kind == "info" {
                process += self.args.clients;
            }
            std::thread::sleep(Duration::from_millis(self.args.think_ms));
        }
    }
}

pub fn run(args: Args) -> u8 {
    kit::start_clock();
    let boot = recover(&args.members, 240, "boot", None);
    if !boot.down.is_empty() {
        return kit::verdict(
            kit::INCONCLUSIVE,
            &format!("members never served a read: {}", boot.down.join(", ")),
        );
    }
    let mut ctl = kit::connect_world(None).ok();
    let seed = kit::rng::entropy_seed();
    kit::evidence(
        "start",
        json!({"workload": "append", "clients": args.clients, "rng_seed": seed}),
    );
    seedbox_sdk::setup_complete(json!({"members": args.members, "workload": "append"}));
    if args.crash_all_every > 0.0 {
        crate::nemesis::crash_all_repeatedly(
            args.members.clone(),
            args.crash_all_every,
            args.crash_all_down,
            0.0,
            args.duration as f64,
        );
    }
    let run = Arc::new(Run {
        keys: Mutex::new(Keys::new(
            seed,
            args.key_count,
            args.max_writes_per_key,
            args.max_txn_length,
            args.read_fraction,
        )),
        args: args.clone(),
        txns: Mutex::new(Vec::new()),
        errors: Mutex::new(BTreeMap::new()),
    });
    let start = Instant::now();
    let workers: Vec<_> = (0..args.clients)
        .map(|client| {
            let run = run.clone();
            std::thread::spawn(move || run.client(client, seed, start))
        })
        .collect();
    for worker in workers {
        let _ = worker.join();
    }
    if kit::under_seedbox() {
        if let Err(error) = seedbox_sdk::control::quiet(Duration::from_secs(3600)) {
            kit::evidence("quiet-failed", json!({"error": error.to_string()}));
        }
    }
    kit::evidence("progress", json!({"errors": *run.errors.lock().unwrap()}));
    kit::pause(args.quiet as f64);
    let Recovery {
        live,
        down,
        restarted,
    } = recover(&args.members, args.recover, "final", ctl.as_mut());
    // A final read of every key touched, as one read-only transaction.
    let touched: Vec<i64> = run.keys.lock().unwrap().touched.iter().copied().collect();
    let reads: Vec<Mop> = touched
        .iter()
        .map(|k| Mop::Read(Atom::Int(*k), None))
        .collect();
    let mut final_read = false;
    'read: for attempt in 0..3u64 {
        for member in &live {
            let t0 = kit::now_us();
            if let Outcome::Ok(value) = execute(member, &reads, 10.0) {
                run.record(usize::MAX / 2, "ok", &reads, Some(value), t0, None);
                final_read = true;
                break 'read;
            }
        }
        kit::pause(1.0 + attempt as f64);
    }
    let (diverged, hashes) = compare_hashes(&live);
    kit::evidence("hashkv", hashes);
    let garbage: Vec<String> = run
        .errors
        .lock()
        .unwrap()
        .keys()
        .filter(|e| e.contains("unparseable"))
        .cloned()
        .collect();
    let txns = run.txns.lock().unwrap().clone();
    let committed = txns.iter().filter(|t| elle::kind_of(t) == "ok").count();
    let result = match report(&txns, "serializable", args.examples) {
        Ok(result) => result,
        Err(error) => return kit::verdict(kit::INCONCLUSIVE, &format!("checker: {error}")),
    };
    let mut text = format!(
        "{} checked as serializable: {} txns {}",
        if result.valid { "valid" } else { "INVALID" },
        json!(result.counts),
        json!(result.txns)
    );
    if let Some(diverged) = &diverged {
        text.push_str(&format!("; {diverged}"));
    }
    if !garbage.is_empty() {
        text.push_str(&format!("; values no client wrote: {garbage:?}"));
    }
    if !restarted.is_empty() {
        text.push_str(&format!(
            "; {} restarted after not serving",
            restarted.join(", ")
        ));
    }
    if !down.is_empty() {
        text.push_str(&format!(
            "; {} never served a read again after the faults ended",
            down.join(", ")
        ));
    }
    if !result.valid || diverged.is_some() || !down.is_empty() || !garbage.is_empty() {
        kit::verdict(kit::VIOLATION, &text)
    } else if committed <= 1 || !final_read {
        kit::verdict(
            kit::INCONCLUSIVE,
            &format!("{text}; no final read or commits"),
        )
    } else {
        kit::verdict(kit::OK, &text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lists_round_trip() {
        assert_eq!(parse_list(""), Some(vec![]));
        assert_eq!(parse_list(&format_list(&[1, 2, 30])), Some(vec![1, 2, 30]));
        assert_eq!(parse_list("1,x"), None);
    }
}
