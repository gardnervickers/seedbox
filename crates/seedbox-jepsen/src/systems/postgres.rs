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
//! [`crate::checker::elle`] checks the history. A clean history counts as
//! valid only when the clients committed at least `--min-commit-rate`
//! transactions per client-second: a server that stalls (as in
//! `harness/jepsen/postgres/stall/`) passes the check vacuously, so such a run
//! is inconclusive instead.
//!
//! Durability and faults: every acknowledged append must be in the final read
//! (`lost`), and no committed transaction may miss a value whose append was
//! acknowledged before it began (`stale`, [`crate::checker::realtime`]); a
//! single server is strictly serializable at SERIALIZABLE and gives strong
//! snapshot isolation at REPEATABLE READ. `--crash-every` power-fails the
//! server repeatedly during the workload (page cache and unflushed sectors
//! are lost); generated fault profiles act from outside. After the workload
//! the server must answer again within `--recover` seconds, or the run fails.
//! With `--standby`, the server streams to a standby; at `--failover-at` the
//! primary is power-failed and left down (fenced), the standby is promoted,
//! and every client moves to it: every acknowledged commit must survive the
//! promotion and no read on the promoted standby may be stale. A value that
//! is not an integer (a corrupted row) reads as -1, which no one appended,
//! so the checker reports it as garbage.
use super::pg_proto::{Pg, PgError};
use crate::checker::elle::{self, Atom, Mop, TxnOp};
use crate::kit::{self, emit, evidence, now_us, verdict, Rng};
use crate::nemesis;
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
    /// Commits per client-second below which a valid history is inconclusive;
    /// 0 disables the check. Healthy runs commit 0.86-4 per client-second and
    /// stalled ones 0.035-0.11.
    #[arg(long, default_value_t = MIN_COMMIT_RATE, value_parser = non_negative)]
    pub min_commit_rate: f64,
    /// Power-fail the server about this often during the workload (half to
    /// one and a half times it, from the guest's seeded entropy), in seconds;
    /// 0 never does.
    #[arg(long, default_value_t = 0.0, value_parser = non_negative)]
    pub crash_every: f64,
    /// Seconds a power-failed server stays down before it boots again.
    #[arg(long, default_value_t = 2.0, value_parser = non_negative)]
    pub crash_down: f64,
    /// Seconds after the workload within which the server (the promoted
    /// standby after a failover) must answer again; one that never does
    /// fails the run.
    #[arg(long, default_value_t = 60.0, value_parser = non_negative)]
    pub recover: f64,
    /// A streaming standby of --host, to fail over to.
    #[arg(long)]
    pub standby: Option<String>,
    /// Seconds into the workload at which to power-fail the primary for good
    /// and promote the standby; 0 never fails over.
    #[arg(long, default_value_t = 0.0, value_parser = non_negative)]
    pub failover_at: f64,
    /// With --standby: delay the primary's traffic to the standby by this
    /// many milliseconds, so that streaming lags the acknowledgements the
    /// clients see (with equal latencies an asynchronous standby receives a
    /// commit as soon as its client does); 0 adds nothing.
    #[arg(long, default_value_t = 0)]
    pub replication_delay_ms: u64,
    /// With --standby: the standby must be a synchronous one before the
    /// workload starts (`sync_state = 'sync'` on the primary).
    #[arg(long)]
    pub expect_sync: bool,
    /// Generator seed; Seedbox's seed varies the schedule.
    #[arg(long, default_value_t = 1)]
    pub seed: u64,
    /// Anomaly examples printed per type.
    #[arg(long, default_value_t = 5)]
    pub examples: usize,
    #[arg(long, default_value = "")]
    pub label: String,
}

/// The default `--min-commit-rate`. Across 543 recorded healthy Seedbox
/// oracle logs (runs and replays, 15 s and 60 s, 20 clients) the lowest rate
/// was 0.86 commits per client-second, with a probe machine competing for the
/// database; the 16 logs of startup stalls committed 0.035-0.11. This sits
/// well clear of both.
pub const MIN_COMMIT_RATE: f64 = 0.25;

fn non_negative(text: &str) -> Result<f64, String> {
    match text.parse::<f64>() {
        Ok(value) if value >= 0.0 && value.is_finite() => Ok(value),
        _ => Err(format!("{text}: expected a non-negative number")),
    }
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

/// What an element that is not an integer reads as: a value no one appends.
pub const GARBAGE: i64 = -1;

/// A comma-separated list value; a missing row or NULL reads as [], and an
/// element that is not an integer as [`GARBAGE`]. More than one row for a
/// key (a broken primary key) concatenates them.
pub fn parse(rows: &[Vec<Option<String>>]) -> Vec<i64> {
    let mut out = Vec::new();
    for row in rows {
        match row.first().cloned().flatten() {
            Some(text) if !text.is_empty() => {
                out.extend(text.split(',').map(|x| x.parse().unwrap_or(GARBAGE)))
            }
            _ => {}
        }
    }
    out
}

/// [`parse`], recording any value that is not a clean list as evidence.
fn read_value(rows: &[Vec<Option<String>>], k: i64) -> Vec<i64> {
    let value = parse(rows);
    if rows.len() > 1 || value.contains(&GARBAGE) {
        evidence(
            "unparseable",
            json!({"key": k, "rows": rows.iter().map(|r| r.first().cloned().flatten()).collect::<Vec<_>>()}),
        );
    }
    value
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
                    value.push(Mop::Read(Atom::Int(*k), Some(read_value(&rows, *k))));
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
    /// The server clients connect to: --host, then the promoted standby.
    host: Mutex<String>,
    keys: Mutex<Keys>,
    txns: Mutex<Vec<TxnOp>>,
    started: Instant,
    stop_at: Instant,
}

impl Run {
    fn host(&self) -> String {
        self.host.lock().unwrap().clone()
    }

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
                match Pg::connect(&self.host(), self.args.port, self.args.timeout) {
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
                let mut pg = Pg::connect(&self.host(), self.args.port, 30.0)?;
                pg.query("BEGIN ISOLATION LEVEL SERIALIZABLE READ ONLY")?;
                let mut value = Vec::new();
                for k in &keys {
                    let (rows, _) = pg.query(&format!(
                        "SELECT val FROM {} WHERE id = {k}",
                        table(*k, self.args.tables)
                    ))?;
                    value.push(Mop::Read(Atom::Int(*k), Some(read_value(&rows, *k))));
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

/// Connect to `host` and run `SELECT 1`, retrying until `seconds` pass.
fn serving(host: &str, port: u16, seconds: f64) -> Result<Pg, String> {
    let deadline = Instant::now() + Duration::from_secs_f64(seconds);
    loop {
        let attempt = Pg::connect(host, port, 10.0).and_then(|mut pg| {
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

/// The first column of the first row, if any.
fn scalar(rows: &[Vec<Option<String>>]) -> String {
    rows.first()
        .and_then(|r| r.first())
        .cloned()
        .flatten()
        .unwrap_or_default()
}

/// Wait until the primary reports a streaming standby (a synchronous one
/// with `sync`), and return its `pg_stat_replication` rows.
fn wait_streaming(args: &Args, sync: bool) -> Result<Value, String> {
    let deadline = Instant::now() + Duration::from_secs_f64(args.ready_timeout);
    loop {
        let rows = Pg::connect(&args.host, args.port, 10.0).and_then(|mut pg| {
            Ok(pg
                .query("SELECT application_name, state, sync_state FROM pg_stat_replication")?
                .0)
        });
        let last = match rows {
            Ok(rows) => {
                // pg_basebackup's own WAL stream is not the standby yet.
                let ready = rows.iter().any(|r| {
                    r.first().cloned().flatten().as_deref() != Some("pg_basebackup")
                        && r.get(1).cloned().flatten().as_deref() == Some("streaming")
                        && (!sync || r.get(2).cloned().flatten().as_deref() == Some("sync"))
                });
                if ready {
                    return Ok(json!(rows));
                }
                json!(rows).to_string()
            }
            Err(error) => error.to_string(),
        };
        if Instant::now() > deadline {
            return Err(format!("no streaming standby: {last}"));
        }
        kit::pause(0.5);
    }
}

/// Power-fail the server about every `--crash-every` seconds until shortly
/// before the workload ends, rebooting it after `--crash-down` seconds.
fn crash_repeatedly(args: Args, started: Instant) -> Option<thread::JoinHandle<()>> {
    let mut ctl = match kit::connect_world(None) {
        Ok(ctl) => ctl,
        Err(error) => {
            evidence("nemesis-unavailable", json!({"error": error.0}));
            return None;
        }
    };
    Some(thread::spawn(move || {
        let mut pace = nemesis::Sleep;
        let mut rng = Rng::new(kit::rng::entropy_seed());
        let host = args.host.as_str();
        let mut crashes = 0;
        loop {
            let gap = rng.uniform(0.5, 1.5) * args.crash_every;
            if started.elapsed().as_secs_f64() + gap + args.crash_down + 3.0 > args.duration {
                break;
            }
            nemesis::Pace::wait(&mut pace, &mut ctl, gap);
            crashes += 1;
            if let Err(error) = nemesis::crash_all(
                &mut ctl,
                &mut pace,
                &[host],
                args.crash_down,
                json!({"n": crashes}),
            ) {
                evidence("nemesis-failed", json!({"error": error.0}));
            }
        }
        evidence("nemesis-done", json!({"crashes": crashes}));
    }))
}

/// At `--failover-at`, power-fail the primary for good (it is never
/// rebooted, so it cannot come back as a second primary), promote the
/// standby and point every client at it.
fn fail_over(run: Arc<Run>, standby: String) -> Option<thread::JoinHandle<()>> {
    let mut ctl = match kit::connect_world(None) {
        Ok(ctl) => ctl,
        Err(error) => {
            evidence("nemesis-unavailable", json!({"error": error.0}));
            return None;
        }
    };
    Some(thread::spawn(move || {
        let args = &run.args;
        let mut pace = nemesis::Sleep;
        nemesis::Pace::wait(
            &mut pace,
            &mut ctl,
            args.failover_at - run.started.elapsed().as_secs_f64(),
        );
        if let Err(error) = nemesis::step(&mut ctl, "crash", &[args.host.as_str()]) {
            evidence(
                "failover-failed",
                json!({"step": "crash", "error": error.0}),
            );
            return;
        }
        let deadline = Instant::now() + Duration::from_secs_f64(args.ready_timeout);
        loop {
            let promoted = Pg::connect(&standby, args.port, 10.0).and_then(|mut pg| {
                let (rows, _) = pg.query("SELECT pg_promote(true, 60)")?;
                Ok(scalar(&rows))
            });
            match promoted {
                Ok(result) if result == "t" => break,
                Ok(result) => evidence("promote-retry", json!({"result": result})),
                // Already promoted (an earlier attempt's reply was lost).
                Err(PgError::Server { code, message }) if code == "55000" => {
                    evidence("promote-retry", json!({"code": code, "message": message}));
                    break;
                }
                Err(error) => evidence("promote-retry", json!({"error": error.to_string()})),
            }
            if Instant::now() > deadline {
                evidence("failover-failed", json!({"step": "promote"}));
                return;
            }
            kit::pause(1.0);
        }
        *run.host.lock().unwrap() = standby.clone();
        evidence("promoted", json!({"host": standby}));
    }))
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
               "min_commit_rate": args.min_commit_rate, "crash_every": args.crash_every,
               "crash_down": args.crash_down, "recover": args.recover, "standby": args.standby,
               "failover_at": args.failover_at, "expect_sync": args.expect_sync,
               "replication_delay_ms": args.replication_delay_ms,
               "seed": args.seed, "label": args.label}),
    );
    let mut pg = match serving(&args.host, args.port, args.ready_timeout) {
        Ok(pg) => pg,
        Err(error) => {
            return verdict(
                kit::INCONCLUSIVE,
                &format!("postgres never became ready: {error}"),
            )
        }
    };
    let setup = (|| -> Result<Value, PgError> {
        let mut settings = serde_json::Map::new();
        for name in [
            "server_version",
            "data_checksums",
            "fsync",
            "synchronous_commit",
            "full_page_writes",
            "wal_sync_method",
            "synchronous_standby_names",
        ] {
            let (rows, _) = pg.query(&format!("SHOW {name}"))?;
            settings.insert(name.into(), scalar(&rows).into());
        }
        for i in 0..args.tables {
            pg.query(&format!("DROP TABLE IF EXISTS txn{i}"))?;
            pg.query(&format!(
                "CREATE TABLE txn{i} (id int NOT NULL PRIMARY KEY, sk int NOT NULL, val text)"
            ))?;
        }
        Ok(Value::Object(settings))
    })();
    drop(pg);
    match setup {
        Ok(settings) => evidence("ready", settings),
        Err(error) => return verdict(kit::INCONCLUSIVE, &format!("schema setup failed: {error}")),
    }
    if let (Some(standby), true) = (&args.standby, args.replication_delay_ms > 0) {
        let shaped = kit::connect_world(None).and_then(|mut ctl| {
            let world = ctl.world().ok_or("native mode has no shaping")?;
            world
                .shape(
                    [args.host.as_str()],
                    [standby.as_str()],
                    args.replication_delay_ms * 1_000_000,
                    seedbox_control_client::ShapeOptions::default(),
                )
                .map_err(kit::Error::from)?;
            Ok(())
        });
        match shaped {
            Ok(()) => evidence(
                "replication-delay",
                json!({"from": args.host, "to": standby, "ms": args.replication_delay_ms}),
            ),
            Err(error) => {
                return verdict(
                    kit::INCONCLUSIVE,
                    &format!("cannot delay replication: {error}"),
                )
            }
        }
    }
    if args.standby.is_some() {
        match wait_streaming(&args, args.expect_sync) {
            Ok(rows) => evidence("replication", json!({"standbys": rows})),
            Err(error) => {
                return verdict(
                    kit::INCONCLUSIVE,
                    &format!("replication never started: {error}"),
                )
            }
        }
    }
    seedbox_sdk::setup_complete(json!({"host": args.host, "standby": args.standby}));
    let started = Instant::now();
    let run = Arc::new(Run {
        host: Mutex::new(args.host.clone()),
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
    let mut nemeses = Vec::new();
    if args.crash_every > 0.0 {
        nemeses.extend(crash_repeatedly(args.clone(), started));
    }
    if let (Some(standby), true) = (&args.standby, args.failover_at > 0.0) {
        nemeses.extend(fail_over(run.clone(), standby.clone()));
    }
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
    for thread in threads.into_iter().chain(nemeses) {
        let _ = thread.join();
    }
    let host = run.host();
    let waited = Instant::now();
    if let Err(error) = serving(&host, args.port, args.recover) {
        evidence(
            "unrecovered",
            json!({"host": host, "waited": waited.elapsed().as_secs_f64(), "last": error}),
        );
        return verdict(
            kit::VIOLATION,
            &format!(
                "unrecovered: {host} did not answer within {} s after the workload: {error}",
                args.recover
            ),
        );
    }
    evidence(
        "recovered",
        json!({"host": host, "after": waited.elapsed().as_secs_f64()}),
    );
    let final_read = run.final_read();
    if let Some(value) = &final_read {
        let t = now_us();
        run.record(json!("final"), "ok", value, Some(value.clone()), t, t, None);
    }
    let txns = run.txns.lock().unwrap().clone();
    errors(&txns);
    let result = match report(&txns, &model, args.examples) {
        Ok(result) => result,
        Err(error) => return verdict(kit::INCONCLUSIVE, &format!("checker: {error}")),
    };
    let durability = durability(&txns, final_read.as_deref(), args.examples);
    let progress = progress(&txns, args.clients, args.duration);
    evidence(
        "throughput",
        json!({"commits": progress.done, "clients": progress.clients, "seconds": progress.seconds,
               "per_client_second": progress.rate(), "minimum": args.min_commit_rate}),
    );
    let (code, text) = decide(
        &result,
        durability,
        progress,
        args.isolation.name(),
        args.min_commit_rate,
    );
    verdict(code, &text)
}

/// Why transactions failed or were indeterminate, by error, as evidence.
fn errors(txns: &[TxnOp]) {
    let mut fail = std::collections::BTreeMap::<String, usize>::new();
    let mut info = std::collections::BTreeMap::<String, usize>::new();
    for t in txns {
        let error = elle::h_line(t)["error"]
            .as_str()
            .unwrap_or("")
            .chars()
            .take(80)
            .collect::<String>();
        match elle::kind_of(t) {
            "fail" => *fail.entry(error).or_default() += 1,
            "info" => *info.entry(error).or_default() += 1,
            _ => {}
        }
    }
    evidence("errors", json!({"fail": fail, "info": info}));
}

/// Lost and stale counts beside the isolation check; `lost` is `None`
/// without a final read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Durability {
    pub lost: Option<usize>,
    pub stale: usize,
}

/// Acknowledged appends missing from the final read (`lost`), and committed
/// reads that miss an append acknowledged before they began (`stale`, not
/// counting lost values), each with evidence.
fn durability(txns: &[TxnOp], final_read: Option<&[Mop]>, examples: usize) -> Durability {
    let finals = [("final".to_string(), final_read.map(<[Mop]>::to_vec))];
    let (stale, lost) = super::galera::durability(txns, &finals);
    let lost = lost.into_iter().next().and_then(|(_, l)| l);
    evidence(
        "durability",
        json!({"lost": lost.as_ref().map(Vec::len),
               "examples": lost.iter().flatten().take(examples).collect::<Vec<_>>()}),
    );
    evidence(
        "stale-reads",
        json!({"count": stale.len(), "examples": stale.iter().take(examples).collect::<Vec<_>>()}),
    );
    Durability {
        lost: lost.map(|l| l.len()),
        stale: stale.len(),
    }
}

/// The clients' commits (the final read is not one) over the run's window.
fn progress(txns: &[TxnOp], clients: usize, seconds: f64) -> kit::Progress {
    let done = txns
        .iter()
        .filter(|t| elle::kind_of(t) == "ok" && t.op.process != Some(Atom::from("final")))
        .count();
    kit::Progress {
        done,
        clients,
        seconds,
    }
}

/// The verdict for a checked history. An anomaly, a lost acknowledged append
/// or a stale read is a violation however little the clients did; a clean
/// history is valid only when they committed something, reached `min_rate`
/// commits per client-second and the final read succeeded, and is otherwise
/// inconclusive.
pub fn decide(
    result: &elle::Checked,
    durability: Durability,
    progress: kit::Progress,
    isolation: &str,
    min_rate: f64,
) -> (u8, String) {
    let model = &result.model;
    let counts = json!(result.counts);
    let types = json!(result.txns);
    let lost = durability.lost.unwrap_or(0);
    let tail = format!("lost {lost} stale {}", durability.stale);
    if !result.valid || lost > 0 || durability.stale > 0 {
        let word = if result.valid { "valid" } else { "INVALID" };
        let prefix = if lost > 0 || durability.stale > 0 {
            "NOT DURABLE: "
        } else {
            ""
        };
        return (
            kit::VIOLATION,
            format!("{prefix}{word} {isolation} checked as {model}: {counts} txns {types} {tail}"),
        );
    }
    let reason = if progress.done == 0 {
        Some("no transaction committed".to_string())
    } else if let Some(shortfall) = progress.shortfall(min_rate, "commits") {
        Some(format!(
            "{shortfall}; history valid {isolation} checked as {model}"
        ))
    } else if durability.lost.is_none() {
        Some("the final read failed".to_string())
    } else {
        None
    };
    match reason {
        Some(reason) => (kit::INCONCLUSIVE, format!("{reason}: {types}")),
        None => (
            kit::OK,
            format!("valid {isolation} checked as {model}: {counts} txns {types} {tail}"),
        ),
    }
}

/// What the last committed final read returned, if any.
fn completed_final(txns: &[TxnOp]) -> Option<Vec<Mop>> {
    txns.iter()
        .rev()
        .filter(|t| t.op.process == Some(Atom::from("final")))
        .find_map(|t| match &t.outcome {
            wgl::OpOutcome::Completed { ret, .. } => Some(ret.clone()),
            _ => None,
        })
}

/// Re-check a saved oracle log of `H` lines, with the oracle's verdict rules
/// (the clients and duration come from its config evidence).
pub fn recheck(text: &str, model: &str) -> Result<(u8, Value), String> {
    let txns = elle::from_h_lines(&kit::tagged_lines(text, "H "))?;
    let result = elle::check(&txns, model)?;
    let config = kit::evidence_of(text, "config").unwrap_or_default();
    let progress = progress(
        &txns,
        config["clients"].as_u64().unwrap_or(0) as usize,
        config["duration"].as_f64().unwrap_or(0.0),
    );
    let final_read = completed_final(&txns);
    let finals = [("final".to_string(), final_read)];
    let (stale, lost) = super::galera::durability(&txns, &finals);
    let lost = lost.into_iter().next().and_then(|(_, l)| l);
    let durability = Durability {
        lost: lost.as_ref().map(Vec::len),
        stale: stale.len(),
    };
    let isolation = config["isolation"].as_str().unwrap_or("serializable");
    let (code, verdict) = decide(&result, durability, progress, isolation, MIN_COMMIT_RATE);
    let mut out = result.to_json();
    out["verdict"] = verdict.into();
    out["lost"] = json!(lost);
    out["stale"] = json!(stale);
    out["throughput"] = json!({"commits": progress.done, "clients": progress.clients,
                               "seconds": progress.seconds, "per_client_second": progress.rate()});
    Ok((code, out))
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

#[cfg(test)]
mod tests {
    use super::*;

    fn append(k: i64, v: i64) -> Mop {
        Mop::Append(Atom::Int(k), v)
    }
    fn read(k: i64, value: &[i64]) -> Mop {
        Mop::Read(Atom::Int(k), Some(value.to_vec()))
    }

    /// `commits` serial single-append transactions on key 0, `infos` lost
    /// ones, then (if `final_read`) the final read of key 0.
    fn history(commits: i64, infos: i64, final_read: bool) -> Vec<TxnOp> {
        let mut txns = Vec::new();
        let mut at = 0;
        let mut push = |kind: &str, value: Vec<Mop>, process: Atom| {
            let i = txns.len() as i64;
            txns.push(elle::txn(i, kind, value, Some(process), at, at + 1, None).unwrap());
            at += 2;
        };
        for v in 1..=commits {
            push("ok", vec![append(0, v)], Atom::Int(v % 20));
        }
        for v in 0..infos {
            push("info", vec![append(1, v + 1)], Atom::Int(100 + v));
        }
        if final_read {
            let all: Vec<i64> = (1..=commits).collect();
            push("ok", vec![read(0, &all)], Atom::from("final"));
        }
        txns
    }

    /// Two transactions that each miss the other's append: G2-item.
    fn write_skew() -> Vec<TxnOp> {
        vec![
            elle::txn(
                0,
                "ok",
                vec![read(0, &[]), append(1, 1)],
                Some(Atom::Int(0)),
                0,
                10,
                None,
            )
            .unwrap(),
            elle::txn(
                1,
                "ok",
                vec![read(1, &[]), append(0, 1)],
                Some(Atom::Int(1)),
                1,
                11,
                None,
            )
            .unwrap(),
            elle::txn(
                2,
                "ok",
                vec![read(0, &[1]), read(1, &[1])],
                Some(Atom::from("final")),
                20,
                20,
                None,
            )
            .unwrap(),
        ]
    }

    fn decide_with(
        txns: &[TxnOp],
        clients: usize,
        seconds: f64,
        final_read: bool,
        min: f64,
    ) -> (u8, String) {
        let result = elle::check(txns, "serializable").unwrap();
        let finals = if final_read {
            completed_final(txns)
        } else {
            None
        };
        decide(
            &result,
            durability(txns, finals.as_deref(), 1),
            progress(txns, clients, seconds),
            "serializable",
            min,
        )
    }

    fn decide_on(txns: &[TxnOp], clients: usize, seconds: f64, final_read: bool) -> (u8, String) {
        decide_with(txns, clients, seconds, final_read, MIN_COMMIT_RATE)
    }

    #[test]
    fn a_stalled_clean_run_is_inconclusive_and_reports_its_counts() {
        // pg124-serializable-15s seed 3 on seedbox-m10: 21 client commits and
        // 40 indeterminate transactions in 15 s, valid as a history.
        let txns = history(21, 40, true);
        assert_eq!(
            progress(&txns, 20, 15.0).done,
            21,
            "the final read is no commit"
        );
        let (code, text) = decide_on(&txns, 20, 15.0, true);
        assert_eq!(code, kit::INCONCLUSIVE);
        assert_eq!(
            text,
            "insufficient progress: 21 commits by 20 clients in 15 s (0.070 per client-second, \
             minimum 0.25); history valid serializable checked as serializable: \
             {\"info\":40,\"ok\":22}"
        );
    }

    #[test]
    fn healthy_clean_runs_stay_valid() {
        // The slowest recorded healthy run: 258 commits in 15 s with a probe
        // competing; 60 s runs commit thousands.
        for (commits, seconds) in [(258, 15.0), (2320, 60.0)] {
            let (code, text) = decide_on(&history(commits, 0, true), 20, seconds, true);
            assert_eq!(code, kit::OK, "{text}");
            assert!(text.starts_with("valid serializable checked as serializable: {} txns"));
        }
        // Disabling the check keeps even a stalled run valid.
        let (code, _) = decide_with(&history(21, 40, true), 20, 15.0, true, 0.0);
        assert_eq!(code, kit::OK);
    }

    #[test]
    fn an_anomaly_is_a_violation_however_slow_the_run() {
        let (code, text) = decide_on(&write_skew(), 20, 15.0, true);
        assert_eq!(code, kit::VIOLATION);
        assert!(
            text.starts_with("INVALID serializable checked as serializable: {\"G2-item\":1}"),
            "{text}"
        );
    }

    #[test]
    fn no_commits_or_no_final_read_is_inconclusive() {
        let (code, text) = decide_on(&history(0, 5, true), 20, 15.0, true);
        assert_eq!(
            (code, text.as_str()),
            (
                kit::INCONCLUSIVE,
                "no transaction committed: {\"info\":5,\"ok\":1}"
            )
        );
        let (code, text) = decide_on(&history(300, 0, false), 20, 15.0, false);
        assert_eq!(code, kit::INCONCLUSIVE);
        assert!(text.starts_with("the final read failed"), "{text}");
    }

    fn log(config: Value, txns: &[TxnOp]) -> String {
        let mut text = format!("EVIDENCE {config}\n");
        for t in txns {
            text += &format!("H {}\n", elle::h_line(t));
        }
        text
    }

    #[test]
    fn recheck_applies_the_progress_rule_from_the_logged_config() {
        let config =
            json!({"kind": "config", "clients": 20, "duration": 15.0, "isolation": "serializable"});
        let (code, out) =
            recheck(&log(config.clone(), &history(21, 40, true)), "serializable").unwrap();
        assert_eq!(code, kit::INCONCLUSIVE);
        assert_eq!(out["valid"], true);
        assert_eq!(out["throughput"]["commits"], 21);
        assert!(out["verdict"]
            .as_str()
            .unwrap()
            .starts_with("insufficient progress: 21 commits"));
        let (code, out) =
            recheck(&log(config.clone(), &history(400, 0, true)), "serializable").unwrap();
        assert_eq!(
            (code, out["throughput"]["commits"].as_u64()),
            (kit::OK, Some(400))
        );
        let (code, _) = recheck(&log(config, &write_skew()), "serializable").unwrap();
        assert_eq!(code, kit::VIOLATION);
    }

    #[test]
    fn a_lost_acknowledged_append_is_not_durable() {
        // 300 commits on key 0; the final read (after a crash) lacks the last.
        let mut txns = history(300, 0, false);
        let all: Vec<i64> = (1..300).collect();
        let at = 10_000;
        txns.push(
            elle::txn(
                300,
                "ok",
                vec![read(0, &all)],
                Some(Atom::from("final")),
                at,
                at,
                None,
            )
            .unwrap(),
        );
        let (code, text) = decide_on(&txns, 20, 15.0, true);
        assert_eq!(code, kit::VIOLATION);
        assert!(
            text.starts_with("NOT DURABLE: valid serializable checked as serializable"),
            "{text}"
        );
        assert!(text.ends_with("lost 1 stale 0"), "{text}");
    }

    #[test]
    fn a_read_missing_an_earlier_acknowledged_append_is_stale() {
        let txns = vec![
            elle::txn(0, "ok", vec![append(0, 1)], Some(Atom::Int(0)), 0, 10, None).unwrap(),
            // Begins after the append was acknowledged, yet misses it.
            elle::txn(
                1,
                "ok",
                vec![read(0, &[])],
                Some(Atom::Int(1)),
                20,
                30,
                None,
            )
            .unwrap(),
            elle::txn(
                2,
                "ok",
                vec![read(0, &[1])],
                Some(Atom::from("final")),
                40,
                40,
                None,
            )
            .unwrap(),
        ];
        let (code, text) = decide_with(&txns, 1, 1.0, true, 0.0);
        assert_eq!(code, kit::VIOLATION);
        assert!(text.ends_with("lost 0 stale 1"), "{text}");
    }

    #[test]
    fn values_that_are_not_clean_lists_read_as_garbage() {
        let row = |v: Option<&str>| vec![v.map(str::to_owned)];
        assert_eq!(parse(&[row(Some("1,2,3"))]), vec![1, 2, 3]);
        assert_eq!(parse(&[row(Some("1,2\u{0}x,3"))]), vec![1, GARBAGE, 3]);
        assert_eq!(parse(&[row(None)]), Vec::<i64>::new());
        assert_eq!(parse(&[]), Vec::<i64>::new());
        // Two rows for one key (a broken primary key) are both read.
        assert_eq!(parse(&[row(Some("1")), row(Some("2"))]), vec![1, 2]);
        // The checker calls a value nobody appended garbage.
        let txns = vec![
            elle::txn(0, "ok", vec![append(0, 1)], Some(Atom::Int(0)), 0, 10, None).unwrap(),
            elle::txn(
                1,
                "ok",
                vec![read(0, &[1, GARBAGE])],
                Some(Atom::from("final")),
                20,
                20,
                None,
            )
            .unwrap(),
        ];
        assert!(elle::check(&txns, "serializable")
            .unwrap()
            .counts
            .contains_key("garbage"));
    }

    #[test]
    fn min_commit_rate_must_be_non_negative() {
        assert_eq!(non_negative("0.25"), Ok(0.25));
        assert_eq!(non_negative("0"), Ok(0.0));
        assert!(non_negative("-1").is_err());
        assert!(non_negative("nan").is_err());
        assert!(non_negative("x").is_err());
    }
}
