//! The delores oracle: delores's `examples/docker-kv` REST KV store (three
//! replicas sharing one virtual log through an S3 server, slots from the
//! in-memory demo sequencer) under Jepsen's register workload. Clients PUT
//! unique values, DELETE and GET a few keys, each request to a random replica
//! with no retries. A PUT or DELETE answered 200 took effect; one answered
//! otherwise (HTTP 500, a timeout, a lost connection) may or may not have,
//! unless it was never sent (no connection) or refused as malformed.
//!
//! Once faults end and a quiet period passes, every replica reads every key.
//! Each key's history, final reads included, must be linearizable
//! ([`RegisterOf`]), so no acknowledged write may be lost; every replica must
//! answer again, and all of them must return the same value for each key.
//!
//! The oracle creates the S3 bucket; each replica's start script seeds the
//! view register itself (see `harness/jepsen/delores/start-replica.sh`).
use crate::checker::models::{
    verdict_word, RegisterOf, RegisterOp, RegisterRet, Timed, STEP_BUDGET,
};
use crate::kit::{self, http, Rng};
use serde_json::json;
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

#[derive(clap::Args, Debug, Clone)]
pub struct Args {
    /// The replicas, in order.
    #[arg(long, value_delimiter = ',', default_value = "n1,n2,n3")]
    pub replicas: Vec<String>,
    /// The replicas' HTTP port.
    #[arg(long, default_value_t = 7000)]
    pub port: u16,
    /// The S3 server the replicas share.
    #[arg(long, default_value = "s3")]
    pub s3: String,
    #[arg(long, default_value_t = 9000)]
    pub s3_port: u16,
    #[arg(long, default_value = "delores")]
    pub bucket: String,
    #[arg(long, default_value = "jepsen")]
    pub access_key: String,
    #[arg(long, default_value = "jepsen-secret")]
    pub secret_key: String,
    /// The number of keys.
    #[arg(long, default_value_t = 5)]
    pub keys: usize,
    #[arg(long, default_value_t = 6)]
    pub clients: usize,
    /// Seconds of operations.
    #[arg(long, default_value_t = 60)]
    pub duration: u64,
    /// Pause between a client's requests, in milliseconds.
    #[arg(long, default_value_t = 20)]
    pub think_ms: u64,
    /// Seconds a request may take.
    #[arg(long, default_value_t = 10)]
    pub timeout: u64,
    /// Seconds to wait after the operations before the final reads.
    #[arg(long, default_value_t = 20)]
    pub quiet: u64,
    /// Seconds after the quiet period that every replica has to answer the
    /// final reads; one that never does fails the run.
    #[arg(long, default_value_t = 120)]
    pub recover: u64,
    /// Seconds the cluster has to serve a first write and read.
    #[arg(long, default_value_t = 180)]
    pub setup: u64,
}

/// One request's outcome.
enum Answer {
    Got(u16, Vec<u8>),
    /// No connection: the request cannot have taken effect.
    NotSent(String),
    /// Sent, but no answer came back.
    Lost(String),
}

fn call(args: &Args, replica: &str, method: &str, path: &str, body: Option<&[u8]>) -> Answer {
    let timeout = Duration::from_secs(args.timeout);
    let stream = match http::connect(replica, args.port, timeout) {
        Ok(stream) => stream,
        Err(error) => return Answer::NotSent(error.0),
    };
    let host = format!("{replica}:{}", args.port);
    match http::exchange(stream, method, &host, path, body.map(|b| ("text/plain", b))) {
        Ok((status, data)) => Answer::Got(status, data),
        Err(error) => Answer::Lost(error.0),
    }
}

/// A stored value: `v{n}`; 0 is "absent" (deleted or never written).
fn body(value: u32) -> Vec<u8> {
    format!("v{value}").into_bytes()
}

fn value_of(body: &[u8]) -> Option<u32> {
    let value: u32 = std::str::from_utf8(body)
        .ok()?
        .strip_prefix('v')?
        .parse()
        .ok()?;
    (value != 0).then_some(value)
}

type KeyHistory = Timed<RegisterOp<u32>, RegisterRet<u32>>;

#[derive(Default)]
struct Tally {
    acknowledged: u64,
    /// Writes and reads without a definite answer, by status or error kind.
    outcomes: BTreeMap<String, u64>,
    /// Reads of bytes no client wrote.
    unwritten: Vec<String>,
}

/// One operation on key `k` through `replica`, recorded in `history`.
/// Returns the value a read saw (0 for absent), if it got one.
fn operate(
    args: &Args,
    key: &str,
    replica: &str,
    op: RegisterOp<u32>,
    history: &Mutex<KeyHistory>,
    tally: &Mutex<Tally>,
) -> Option<u32> {
    let id = history.lock().unwrap().invoke(op.clone());
    let path = format!("/kv/{key}");
    let answer = match &op {
        RegisterOp::Read => call(args, replica, "GET", &path, None),
        RegisterOp::Write(0) => call(args, replica, "DELETE", &path, None),
        RegisterOp::Write(value) => call(args, replica, "PUT", &path, Some(&body(*value))),
        RegisterOp::Cas(..) => unreachable!("docker-kv has no compare-and-set"),
    };
    let label = match &answer {
        Answer::Got(status, _) => format!("{} {status}", method_of(&op)),
        Answer::NotSent(_) => format!("{} not sent", method_of(&op)),
        Answer::Lost(_) => format!("{} lost", method_of(&op)),
    };
    *tally.lock().unwrap().outcomes.entry(label).or_default() += 1;
    let mut history = history.lock().unwrap();
    match (&op, answer) {
        (RegisterOp::Read, Answer::Got(200, data)) => match value_of(&data) {
            Some(value) => {
                history.complete(id, RegisterRet::Value(Some(value)));
                return Some(value);
            }
            None => {
                // Not linearizable with any write; the checker flags it too.
                history.complete(id, RegisterRet::Value(Some(u32::MAX)));
                tally.lock().unwrap().unwritten.push(format!(
                    "{key} from {replica}: {:?}",
                    String::from_utf8_lossy(&data[..data.len().min(32)])
                ));
            }
        },
        (RegisterOp::Read, Answer::Got(404, _)) => {
            history.complete(id, RegisterRet::Value(Some(0)));
            return Some(0);
        }
        // A failed read has no effect.
        (RegisterOp::Read, _) => history.fail(id),
        (RegisterOp::Write(_), Answer::Got(200, _)) => {
            history.complete(id, RegisterRet::Wrote);
            tally.lock().unwrap().acknowledged += 1;
        }
        // Never sent, or refused before it reached the log.
        (_, Answer::NotSent(_) | Answer::Got(400..=499, _)) => history.fail(id),
        // HTTP 500 and lost answers may or may not have taken effect.
        _ => {}
    }
    None
}

fn method_of(op: &RegisterOp<u32>) -> &'static str {
    match op {
        RegisterOp::Read => "GET",
        RegisterOp::Write(0) => "DELETE",
        _ => "PUT",
    }
}

/// Create the bucket, then wait until every replica has served a write and
/// a read of a key outside the workload. Returns why not, if it never did.
fn setup(args: &Args) -> Result<(), String> {
    let deadline = Instant::now() + Duration::from_secs(args.setup);
    let timeout = Duration::from_secs(args.timeout);
    let keys = (args.access_key.as_str(), args.secret_key.as_str());
    let bucket = format!("/{}", args.bucket);
    loop {
        match super::rustfs::signed_request(&args.s3, args.s3_port, keys, "PUT", &bucket, timeout) {
            Ok(200 | 409) => break,
            Ok(status) if Instant::now() > deadline => {
                return Err(format!("bucket creation failed: HTTP {status}"))
            }
            Err(error) if Instant::now() > deadline => {
                return Err(format!("S3 unreachable: {error}"))
            }
            _ => std::thread::sleep(Duration::from_millis(500)),
        }
    }
    let probe = "/kv/setup-probe";
    let mut written = false;
    let mut last = String::new();
    for replica in &args.replicas {
        loop {
            let answer = if written {
                call(args, replica, "GET", probe, None)
            } else {
                call(args, replica, "PUT", probe, Some(b"v1"))
            };
            match answer {
                Answer::Got(200, _) => {
                    if written {
                        break;
                    }
                    written = true;
                }
                Answer::Got(status, data) => {
                    last = format!(
                        "{replica}: HTTP {status} {}",
                        String::from_utf8_lossy(&data[..data.len().min(200)])
                    )
                }
                Answer::NotSent(error) | Answer::Lost(error) => {
                    last = format!("{replica}: {error}")
                }
            }
            if Instant::now() > deadline {
                return Err(format!("the cluster never served the setup probe: {last}"));
            }
            std::thread::sleep(Duration::from_millis(500));
        }
    }
    Ok(())
}

pub fn run(args: Args) -> u8 {
    kit::start_clock();
    if let Err(error) = setup(&args) {
        kit::evidence("setup", json!({"error": error}));
        return kit::verdict(kit::INCONCLUSIVE, &error);
    }
    seedbox_sdk::setup_complete(json!({"replicas": args.replicas, "keys": args.keys}));
    let keys: Vec<String> = (0..args.keys).map(|k| format!("k{k}")).collect();
    let histories: Arc<Vec<Mutex<KeyHistory>>> =
        Arc::new(keys.iter().map(|_| Mutex::new(KeyHistory::new())).collect());
    // Every key starts absent: record that before the clients start, so the
    // model's initial state (no value) is never observed.
    for history in histories.iter() {
        let mut history = history.lock().unwrap();
        let id = history.invoke(RegisterOp::Write(0));
        history.complete(id, RegisterRet::Wrote);
    }
    let tally = Arc::new(Mutex::new(Tally::default()));
    let seed = kit::rng::entropy_seed();
    kit::evidence(
        "start",
        json!({"workload": "register", "keys": args.keys, "rng_seed": seed}),
    );
    let start = Instant::now();
    let workers: Vec<_> = (0..args.clients)
        .map(|client| {
            let (args, keys, histories, tally) =
                (args.clone(), keys.clone(), histories.clone(), tally.clone());
            std::thread::spawn(move || {
                let mut rng = Rng::stream(seed, client as u64);
                let mut next = 1u32;
                while start.elapsed() < Duration::from_secs(args.duration) {
                    let k = rng.below(keys.len() as u64) as usize;
                    let replica = rng.choice(&args.replicas).clone();
                    let op = match rng.weighted(&[5.0, 4.0, 1.0]) {
                        0 => RegisterOp::Read,
                        1 => {
                            // Unique per client: client in the low bits.
                            let value = next * args.clients as u32 + client as u32 + 1;
                            next += 1;
                            RegisterOp::Write(value)
                        }
                        _ => RegisterOp::Write(0),
                    };
                    operate(&args, &keys[k], &replica, op, &histories[k], &tally);
                    std::thread::sleep(Duration::from_millis(args.think_ms));
                }
            })
        })
        .collect();
    for worker in workers {
        let _ = worker.join();
    }
    {
        let t = tally.lock().unwrap();
        kit::evidence(
            "progress",
            json!({"acknowledged": t.acknowledged, "outcomes": t.outcomes}),
        );
    }
    // Faults end with the operations; let restarts and heals settle.
    kit::pause(args.quiet as f64);
    // Every replica reads every key, retrying each read until it is answered
    // or the recovery window closes.
    let deadline = Instant::now() + Duration::from_secs(args.recover);
    let mut finals: BTreeMap<&str, BTreeMap<&str, u32>> = BTreeMap::new();
    for replica in &args.replicas {
        for (k, key) in keys.iter().enumerate() {
            loop {
                if let Some(value) =
                    operate(&args, key, replica, RegisterOp::Read, &histories[k], &tally)
                {
                    finals.entry(key).or_default().insert(replica, value);
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
        .replicas
        .iter()
        .filter(|r| !finals.values().any(|seen| seen.contains_key(r.as_str())))
        .collect();
    let divergent: Vec<_> = finals
        .iter()
        .filter(|(_, seen)| {
            let mut values = seen.values();
            let first = values.next();
            values.any(|v| Some(v) != first)
        })
        .map(|(key, seen)| json!({"key": key, "values": seen}))
        .collect();
    let mut violations = Vec::new();
    let mut exhausted = Vec::new();
    let mut counts = json!({});
    for (key, history) in keys.iter().zip(histories.iter()) {
        let history = history.lock().unwrap();
        let operations = history.operations();
        let pending = operations
            .iter()
            .filter(|o| matches!(o.outcome, wgl::OpOutcome::Pending))
            .count();
        let verdict = wgl::check_history(&RegisterOf::<u32>::default(), operations, STEP_BUDGET);
        counts[key] =
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
                    json!({"key": key, "blocked_on": blocked, "linearized": e.longest_prefix.len()}),
                );
                history.print(key);
                violations.push(key.clone());
            }
            wgl::Verdict::BudgetExhausted(_) => exhausted.push(key.clone()),
        }
    }
    let (acknowledged, unwritten) = {
        let t = tally.lock().unwrap();
        (t.acknowledged, t.unwritten.clone())
    };
    kit::evidence(
        "details",
        json!({"keys": counts, "finals": finals, "divergent": divergent, "down": down,
               "unwritten": unwritten.iter().take(10).collect::<Vec<_>>()}),
    );
    let summary = format!(
        "{} of {} keys not linearizable, {} undecided; {} acknowledged writes; {} keys divergent across replicas; {} reads of bytes no client wrote{}",
        violations.len(),
        keys.len(),
        exhausted.len(),
        acknowledged,
        divergent.len(),
        unwritten.len(),
        if down.is_empty() {
            String::new()
        } else {
            format!(
                "; {} never answered again after the faults ended",
                down.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", ")
            )
        }
    );
    if !violations.is_empty() || !divergent.is_empty() || !unwritten.is_empty() || !down.is_empty()
    {
        kit::verdict(kit::VIOLATION, &summary)
    } else if !exhausted.is_empty() || acknowledged == 0 {
        kit::verdict(kit::INCONCLUSIVE, &summary)
    } else {
        kit::verdict(kit::OK, &summary)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values_round_trip_and_reject_other_bytes() {
        assert_eq!(value_of(&body(42)), Some(42));
        assert_eq!(value_of(b"v0"), None);
        assert_eq!(value_of(b"hello"), None);
        assert_eq!(value_of(b"v42 "), None);
    }
}
