//! Jepsen's MySQL workloads (jepsen-io/mysql, the MySQL 8.0.34 analysis) as a
//! Seedbox oracle, speaking the MySQL protocol through the `mysql` crate.
//!
//! * `append`: Elle list-append over `--tables` tables `txnN (id int primary
//!   key, sk int, val text)`, `val` a comma-separated list. As in Jepsen, an
//!   append is `UPDATE ... SET val = CONCAT(val, ',', v)`, falling back to an
//!   `INSERT` under a savepoint when no row matched and to the update again
//!   when that insert raced another (`--on-duplicate` uses `INSERT ... ON
//!   DUPLICATE KEY UPDATE` instead). A one-operation transaction runs in
//!   autocommit mode; longer ones in `START TRANSACTION ... COMMIT`. Keys follow
//!   Elle's generator ([`super::postgres::Keys`]). A final read checks that
//!   every acknowledged append is still there (`lost`), which matters after a
//!   `--crash-at` power failure.
//! * `nrr`: Jepsen's non-repeatable-read test on `people (id, name, gender)`:
//!   name changes, deletes and re-inserts of row 0, and transactions that read
//!   the name, update the gender and read the name again
//!   ([`crate::checker::views::repeatable`]).
//! * `mav`: Jepsen's monotonic-atomic-view test on two `mav` rows: writers
//!   increment both rows' `value` in random order; readers read row 0, set
//!   row 1's `noop`, then read rows 1 and 0 ([`crate::checker::views::monotonic`]).
//!
//! Every statement waits a random 0 to `--mop-sleep-ms` ms first, as Jepsen's
//! clients do. A server error is a definite abort (`fail`), after which the
//! client rolls back; a timeout or lost connection is indeterminate (`info`)
//! and the client continues as a new process. Nothing is retried. With
//! `--replica` hosts, Jepsen's read-only generator applies: processes whose
//! number is not a multiple of the host count run on a replica, with every
//! append turned into a read. The oracle sets up GTID replication from
//! `--host` before creating the schema.
use super::postgres::{report, Keys};
use crate::checker::elle::{self, Atom, Mop, TxnOp};
use crate::checker::views;
use crate::kit::{self, emit, evidence, now_us, verdict, Rng};
use crate::nemesis;
use mysql::prelude::Queryable;
use mysql::{Conn, OptsBuilder};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
};

#[derive(clap::Args, Debug, Clone)]
pub struct Args {
    #[arg(long, value_enum, default_value = "append")]
    pub workload: Workload,
    /// The primary: every write goes here.
    #[arg(long, default_value = "mysql")]
    pub host: String,
    /// Read-only replicas of --host; repeatable.
    #[arg(long)]
    pub replica: Vec<String>,
    #[arg(long, default_value_t = 3306)]
    pub port: u16,
    #[arg(long, value_enum, default_value = "repeatable-read")]
    pub isolation: Isolation,
    /// Consistency model the append history is checked against [default: from --isolation].
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
    /// Append with INSERT ... ON DUPLICATE KEY UPDATE.
    #[arg(long)]
    pub on_duplicate: bool,
    /// Each statement first waits a random 0 to this many milliseconds.
    #[arg(long, default_value_t = 10)]
    pub mop_sleep_ms: u64,
    /// Seconds before a statement counts as indeterminate.
    #[arg(long, default_value_t = 10.0)]
    pub timeout: f64,
    #[arg(long, default_value_t = 600.0)]
    pub ready_timeout: f64,
    /// Power-fail --host at these seconds after the workload starts; repeatable.
    #[arg(long)]
    pub crash_at: Vec<f64>,
    /// Seconds a crashed host stays down before its reboot.
    #[arg(long, default_value_t = 2.0)]
    pub crash_down: f64,
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
pub enum Workload {
    /// Elle list-append.
    Append,
    /// Jepsen's non-repeatable-read test (people table).
    Nrr,
    /// Jepsen's monotonic-atomic-view test (two mav rows).
    Mav,
}

impl Workload {
    fn name(self) -> &'static str {
        match self {
            Self::Append => "append",
            Self::Nrr => "nrr",
            Self::Mav => "mav",
        }
    }
}

#[derive(clap::ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum Isolation {
    /// SERIALIZABLE, checked as serializable.
    Serializable,
    /// REPEATABLE READ (the default), checked as Adya's repeatable read, as Jepsen did.
    RepeatableRead,
    /// READ COMMITTED, checked as read committed.
    ReadCommitted,
    /// READ UNCOMMITTED, checked as read committed.
    ReadUncommitted,
}

impl Isolation {
    pub(crate) fn sql(self) -> &'static str {
        match self {
            Self::Serializable => "SERIALIZABLE",
            Self::RepeatableRead => "REPEATABLE READ",
            Self::ReadCommitted => "READ COMMITTED",
            Self::ReadUncommitted => "READ UNCOMMITTED",
        }
    }
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Serializable => "serializable",
            Self::RepeatableRead => "repeatable-read",
            Self::ReadCommitted => "read-committed",
            Self::ReadUncommitted => "read-uncommitted",
        }
    }
    fn model(self) -> &'static str {
        match self {
            Self::Serializable => "serializable",
            Self::RepeatableRead => "repeatable-read",
            Self::ReadCommitted | Self::ReadUncommitted => "read-committed",
        }
    }
}

// ------------------------------------------------------------ connections

#[derive(Debug, Clone)]
pub(crate) enum SqlError {
    /// The server rejected the statement (its error code).
    Server(u16, String),
    /// The homebrew upsert's insert and both updates all missed.
    Upsert,
    /// A timeout or a lost connection: the outcome is unknown.
    Indeterminate(String),
}

impl From<mysql::Error> for SqlError {
    fn from(error: mysql::Error) -> Self {
        match error {
            mysql::Error::MySqlError(e) => Self::Server(e.code, e.message),
            other => Self::Indeterminate(other.to_string()),
        }
    }
}

impl std::fmt::Display for SqlError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Server(code, message) => write!(f, "{code}: {message}"),
            Self::Upsert => f.write_str("upsert-failed"),
            Self::Indeterminate(message) => f.write_str(message),
        }
    }
}

pub(crate) type Sql<T> = std::result::Result<T, SqlError>;

/// MySQL's error code for a duplicate key.
const DUPLICATE: u16 = 1062;

pub(crate) fn connect(host: &str, port: u16, db: Option<&str>, timeout: f64) -> Sql<Conn> {
    let timeout = Duration::from_secs_f64(timeout);
    let opts = OptsBuilder::new()
        .ip_or_hostname(Some(host))
        .tcp_port(port)
        .user(Some("root"))
        .db_name(db)
        .prefer_socket(false)
        .read_timeout(Some(timeout))
        .write_timeout(Some(timeout))
        .tcp_connect_timeout(Some(timeout));
    Ok(Conn::new(opts)?)
}

/// The first column of the first row; `None` for no row or NULL.
pub(crate) fn scalar<T: mysql::prelude::FromValue>(conn: &mut Conn, sql: &str) -> Sql<Option<T>> {
    Ok(conn.query_first::<Option<T>, _>(sql)?.flatten())
}

pub(crate) fn pause_ms(rng: &mut Rng, max: u64) {
    if max > 0 {
        kit::pause(rng.below(max) as f64 / 1000.0);
    }
}

// ------------------------------------------------------------ append

fn table(k: i64, tables: i64) -> String {
    format!("txn{}", k.rem_euclid(tables))
}

fn parse(text: Option<String>) -> Vec<i64> {
    match text {
        Some(text) if !text.is_empty() => text.split(',').filter_map(|x| x.parse().ok()).collect(),
        _ => Vec::new(),
    }
}

fn append(conn: &mut Conn, args: &Args, k: i64, v: i64, txn: bool) -> Sql<()> {
    append_value(conn, args.tables, args.on_duplicate, k, v, txn)
}

/// Jepsen's homebrew upsert (or `on_duplicate`), over `tables` tables.
pub(crate) fn append_value(
    conn: &mut Conn,
    tables: i64,
    on_duplicate: bool,
    k: i64,
    v: i64,
    txn: bool,
) -> Sql<()> {
    let t = table(k, tables);
    if on_duplicate {
        conn.query_drop(format!(
            "INSERT INTO {t} AS t (id, sk, val) VALUES ({k}, {k}, '{v}') \
             ON DUPLICATE KEY UPDATE val = CONCAT(t.val, ',', '{v}')"
        ))?;
        return Ok(());
    }
    let update = format!("UPDATE {t} SET val = CONCAT(val, ',', '{v}') WHERE id = {k}");
    conn.query_drop(&update)?;
    if conn.affected_rows() > 0 {
        return Ok(());
    }
    if txn {
        conn.query_drop("SAVEPOINT upsert")?;
    }
    match conn.query_drop(format!(
        "INSERT INTO {t} (id, sk, val) VALUES ({k}, {k}, '{v}')"
    )) {
        Ok(()) => {
            if txn {
                conn.query_drop("RELEASE SAVEPOINT upsert")?;
            }
            return Ok(());
        }
        Err(mysql::Error::MySqlError(e)) if e.code == DUPLICATE => {
            if txn {
                conn.query_drop("ROLLBACK TO SAVEPOINT upsert")?;
            }
        }
        Err(e) => return Err(e.into()),
    }
    conn.query_drop(&update)?;
    if conn.affected_rows() > 0 {
        Ok(())
    } else {
        Err(SqlError::Upsert)
    }
}

fn read_key(conn: &mut Conn, args: &Args, k: i64) -> Sql<Vec<i64>> {
    read_value(conn, args.tables, k)
}

pub(crate) fn read_value(conn: &mut Conn, tables: i64, k: i64) -> Sql<Vec<i64>> {
    Ok(parse(scalar::<String>(
        conn,
        &format!("SELECT val FROM {} WHERE id = {k}", table(k, tables)),
    )?))
}

pub(crate) enum Outcome<T> {
    Ok(T),
    /// A definite abort, and whether the connection must be replaced.
    Fail(String, bool),
}

/// Run `body` as one transaction (or in autocommit mode when `txn` is
/// false), rolling back after a server error.
pub(crate) fn transact<T>(
    conn: &mut Conn,
    txn: bool,
    body: impl FnOnce(&mut Conn) -> Sql<T>,
) -> Result<Outcome<T>, String> {
    let result = (|| {
        if txn {
            conn.query_drop("START TRANSACTION")?;
        }
        let value = body(conn)?;
        if txn {
            conn.query_drop("COMMIT")?;
        }
        Ok(value)
    })();
    match result {
        Ok(value) => Ok(Outcome::Ok(value)),
        Err(error @ (SqlError::Server(..) | SqlError::Upsert)) => {
            let code = match &error {
                SqlError::Server(code, _) => code.to_string(),
                _ => error.to_string(),
            };
            // A lost rollback still leaves a definite abort: nothing committed.
            let broken = txn && conn.query_drop("ROLLBACK").is_err();
            Ok(Outcome::Fail(code, broken))
        }
        Err(SqlError::Indeterminate(error)) => Err(error),
    }
}

// ------------------------------------------------------------ the run

/// Jepsen's names and genders for the non-repeatable-read test.
const NAMES: [&str; 9] = [
    "currant", "leaf", "brick", "truck", "pebble", "moss", "germ", "ferris", "s'more",
];
const GENDERS: [&str; 6] = ["butch", "dyke", "femme", "enby", "leatherboy", "agender"];

#[derive(Default)]
struct Cycles {
    names: usize,
    genders: usize,
    deletes: usize,
}

struct Run {
    args: Args,
    keys: Mutex<Keys>,
    cycles: Mutex<Cycles>,
    /// Append transactions (the Elle history).
    txns: Mutex<Vec<TxnOp>>,
    /// Non-append operations, as their H lines.
    ops: Mutex<Vec<Value>>,
    stop_at: Instant,
}

impl Run {
    fn host_of(&self, slot: usize) -> &str {
        let hosts = 1 + self.args.replica.len();
        match slot % hosts {
            0 => &self.args.host,
            n => &self.args.replica[n - 1],
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn record_txn(
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
        emit(&format!(
            "H {}",
            json!({"i": index, "p": process, "type": kind, "t0": t0, "t1": t1,
                   "value": value.iter().map(Mop::to_json).collect::<Vec<_>>(), "error": error})
        ));
    }

    #[allow(clippy::too_many_arguments)]
    fn record_op(
        &self,
        process: usize,
        f: &str,
        kind: &str,
        value: Value,
        t0: f64,
        t1: f64,
        error: Option<String>,
    ) {
        let mut ops = self.ops.lock().unwrap();
        let line = json!({"i": ops.len(), "p": process, "f": f, "type": kind, "t0": t0, "t1": t1,
                          "value": value, "error": error});
        emit(&format!("H {line}"));
        ops.push(line);
    }

    fn open(&self, host: &str) -> Sql<Conn> {
        let mut conn = connect(host, self.args.port, Some("jepsen"), self.args.timeout)?;
        conn.query_drop(format!(
            "SET SESSION TRANSACTION ISOLATION LEVEL {}",
            self.args.isolation.sql()
        ))?;
        Ok(conn)
    }

    fn client(&self, slot: usize) {
        let mut process = slot;
        let host = self.host_of(slot).to_string();
        let read_only = host != self.args.host;
        let mut rng = Rng::new(self.args.seed * 1000 + slot as u64);
        let mut conn: Option<Conn> = None;
        while Instant::now() < self.stop_at {
            if conn.is_none() {
                match self.open(&host) {
                    Ok(c) => conn = Some(c),
                    Err(error) => {
                        evidence(
                            "connect-error",
                            json!({"process": process, "host": host, "error": error.to_string()}),
                        );
                        kit::pause(1.0);
                        continue;
                    }
                }
            }
            let c = conn.as_mut().expect("connected");
            let broken = match self.args.workload {
                Workload::Append => self.append_op(c, process, read_only, &mut rng),
                Workload::Nrr => self.nrr_op(c, process, &mut rng),
                Workload::Mav => self.mav_op(c, process, &mut rng),
            };
            if broken {
                conn = None;
                process += self.args.clients;
            }
        }
    }

    /// One list-append transaction; whether the connection must be replaced.
    fn append_op(&self, conn: &mut Conn, process: usize, read_only: bool, rng: &mut Rng) -> bool {
        let mut ops = self.keys.lock().unwrap().transaction();
        if read_only {
            for mop in &mut ops {
                *mop = mop.invocation();
                if let Mop::Append(k, _) = mop {
                    *mop = Mop::Read(k.clone(), None);
                }
            }
        }
        let t0 = now_us();
        let sleep = self.args.mop_sleep_ms;
        let result = transact(conn, ops.len() > 1, |conn| {
            let mut value = Vec::new();
            for mop in &ops {
                pause_ms(rng, sleep);
                match mop {
                    Mop::Read(Atom::Int(k), _) => value.push(Mop::Read(
                        Atom::Int(*k),
                        Some(read_key(conn, &self.args, *k)?),
                    )),
                    Mop::Append(Atom::Int(k), v) => {
                        append(conn, &self.args, *k, *v, ops.len() > 1)?;
                        value.push(mop.clone());
                    }
                    _ => {}
                }
            }
            Ok(value)
        });
        let (kind, value, error, broken) = match result {
            Ok(Outcome::Ok(value)) => ("ok", Some(value), None, false),
            Ok(Outcome::Fail(code, broken)) => ("fail", None, Some(code), broken),
            Err(error) => ("info", None, Some(error), true),
        };
        self.record_txn(json!(process), kind, &ops, value, t0, now_us(), error);
        broken
    }

    fn nrr_op(&self, conn: &mut Conn, process: usize, rng: &mut Rng) -> bool {
        let sleep = self.args.mop_sleep_ms;
        let t0 = now_us();
        // Jepsen's gen/mix over three generators: name changes, alternating
        // deletes and inserts, and reads.
        let (f, sql, value) = {
            let mut cycles = self.cycles.lock().unwrap();
            match rng.below(3) {
                0 => {
                    let name = NAMES[cycles.names % NAMES.len()];
                    cycles.names += 1;
                    let sql = format!(
                        "UPDATE people SET name = '{}' WHERE id = 0",
                        name.replace('\'', "''")
                    );
                    ("change-name", Some(sql), json!({"id": 0, "name": name}))
                }
                1 => {
                    let delete = cycles.deletes.is_multiple_of(2);
                    cycles.deletes += 1;
                    if delete {
                        (
                            "delete",
                            Some("DELETE FROM people WHERE id = 0".to_string()),
                            json!({"id": 0}),
                        )
                    } else {
                        (
                            "insert",
                            Some(
                                "INSERT INTO people (id, name, gender) VALUES (0, 'moss', 'enby')"
                                    .to_string(),
                            ),
                            json!({"id": 0, "name": "moss", "gender": "enby"}),
                        )
                    }
                }
                _ => {
                    let gender = GENDERS[cycles.genders % GENDERS.len()];
                    cycles.genders += 1;
                    ("read", None, json!({"id": 0, "gender": gender}))
                }
            }
        };
        let result = match &sql {
            Some(sql) => transact(conn, false, |conn| {
                conn.query_drop(sql)?;
                Ok(value.clone())
            }),
            None => transact(conn, true, |conn| {
                let name1 = scalar::<String>(conn, "SELECT name FROM people WHERE id = 0")?;
                pause_ms(rng, sleep);
                conn.query_drop(format!(
                    "UPDATE people SET gender = '{}' WHERE id = 0",
                    value["gender"].as_str().unwrap_or("enby")
                ))?;
                pause_ms(rng, sleep);
                let name2 = scalar::<String>(conn, "SELECT name FROM people WHERE id = 0")?;
                Ok(json!({"id": 0, "gender": value["gender"], "name1": name1, "name2": name2}))
            }),
        };
        self.finish_op(process, f, value, t0, result)
    }

    fn mav_op(&self, conn: &mut Conn, process: usize, rng: &mut Rng) -> bool {
        let sleep = self.args.mop_sleep_ms;
        let t0 = now_us();
        if rng.below(2) == 0 {
            let mut rows = [0, 1];
            rng.shuffle(&mut rows);
            let result = transact(conn, true, |conn| {
                for id in rows {
                    pause_ms(rng, sleep);
                    conn.query_drop(format!(
                        "UPDATE mav SET `value` = `value` + 1 WHERE id = {id}"
                    ))?;
                }
                Ok(json!({"order": rows}))
            });
            return self.finish_op(process, "write", json!({"order": rows}), t0, result);
        }
        let noop = rng.below(100);
        let result = transact(conn, true, |conn| {
            let a1 = scalar::<i64>(conn, "SELECT `value` FROM mav WHERE id = 0")?;
            pause_ms(rng, sleep);
            conn.query_drop(format!("UPDATE mav SET noop = {noop} WHERE id = 1"))?;
            let b2 = scalar::<i64>(conn, "SELECT `value` FROM mav WHERE id = 1")?;
            pause_ms(rng, sleep);
            let a2 = scalar::<i64>(conn, "SELECT `value` FROM mav WHERE id = 0")?;
            Ok(json!({"a1": a1, "b2": b2, "a2": a2}))
        });
        self.finish_op(process, "read", json!({}), t0, result)
    }

    fn finish_op(
        &self,
        process: usize,
        f: &str,
        invoke: Value,
        t0: f64,
        result: Result<Outcome<Value>, String>,
    ) -> bool {
        let (kind, value, error, broken) = match result {
            Ok(Outcome::Ok(value)) => ("ok", value, None, false),
            Ok(Outcome::Fail(code, broken)) => ("fail", invoke, Some(code), broken),
            Err(error) => ("info", invoke, Some(error), true),
        };
        self.record_op(process, f, kind, value, t0, now_us(), error);
        broken
    }

    fn final_read(&self) -> Option<Vec<Mop>> {
        let keys: Vec<i64> = self.keys.lock().unwrap().touched.iter().copied().collect();
        for attempt in 0..20 {
            let read = (|| -> Sql<Vec<Mop>> {
                let mut conn = connect(&self.args.host, self.args.port, Some("jepsen"), 30.0)?;
                conn.query_drop("START TRANSACTION WITH CONSISTENT SNAPSHOT, READ ONLY")?;
                let mut value = Vec::new();
                for k in &keys {
                    value.push(Mop::Read(
                        Atom::Int(*k),
                        Some(read_key(&mut conn, &self.args, *k)?),
                    ));
                }
                conn.query_drop("COMMIT")?;
                Ok(value)
            })();
            match read {
                Ok(value) => return Some(value),
                Err(error) => {
                    evidence(
                        "final-read-error",
                        json!({"attempt": attempt, "error": error.to_string()}),
                    );
                    kit::pause(2.0);
                }
            }
        }
        None
    }

    fn counts(&self) -> Value {
        let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
        for t in self.txns.lock().unwrap().iter() {
            *counts.entry(elle::kind_of(t)).or_default() += 1;
        }
        for op in self.ops.lock().unwrap().iter() {
            let kind = match op["type"].as_str() {
                Some("ok") => "ok",
                Some("fail") => "fail",
                _ => "info",
            };
            *counts.entry(kind).or_default() += 1;
        }
        json!(counts)
    }
}

/// Acknowledged appends missing from the final read of their key.
pub fn lost_appends(txns: &[TxnOp], final_read: &[Mop]) -> Vec<Value> {
    let finals: BTreeMap<&Atom, BTreeSet<i64>> = final_read
        .iter()
        .filter_map(|m| match m {
            Mop::Read(k, Some(values)) => Some((k, values.iter().copied().collect())),
            _ => None,
        })
        .collect();
    let mut lost = Vec::new();
    for t in txns.iter().filter(|t| elle::kind_of(t) == "ok") {
        for mop in &t.op.mops {
            if let Mop::Append(k, v) = mop {
                if finals.get(k).is_some_and(|values| !values.contains(v)) {
                    lost.push(
                        json!({"txn": t.op.id.to_json(), "key": k.to_json(), "value": v,
                        "acked_at": match t.outcome {
                            wgl::OpOutcome::Completed { at, .. } => at as f64 / 1e9,
                            _ => 0.0,
                        }}),
                    );
                }
            }
        }
    }
    lost
}

// ------------------------------------------------------------ setup

pub(crate) fn wait_ready(host: &str, port: u16, deadline: Instant) -> Result<Conn, String> {
    loop {
        let attempt = connect(host, port, None, 10.0).and_then(|mut conn| {
            conn.query_drop("SELECT 1")?;
            Ok(conn)
        });
        match attempt {
            Ok(conn) => return Ok(conn),
            Err(error) if Instant::now() > deadline => return Err(format!("{host}: {error}")),
            Err(_) => kit::pause(1.0),
        }
    }
}

fn server_info(conn: &mut Conn) -> Value {
    let row: Option<(String, String, i64, i64)> = conn
        .query_first(
            "SELECT @@version, @@transaction_isolation, @@innodb_flush_log_at_trx_commit, @@log_bin",
        )
        .ok()
        .flatten();
    match row {
        Some((version, isolation, flush, log_bin)) => json!({"version": version,
            "default_isolation": isolation, "innodb_flush_log_at_trx_commit": flush, "log_bin": log_bin}),
        None => Value::Null,
    }
}

fn replicate(replica: &str, args: &Args, deadline: Instant) -> Result<Value, String> {
    let mut conn = wait_ready(replica, args.port, deadline)?;
    let sql = format!(
        "CHANGE REPLICATION SOURCE TO SOURCE_HOST = '{}', SOURCE_PORT = {}, SOURCE_USER = 'root', \
         SOURCE_PASSWORD = '', SOURCE_AUTO_POSITION = 1, GET_SOURCE_PUBLIC_KEY = 1",
        args.host, args.port
    );
    conn.query_drop(sql)
        .map_err(|e| format!("{replica}: {e}"))?;
    conn.query_drop("START REPLICA")
        .map_err(|e| format!("{replica}: {e}"))?;
    let settings: Option<(i64, String)> = conn
        .query_first("SELECT @@replica_parallel_workers, @@replica_preserve_commit_order")
        .ok()
        .flatten();
    let mut info = server_info(&mut conn);
    if let Some((workers, preserve)) = settings {
        info["replica_parallel_workers"] = workers.into();
        info["replica_preserve_commit_order"] = preserve.into();
    }
    Ok(info)
}

fn schema(conn: &mut Conn, args: &Args) -> Sql<()> {
    conn.query_drop("CREATE DATABASE IF NOT EXISTS jepsen")?;
    conn.query_drop("USE jepsen")?;
    match args.workload {
        Workload::Append => {
            for i in 0..args.tables {
                conn.query_drop(format!("DROP TABLE IF EXISTS txn{i}"))?;
                conn.query_drop(format!(
                    "CREATE TABLE txn{i} (id int NOT NULL PRIMARY KEY, sk int NOT NULL, val text)"
                ))?;
            }
        }
        Workload::Nrr => {
            conn.query_drop("DROP TABLE IF EXISTS people")?;
            conn.query_drop(
                "CREATE TABLE people (id int NOT NULL, name text NOT NULL, gender text NOT NULL, PRIMARY KEY (id))",
            )?;
            conn.query_drop("INSERT INTO people (id, name, gender) VALUES (0, 'moss', 'enby')")?;
        }
        Workload::Mav => {
            conn.query_drop("DROP TABLE IF EXISTS mav")?;
            conn.query_drop(
                "CREATE TABLE mav (id int NOT NULL, `value` int NOT NULL, noop int NOT NULL, PRIMARY KEY (id))",
            )?;
            conn.query_drop("INSERT INTO mav (id, `value`, noop) VALUES (0, 0, 0), (1, 0, 0)")?;
        }
    }
    Ok(())
}

/// Wait until a replica has applied the schema.
fn replica_caught_up(replica: &str, args: &Args, deadline: Instant) -> Result<(), String> {
    let table = match args.workload {
        Workload::Append => format!("txn{}", args.tables - 1),
        Workload::Nrr => "people".into(),
        Workload::Mav => "mav".into(),
    };
    let sql = format!(
        "SELECT COUNT(*) FROM information_schema.tables WHERE table_schema = 'jepsen' AND table_name = '{table}'"
    );
    loop {
        let found = connect(replica, args.port, None, 10.0)
            .and_then(|mut conn| scalar::<i64>(&mut conn, &sql))
            .ok()
            .flatten();
        if found == Some(1) {
            return Ok(());
        }
        if Instant::now() > deadline {
            return Err(format!("{replica}: the schema never replicated"));
        }
        kit::pause(1.0);
    }
}

fn crash_schedule(args: &Args) {
    let start = Instant::now();
    let mut ctl = match kit::connect_world(None) {
        Ok(ctl) => ctl,
        Err(error) => {
            evidence("crash-failed", json!({"error": error.0}));
            return;
        }
    };
    for at in &args.crash_at {
        kit::pause(at - start.elapsed().as_secs_f64());
        if let Err(error) =
            nemesis::crash_one(&mut ctl, &mut nemesis::Sleep, &args.host, args.crash_down)
        {
            evidence("crash-failed", json!({"error": error.0}));
            return;
        }
    }
}

// ------------------------------------------------------------ checks

fn check_append(run: &Run, model: &str, final_read: Option<&[Mop]>) -> (u8, String) {
    let txns = run.txns.lock().unwrap().clone();
    let committed = txns.iter().filter(|t| elle::kind_of(t) == "ok").count();
    let result = match report(&txns, model, run.args.examples) {
        Ok(result) => result,
        Err(error) => return (kit::INCONCLUSIVE, format!("checker: {error}")),
    };
    let lost = final_read.map(|f| lost_appends(&txns, f));
    if let Some(lost) = &lost {
        evidence(
            "durability",
            json!({"lost": lost.len(), "examples": lost.iter().take(run.args.examples).collect::<Vec<_>>()}),
        );
    }
    let lost_count = lost.as_ref().map_or(0, Vec::len);
    let types = json!(result.txns);
    let valid = result.valid && lost_count == 0;
    if valid && (committed <= 1 || final_read.is_none()) {
        let reason = if committed <= 1 {
            "no transaction committed"
        } else {
            "the final read failed"
        };
        return (kit::INCONCLUSIVE, format!("{reason}: {types}"));
    }
    let word = if valid { "valid" } else { "INVALID" };
    let text = format!(
        "{word} {} append checked as {model}: {} lost {lost_count} txns {types}",
        run.args.isolation.name(),
        json!(result.counts)
    );
    (if valid { kit::OK } else { kit::VIOLATION }, text)
}

fn nrr_reads(ops: &[Value]) -> Vec<views::Reads<Value>> {
    ops.iter()
        .filter(|op| op["f"] == "read" && op["type"] == "ok")
        .map(|op| views::Reads {
            id: op["i"].clone(),
            reads: vec![
                (op["value"]["id"].clone(), op["value"]["name1"].clone()),
                (op["value"]["id"].clone(), op["value"]["name2"].clone()),
            ],
        })
        .collect()
}

fn mav_reads(ops: &[Value]) -> Vec<views::Reads<i64>> {
    ops.iter()
        .filter(|op| op["f"] == "read" && op["type"] == "ok")
        .filter_map(|op| {
            let v = |k: &str| op["value"][k].as_i64();
            Some(views::Reads {
                id: op["i"].clone(),
                reads: vec![
                    (0.into(), v("a1")?),
                    (1.into(), v("b2")?),
                    (0.into(), v("a2")?),
                ],
            })
        })
        .collect()
}

/// The view check for `nrr` or `mav` operations: (valid, summary, errors).
fn check_views(workload: Workload, ops: &[Value]) -> (bool, Value, Vec<Value>) {
    let reads = ops
        .iter()
        .filter(|op| op["f"] == "read" && op["type"] == "ok")
        .count();
    let errors = match workload {
        Workload::Nrr => views::repeatable(&nrr_reads(ops)),
        _ => views::monotonic(&mav_reads(ops)),
    };
    let mut summary = json!({"reads": reads, "errors": errors.len()});
    if workload == Workload::Nrr {
        summary["kinds"] = json!(views::kinds(&errors));
    }
    (errors.is_empty(), summary, errors)
}

fn report_views(run: &Run) -> (u8, String) {
    let ops = run.ops.lock().unwrap().clone();
    let (valid, summary, errors) = check_views(run.args.workload, &ops);
    evidence("check", json!({"valid": valid, "summary": summary}));
    for error in errors.iter().take(run.args.examples) {
        evidence("anomaly", error.clone());
    }
    let what = match run.args.workload {
        Workload::Nrr => "non-repeatable reads",
        _ => "non-monotonic views",
    };
    if valid && summary["reads"] == 0 {
        return (
            kit::INCONCLUSIVE,
            format!("no read committed: {}", run.counts()),
        );
    }
    let word = if valid { "valid" } else { "INVALID" };
    let text = format!(
        "{word} {} {}: {} {what} in {} reads {} txns {}",
        run.args.isolation.name(),
        run.args.workload.name(),
        summary["errors"],
        summary["reads"],
        summary.get("kinds").cloned().unwrap_or(json!({})),
        run.counts()
    );
    (if valid { kit::OK } else { kit::VIOLATION }, text)
}

pub fn run(args: Args) -> u8 {
    kit::start_clock();
    let model = args
        .model
        .clone()
        .unwrap_or_else(|| args.isolation.model().into());
    evidence(
        "config",
        json!({"workload": args.workload.name(), "host": args.host, "replicas": args.replica,
               "port": args.port, "isolation": args.isolation.name(), "model_checked": model,
               "clients": args.clients, "duration": args.duration, "tables": args.tables,
               "key_count": args.key_count, "max_writes_per_key": args.max_writes_per_key,
               "max_txn_length": args.max_txn_length, "read_fraction": args.read_fraction,
               "on_duplicate": args.on_duplicate, "mop_sleep_ms": args.mop_sleep_ms,
               "timeout": args.timeout, "crash_at": args.crash_at, "crash_down": args.crash_down,
               "seed": args.seed, "label": args.label}),
    );
    let deadline = Instant::now() + Duration::from_secs_f64(args.ready_timeout);
    let mut conn = match wait_ready(&args.host, args.port, deadline) {
        Ok(conn) => conn,
        Err(error) => {
            return verdict(
                kit::INCONCLUSIVE,
                &format!("mysql never became ready: {error}"),
            )
        }
    };
    let mut replicas = serde_json::Map::new();
    for replica in &args.replica {
        match replicate(replica, &args, deadline) {
            Ok(info) => {
                replicas.insert(replica.clone(), info);
            }
            Err(error) => {
                return verdict(
                    kit::INCONCLUSIVE,
                    &format!("replication setup failed: {error}"),
                )
            }
        }
    }
    if let Err(error) = schema(&mut conn, &args) {
        return verdict(kit::INCONCLUSIVE, &format!("schema setup failed: {error}"));
    }
    evidence(
        "ready",
        json!({"server": server_info(&mut conn), "replicas": replicas}),
    );
    drop(conn);
    for replica in &args.replica {
        if let Err(error) = replica_caught_up(replica, &args, deadline) {
            return verdict(kit::INCONCLUSIVE, &error);
        }
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
        cycles: Mutex::new(Cycles::default()),
        txns: Mutex::new(Vec::new()),
        ops: Mutex::new(Vec::new()),
        stop_at: started + Duration::from_secs_f64(args.duration),
        args: args.clone(),
    });
    let nemesis = (!args.crash_at.is_empty()).then(|| {
        let args = args.clone();
        thread::spawn(move || crash_schedule(&args))
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
        evidence("progress", run.counts());
    }
    for thread in threads {
        let _ = thread.join();
    }
    if let Some(nemesis) = nemesis {
        let _ = nemesis.join();
    }
    let (code, text) = match args.workload {
        Workload::Append => {
            let final_read = run.final_read();
            if let Some(value) = &final_read {
                let t = now_us();
                run.record_txn(json!("final"), "ok", value, Some(value.clone()), t, t, None);
            }
            check_append(&run, &model, final_read.as_deref())
        }
        Workload::Nrr | Workload::Mav => report_views(&run),
    };
    verdict(code, &text)
}

/// Re-check a saved oracle log; the workload comes from its config evidence.
pub fn recheck(text: &str, model: &str) -> Result<(u8, Value), String> {
    let config = kit::evidence_of(text, "config").unwrap_or_default();
    let lines = kit::tagged_lines(text, "H ");
    match config["workload"].as_str().unwrap_or("append") {
        "append" => {
            let txns = elle::from_h_lines(&lines)?;
            let result = elle::check(&txns, model)?;
            let mut out = result.to_json();
            let final_read = txns
                .iter()
                .find(|t| t.op.process == Some(Atom::from("final")))
                .and_then(|t| match &t.outcome {
                    wgl::OpOutcome::Completed { ret, .. } => Some(ret.clone()),
                    _ => None,
                });
            let lost = final_read.map(|f| lost_appends(&txns, &f));
            out["lost"] = json!(lost.as_ref().map(Vec::len));
            let valid = result.valid && lost.is_none_or(|l| l.is_empty());
            Ok((if valid { kit::OK } else { kit::VIOLATION }, out))
        }
        name => {
            let workload = if name == "nrr" {
                Workload::Nrr
            } else {
                Workload::Mav
            };
            let (valid, summary, errors) = check_views(workload, &lines);
            Ok((
                if valid { kit::OK } else { kit::VIOLATION },
                json!({"valid": valid, "summary": summary, "errors": errors}),
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(id: i64, value: Vec<Mop>, at: u64) -> TxnOp {
        elle::txn(id, "ok", value, None, 0, at, None).unwrap()
    }

    #[test]
    fn lost_appends_are_acked_values_the_final_read_lacks() {
        let txns = [
            ok(0, vec![Mop::Append(1.into(), 1)], 1),
            ok(
                1,
                vec![Mop::Append(1.into(), 2), Mop::Append(2.into(), 1)],
                2,
            ),
            elle::txn(2, "info", vec![Mop::Append(1.into(), 3)], None, 0, 3, None).unwrap(),
        ];
        let final_read = [
            Mop::Read(1.into(), Some(vec![1])),
            Mop::Read(2.into(), Some(vec![])),
        ];
        let lost = lost_appends(&txns, &final_read);
        let values: Vec<(Value, Value)> = lost
            .iter()
            .map(|l| (l["key"].clone(), l["value"].clone()))
            .collect();
        assert_eq!(values, [(json!(1), json!(2)), (json!(2), json!(1))]);
    }

    #[test]
    fn view_checks_read_h_lines() {
        let lines = [
            json!({"i": 0, "f": "read", "type": "ok", "value": {"id": 0, "name1": "pebble", "name2": "moss"}}),
            json!({"i": 1, "f": "read", "type": "ok", "value": {"id": 0, "name1": null, "name2": "moss"}}),
            json!({"i": 2, "f": "read", "type": "fail", "value": {"id": 0}}),
            json!({"i": 3, "f": "change-name", "type": "ok", "value": {"id": 0, "name": "leaf"}}),
            json!({"i": 4, "f": "read", "type": "ok", "value": {"id": 0, "name1": "leaf", "name2": "leaf"}}),
        ];
        let (valid, summary, _) = check_views(Workload::Nrr, &lines);
        assert!(!valid);
        assert_eq!(
            summary,
            json!({"reads": 3, "errors": 2, "kinds": {"appeared": 1, "changed": 1}})
        );
        let mav = [
            json!({"i": 0, "f": "read", "type": "ok", "value": {"a1": 0, "b2": 1, "a2": 0}}),
            json!({"i": 1, "f": "read", "type": "ok", "value": {"a1": 2, "b2": 2, "a2": 2}}),
            json!({"i": 2, "f": "write", "type": "ok", "value": {"order": [1, 0]}}),
        ];
        let (valid, summary, errors) = check_views(Workload::Mav, &mav);
        assert!(!valid);
        assert_eq!(summary, json!({"reads": 2, "errors": 1}));
        assert_eq!(errors[0]["txn"], 0);
    }
}
