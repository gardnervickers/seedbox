//! Jepsen's PostgreSQL list-append workload as a Seedbox oracle.
//!
//! Each key is a row of one of `--tables` tables, `txnN (id int primary key,
//! sk int, val text)`, chosen by `k mod tables`, where `val` is a
//! comma-separated list. An append is `INSERT ... ON CONFLICT (id) DO UPDATE
//! SET val = CONCAT(t.val, ',', 'v')`, so the first append to a key inserts a
//! fresh row; a read is `SELECT val ... WHERE id = k` (or `sk = k` for the
//! `--sk-reads` fraction) and a missing row reads as []. `--clients` threads
//! run transactions back to back for `--duration` seconds; keys follow Elle's
//! generator (exponentially weighted active keys, retired after
//! `--max-writes-per-key` appends). A server error before or at COMMIT is a
//! definite abort (`fail`); a timeout or lost connection is indeterminate
//! (`info`) and the client continues as a new process. Nothing is retried.
//! A final read-only transaction reads every key the run touched, and
//! [`crate::checker::elle`] checks the history.
use super::pg_proto::{Pg, PgError};
use crate::checker::elle::{self, Atom, Mop, TxnOp};
use crate::kit::{self, emit, evidence, now_us, verdict, Rng};
use serde_json::{json, Value};
use std::{
    collections::BTreeSet,
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
};

#[derive(clap::Args, Debug, Clone)]
pub struct Args {
    #[arg(long, default_value = "pg")]
    pub host: String,
    #[arg(long, default_value_t = 5432)]
    pub port: u16,
    #[arg(long, value_enum, default_value = "serializable")]
    pub isolation: Isolation,
    /// Consistency model to check [default: from --isolation].
    #[arg(long)]
    pub model: Option<String>,
    #[arg(long, default_value_t = 20)]
    pub clients: usize,
    #[arg(long, default_value_t = 60.0)]
    pub duration: f64,
    #[arg(long, default_value_t = 3)]
    pub tables: i64,
    #[arg(long, default_value_t = 10)]
    pub key_count: usize,
    #[arg(long, default_value_t = 32)]
    pub max_writes_per_key: i64,
    #[arg(long, default_value_t = 4)]
    pub max_txn_length: u64,
    #[arg(long, default_value_t = 0.5)]
    pub read_fraction: f64,
    #[arg(long, default_value_t = 0.0)]
    pub sk_reads: f64,
    /// Seconds before a statement counts as indeterminate.
    #[arg(long, default_value_t = 10.0)]
    pub timeout: f64,
    #[arg(long, default_value_t = 300.0)]
    pub ready_timeout: f64,
    /// Generator seed; Seedbox's seed varies the schedule.
    #[arg(long, default_value_t = 1)]
    pub seed: u64,
    /// Anomaly examples printed per type.
    #[arg(long, default_value_t = 5)]
    pub examples: usize,
    #[arg(long, default_value = "")]
    pub label: String,
}

#[derive(clap::ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum Isolation {
    /// SERIALIZABLE, checked as serializable.
    Serializable,
    /// REPEATABLE READ (PostgreSQL's snapshot isolation), checked as snapshot isolation.
    RepeatableRead,
    /// READ COMMITTED, checked as read committed.
    ReadCommitted,
}

impl Isolation {
    fn sql(self) -> &'static str {
        match self {
            Self::Serializable => "SERIALIZABLE",
            Self::RepeatableRead => "REPEATABLE READ",
            Self::ReadCommitted => "READ COMMITTED",
        }
    }
    fn name(self) -> &'static str {
        match self {
            Self::Serializable => "serializable",
            Self::RepeatableRead => "repeatable-read",
            Self::ReadCommitted => "read-committed",
        }
    }
    fn model(self) -> &'static str {
        match self {
            Self::Serializable => "serializable",
            Self::RepeatableRead => "snapshot-isolation",
            Self::ReadCommitted => "read-committed",
        }
    }
}

/// Elle's list-append key generator: a pool of active keys, exponential
/// choice, and retirement after enough appends.
pub struct Keys {
    rng: Rng,
    active: Vec<i64>,
    next_key: i64,
    writes: std::collections::BTreeMap<i64, i64>,
    pub touched: BTreeSet<i64>,
    weights: Vec<f64>,
    max_writes: i64,
    max_length: u64,
    read_fraction: f64,
}

impl Keys {
    pub fn new(
        seed: u64,
        key_count: usize,
        max_writes: i64,
        max_length: u64,
        read_fraction: f64,
    ) -> Self {
        Self {
            rng: Rng::new(seed),
            active: (0..key_count as i64).collect(),
            next_key: key_count as i64,
            writes: Default::default(),
            touched: BTreeSet::new(),
            weights: (0..key_count).map(|i| 2f64.powi(-(i as i32))).collect(),
            max_writes,
            max_length,
            read_fraction,
        }
    }

    pub fn transaction(&mut self) -> Vec<Mop> {
        let mut ops = Vec::new();
        for _ in 0..self.rng.range(1, self.max_length) {
            let slot = self.rng.weighted(&self.weights);
            let k = self.active[slot];
            self.touched.insert(k);
            if self.rng.chance(self.read_fraction) {
                ops.push(Mop::Read(Atom::Int(k), None));
                continue;
            }
            let count = self.writes.entry(k).or_insert(0);
            *count += 1;
            ops.push(Mop::Append(Atom::Int(k), *count));
            if *count >= self.max_writes {
                self.active[slot] = self.next_key;
                self.next_key += 1;
            }
        }
        ops
    }
}

fn table(k: i64, tables: i64) -> String {
    format!("txn{}", k.rem_euclid(tables))
}

/// A comma-separated list value; a missing row or NULL reads as [].
pub fn parse(rows: &[Vec<Option<String>>]) -> Vec<i64> {
    match rows.first().and_then(|row| row.first()).cloned().flatten() {
        Some(text) if !text.is_empty() => text.split(',').filter_map(|x| x.parse().ok()).collect(),
        _ => Vec::new(),
    }
}

enum Outcome {
    Ok(Vec<Mop>),
    Fail(String, bool),
}

fn execute(pg: &mut Pg, ops: &[Mop], args: &Args, rng: &mut Rng) -> Result<Outcome, String> {
    let run = |pg: &mut Pg, rng: &mut Rng| -> Result<(Vec<Mop>, Option<String>), PgError> {
        pg.query(&format!("BEGIN ISOLATION LEVEL {}", args.isolation.sql()))?;
        let mut value = Vec::new();
        for mop in ops {
            match mop {
                Mop::Read(Atom::Int(k), _) => {
                    let column = if rng.chance(args.sk_reads) {
                        "sk"
                    } else {
                        "id"
                    };
                    let (rows, _) = pg.query(&format!(
                        "SELECT val FROM {} WHERE {column} = {k}",
                        table(*k, args.tables)
                    ))?;
                    value.push(Mop::Read(Atom::Int(*k), Some(parse(&rows))));
                }
                Mop::Append(Atom::Int(k), v) => {
                    let t = table(*k, args.tables);
                    pg.query(&format!(
                        "INSERT INTO {t} AS t (id, sk, val) VALUES ({k}, {k}, '{v}') \
                         ON CONFLICT (id) DO UPDATE SET val = CONCAT(t.val, ',', '{v}') WHERE t.id = {k}"
                    ))?;
                    value.push(mop.clone());
                }
                _ => {}
            }
        }
        let (_, tag) = pg.query("COMMIT")?;
        Ok((value, tag))
    };
    match run(pg, rng) {
        Ok((value, tag)) if tag.as_deref() == Some("COMMIT") => Ok(Outcome::Ok(value)),
        Ok((_, tag)) => Ok(Outcome::Fail(
            format!("commit tag {}", tag.unwrap_or_default()),
            false,
        )),
        Err(PgError::Server { code, .. }) => {
            // Still a definite abort when the rollback itself is lost: the server rejected it.
            let broken = matches!(pg.query("ROLLBACK"), Err(PgError::Indeterminate(_)));
            Ok(Outcome::Fail(code, broken))
        }
        Err(error) => Err(error.to_string()),
    }
}

struct Run {
    args: Args,
    keys: Mutex<Keys>,
    txns: Mutex<Vec<TxnOp>>,
    started: Instant,
    stop_at: Instant,
}

impl Run {
    #[allow(clippy::too_many_arguments)]
    fn record(
        &self,
        process: Value,
        kind: &str,
        invoke: &[Mop],
        value: Option<Vec<Mop>>,
        t0: f64,
        t1: f64,
        error: Option<String>,
    ) {
        let mut txns = self.txns.lock().unwrap();
        let index = txns.len() as i64;
        let value = if kind == "ok" {
            value.unwrap_or_default()
        } else {
            invoke.iter().map(Mop::invocation).collect()
        };
        let op = elle::txn(
            index,
            kind,
            value.clone(),
            Atom::from_json(&process),
            elle::seconds_to_ns(t0),
            elle::seconds_to_ns(t1),
            error.clone(),
        )
        .expect("known transaction type");
        txns.push(op);
        // Print under the lock so H lines follow history order.
        emit(&format!(
            "H {}",
            json!({"i": index, "p": process, "type": kind, "t0": t0, "t1": t1,
                   "value": value.iter().map(Mop::to_json).collect::<Vec<_>>(), "error": error})
        ));
    }

    fn client(&self, slot: usize) {
        let mut process = slot;
        let mut rng = Rng::new(self.args.seed * 1000 + slot as u64);
        let mut pg: Option<Pg> = None;
        while Instant::now() < self.stop_at {
            if pg.is_none() {
                match Pg::connect(&self.args.host, self.args.port, self.args.timeout) {
                    Ok(connection) => pg = Some(connection),
                    Err(error) => {
                        evidence(
                            "connect-error",
                            json!({"process": process, "error": error.to_string()}),
                        );
                        kit::pause(1.0);
                        continue;
                    }
                }
            }
            let ops = self.keys.lock().unwrap().transaction();
            let t0 = now_us();
            let connection = pg.as_mut().expect("connected");
            let (kind, value, error, broken) = match execute(connection, &ops, &self.args, &mut rng)
            {
                Ok(Outcome::Ok(value)) => ("ok", Some(value), None, false),
                Ok(Outcome::Fail(code, broken)) => ("fail", None, Some(code), broken),
                Err(error) => ("info", None, Some(error), true),
            };
            self.record(json!(process), kind, &ops, value, t0, now_us(), error);
            if broken {
                pg = None;
                process += self.args.clients;
            }
        }
    }

    fn final_read(&self) -> Option<Vec<Mop>> {
        let keys: Vec<i64> = self.keys.lock().unwrap().touched.iter().copied().collect();
        for attempt in 0..10 {
            let read = (|| -> Result<Vec<Mop>, PgError> {
                let mut pg = Pg::connect(&self.args.host, self.args.port, 30.0)?;
                pg.query("BEGIN ISOLATION LEVEL SERIALIZABLE READ ONLY")?;
                let mut value = Vec::new();
                for k in &keys {
                    let (rows, _) = pg.query(&format!(
                        "SELECT val FROM {} WHERE id = {k}",
                        table(*k, self.args.tables)
                    ))?;
                    value.push(Mop::Read(Atom::Int(*k), Some(parse(&rows))));
                }
                pg.query("COMMIT")?;
                Ok(value)
            })();
            match read {
                Ok(value) => return Some(value),
                Err(error) => {
                    evidence(
                        "final-read-error",
                        json!({"attempt": attempt, "error": error.to_string()}),
                    );
                    kit::pause(1.0);
                }
            }
        }
        None
    }
}

/// Check a history; print the check, anomaly examples and their explanations.
pub fn report(txns: &[TxnOp], model: &str, examples: usize) -> Result<elle::Checked, String> {
    let result = elle::check(txns, model)?;
    evidence(
        "check",
        json!({"valid": result.valid, "model": model, "counts": result.counts, "illegal": result.illegal,
               "txns": result.txns, "edges": result.edges}),
    );
    let by_id = |id: &Value| txns.iter().find(|t| &t.op.id.to_json() == id);
    for (kind, items) in &result.anomalies {
        for item in items.iter().take(examples) {
            if let Some(explanation) = item.get("explanation").and_then(Value::as_str) {
                let described: Vec<Value> = item["cycle"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(by_id)
                    .map(|t| json!({"id": t.op.id.to_json(), "type": elle::kind_of(t), "process": t.op.process.as_ref().map(Atom::to_json),
                                    "invoke": t.invoked_at as f64 / 1e9, "value": elle::h_line(t)["value"]}))
                    .collect();
                evidence(
                    "anomaly",
                    json!({"type": kind, "cycle": item["cycle"], "steps": item["steps"],
                                           "component_size": item["component-size"], "txns": described}),
                );
                emit(&format!("ANOMALY {kind}\n{explanation}"));
            } else {
                let mut fields = item.as_object().cloned().unwrap_or_default();
                fields.insert("type".into(), kind.clone().into());
                evidence("anomaly", Value::Object(fields));
            }
        }
    }
    Ok(result)
}

fn wait_ready(args: &Args) -> Result<Pg, String> {
    let deadline = Instant::now() + Duration::from_secs_f64(args.ready_timeout);
    loop {
        let attempt = Pg::connect(&args.host, args.port, 10.0).and_then(|mut pg| {
            pg.query("SELECT 1")?;
            Ok(pg)
        });
        match attempt {
            Ok(pg) => return Ok(pg),
            Err(error) if Instant::now() > deadline => return Err(error.to_string()),
            Err(_) => kit::pause(0.5),
        }
    }
}

pub fn run(args: Args) -> u8 {
    kit::start_clock();
    let model = args
        .model
        .clone()
        .unwrap_or_else(|| args.isolation.model().into());
    evidence(
        "config",
        json!({"host": args.host, "port": args.port, "isolation": args.isolation.name(), "model_checked": model,
               "clients": args.clients, "duration": args.duration, "tables": args.tables, "key_count": args.key_count,
               "max_writes_per_key": args.max_writes_per_key, "max_txn_length": args.max_txn_length,
               "read_fraction": args.read_fraction, "sk_reads": args.sk_reads, "timeout": args.timeout,
               "seed": args.seed, "label": args.label}),
    );
    let mut pg = match wait_ready(&args) {
        Ok(pg) => pg,
        Err(error) => {
            return verdict(
                kit::INCONCLUSIVE,
                &format!("postgres never became ready: {error}"),
            )
        }
    };
    let setup = (|| -> Result<String, PgError> {
        let (version, _) = pg.query("SHOW server_version")?;
        for i in 0..args.tables {
            pg.query(&format!("DROP TABLE IF EXISTS txn{i}"))?;
            pg.query(&format!(
                "CREATE TABLE txn{i} (id int NOT NULL PRIMARY KEY, sk int NOT NULL, val text)"
            ))?;
        }
        Ok(version
            .first()
            .and_then(|r| r.first())
            .cloned()
            .flatten()
            .unwrap_or_default())
    })();
    drop(pg);
    match setup {
        Ok(version) => evidence("ready", json!({"server_version": version})),
        Err(error) => return verdict(kit::INCONCLUSIVE, &format!("schema setup failed: {error}")),
    }
    let started = Instant::now();
    let run = Arc::new(Run {
        keys: Mutex::new(Keys::new(
            args.seed,
            args.key_count,
            args.max_writes_per_key,
            args.max_txn_length,
            args.read_fraction,
        )),
        txns: Mutex::new(Vec::new()),
        started,
        stop_at: started + Duration::from_secs_f64(args.duration),
        args: args.clone(),
    });
    let threads: Vec<_> = (0..args.clients)
        .map(|slot| {
            let run = run.clone();
            thread::spawn(move || run.client(slot))
        })
        .collect();
    while threads.iter().any(|t| !t.is_finished()) {
        let wake = Instant::now() + Duration::from_secs(10);
        while Instant::now() < wake && threads.iter().any(|t| !t.is_finished()) {
            kit::pause(0.5);
        }
        let txns = run.txns.lock().unwrap();
        let count = |kind: &str| txns.iter().filter(|t| elle::kind_of(t) == kind).count();
        evidence(
            "progress",
            json!({"ok": count("ok"), "fail": count("fail"), "info": count("info")}),
        );
    }
    for thread in threads {
        let _ = thread.join();
    }
    let final_read = run.final_read();
    if let Some(value) = &final_read {
        let t = now_us();
        run.record(json!("final"), "ok", value, Some(value.clone()), t, t, None);
    }
    let txns = run.txns.lock().unwrap().clone();
    let _ = run.started;
    let committed = txns.iter().filter(|t| elle::kind_of(t) == "ok").count();
    let result = match report(&txns, &model, args.examples) {
        Ok(result) => result,
        Err(error) => return verdict(kit::INCONCLUSIVE, &format!("checker: {error}")),
    };
    let counts = json!(result.counts);
    let types = json!(result.txns);
    if result.valid && (committed <= 1 || final_read.is_none()) {
        let reason = if committed <= 1 {
            "no transaction committed"
        } else {
            "the final read failed"
        };
        return verdict(kit::INCONCLUSIVE, &format!("{reason}: {types}"));
    }
    let word = if result.valid { "valid" } else { "INVALID" };
    let text = format!(
        "{word} {} checked as {model}: {counts} txns {types}",
        args.isolation.name()
    );
    verdict(
        if result.valid {
            kit::OK
        } else {
            kit::VIOLATION
        },
        &text,
    )
}

/// Re-check a saved oracle log of `H` lines.
pub fn recheck(text: &str, model: &str) -> Result<(u8, Value), String> {
    let txns = elle::from_h_lines(&kit::tagged_lines(text, "H "))?;
    let result = elle::check(&txns, model)?;
    let code = if result.valid {
        kit::OK
    } else {
        kit::VIOLATION
    };
    Ok((code, result.to_json()))
}

const PROBES: [(&str, &str); 4] = [
    ("activity", "SELECT pid, state, wait_event_type, wait_event, backend_xid, backend_xmin, \
                  extract(epoch FROM now() - xact_start)::float8, extract(epoch FROM now() - state_change)::float8, \
                  pg_blocking_pids(pid), left(query, 70) FROM pg_stat_activity WHERE backend_type = 'client backend' ORDER BY pid"),
    ("waiting", "SELECT pid, locktype, mode, relation::regclass, page, tuple, transactionid, virtualtransaction \
                 FROM pg_locks WHERE NOT granted ORDER BY pid"),
    ("holders", "SELECT l.pid, l.locktype, l.mode, l.transactionid, l.relation::regclass, a.state \
                 FROM pg_locks l LEFT JOIN pg_stat_activity a USING (pid) WHERE l.granted AND l.locktype IN ('transactionid','tuple') ORDER BY l.pid"),
    ("settings", "SELECT name, setting FROM pg_settings WHERE name IN ('deadlock_timeout','max_connections','lock_timeout')"),
];

/// A diagnostic machine: idle until each `at` second, then report what pg's
/// backends wait on as one `PROBE {json}` line.
pub fn probe(host: &str, at: &[f64]) -> u8 {
    kit::start_clock();
    let start = Instant::now();
    let times = if at.is_empty() {
        vec![14.0, 18.0]
    } else {
        at.to_vec()
    };
    for at in times {
        kit::pause(at - start.elapsed().as_secs_f64());
        let report = (|| -> Result<Value, PgError> {
            let mut pg = Pg::connect(host, 5432, 5.0)?;
            let mut out = json!({"t": (start.elapsed().as_secs_f64() * 1000.0).round() / 1000.0});
            for (name, sql) in PROBES {
                out[name] = json!(pg.query(sql)?.0);
            }
            Ok(out)
        })();
        match report {
            Ok(report) => emit(&format!("PROBE {report}")),
            Err(error) => emit(&format!("PROBE-ERROR {error}")),
        }
    }
    loop {
        std::thread::sleep(Duration::from_secs(3600));
    }
}
