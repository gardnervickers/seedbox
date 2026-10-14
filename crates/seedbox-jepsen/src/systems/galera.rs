//! Jepsen's MariaDB Galera Cluster 12.1.2 analysis (2026-03-16) as a Seedbox
//! oracle: Elle list-append on a three-node cluster through the `mysql`
//! crate, with the MySQL oracle's append, transaction and error discipline
//! ([`super::mysql`]).
//!
//! * The oracle forms the cluster. Each node runs `galera-node.sh`, which
//!   recovers its last committed position (`mariadbd --wsrep-recover`) and
//!   then waits on port `--control-port` for one connection: it sends a JSON
//!   report and reads `bootstrap` or `join`. On first boot the first node
//!   bootstraps; after a total failure the most advanced node does (highest
//!   recovered seqno, then `safe_to_bootstrap`, then node order), as an
//!   operator following MariaDB's documentation would. The others join one
//!   at a time, each once the previous is `Synced`.
//! * Clients run on `--client-nodes` round robin; those on `--read-only`
//!   nodes turn every append into a read (Jepsen's write-on-n1, read-on-n2
//!   stale-read setup). `--sync-wait` sets `wsrep_sync_wait` per session.
//! * `--fault crash` power-fails every node at `--fault-at` and reboots them
//!   (page cache and unflushed sectors lost); `--fault kill` stops every
//!   node's processes and restarts them, as Jepsen's coordinated kill did.
//!   The cluster is then re-formed as above while the clients keep going.
//! * At the end, once every node is Primary and Synced, each node reads
//!   every key in one consistent snapshot (`final-nN`). The history is
//!   checked by the Elle-style checker against `--model`, for real-time
//!   stale reads ([`crate::checker::realtime`]), and every acknowledged
//!   append must be in every node's final read (`lost`).
use super::mysql::{self as my, Isolation, Outcome};
use super::postgres::{report, Keys};
use crate::checker::elle::{self, Atom, Mop, TxnOp};
use crate::checker::realtime;
use crate::kit::{self, emit, evidence, now, now_us, verdict, Rng};
use crate::nemesis;
use mysql::prelude::Queryable;
use mysql::Conn;
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    io::{BufRead, BufReader, Write},
    net::TcpStream,
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
};

#[derive(clap::Args, Debug, Clone)]
pub struct Args {
    /// The cluster's nodes; the first bootstraps a fresh cluster.
    #[arg(long, value_delimiter = ',', default_value = "n1,n2,n3")]
    pub nodes: Vec<String>,
    /// Nodes the clients run on, round robin [default: --nodes].
    #[arg(long, value_delimiter = ',')]
    pub client_nodes: Vec<String>,
    /// Client nodes whose clients only read (appends become reads).
    #[arg(long, value_delimiter = ',')]
    pub read_only: Vec<String>,
    #[arg(long, default_value_t = 3306)]
    pub port: u16,
    /// The port galera-node.sh waits on for bootstrap or join.
    #[arg(long, default_value_t = 4599)]
    pub control_port: u16,
    #[arg(long, value_enum, default_value = "repeatable-read")]
    pub isolation: Isolation,
    /// Consistency model the history is checked against.
    #[arg(long, default_value = "snapshot-isolation")]
    pub model: String,
    /// Set wsrep_sync_wait to this in every client session.
    #[arg(long)]
    pub sync_wait: Option<u32>,
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
    /// Each statement first waits a random 0 to this many milliseconds.
    #[arg(long, default_value_t = 10)]
    pub mop_sleep_ms: u64,
    /// Seconds before a statement counts as indeterminate.
    #[arg(long, default_value_t = 10.0)]
    pub timeout: f64,
    /// Seconds to form the cluster, at the start and after a fault.
    #[arg(long, default_value_t = 900.0)]
    pub ready_timeout: f64,
    #[arg(long, value_enum, default_value = "none")]
    pub fault: Fault,
    /// Seconds after the workload starts at which --fault hits every node.
    #[arg(long, default_value_t = 20.0)]
    pub fault_at: f64,
    /// Seconds the nodes stay down.
    #[arg(long, default_value_t = 2.0)]
    pub fault_down: f64,
    #[arg(long, default_value_t = 5.0)]
    pub pause_length: f64,
    #[arg(long, default_value_t = 5.0)]
    pub pause_gap: f64,
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
pub enum Fault {
    /// No fault.
    None,
    /// Power-fail every node together, then reboot them.
    Crash,
    /// Kill every node's processes together (Seedbox stop), then restart them.
    Kill,
    /// From --fault-at to the end, pause a random node's processes for
    /// --pause-length seconds, then --pause-gap seconds of quiet (Jepsen's pause nemesis).
    Pause,
}

impl Fault {
    fn name(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Crash => "crash",
            Self::Kill => "kill",
            Self::Pause => "pause",
        }
    }
}

/// MariaDB's error for a node that is not yet (or no longer) in a Primary
/// component: the client reconnects after a pause instead of spinning.
const WSREP_NOT_READY: &str = "1047";

// ------------------------------------------------------------ cluster formation

/// One global status variable, upper-case name.
fn status(conn: &mut Conn, name: &str) -> Option<String> {
    my::scalar::<String>(
        conn,
        &format!(
            "SELECT VARIABLE_VALUE FROM information_schema.GLOBAL_STATUS WHERE VARIABLE_NAME = '{name}'"
        ),
    )
    .ok()
    .flatten()
}

/// A node's wsrep view: (cluster status, local state, cluster size).
fn wsrep_view(host: &str, port: u16) -> Option<(String, String, i64)> {
    let mut conn = my::connect(host, port, None, 5.0).ok()?;
    let cluster = status(&mut conn, "WSREP_CLUSTER_STATUS")?;
    let state = status(&mut conn, "WSREP_LOCAL_STATE_COMMENT")?;
    let size = status(&mut conn, "WSREP_CLUSTER_SIZE")?.parse().ok()?;
    Some((cluster, state, size))
}

/// Wait until `host` is Primary and Synced in a cluster of at least `size`.
fn wait_synced(host: &str, port: u16, size: i64, deadline: Instant) -> Result<(), String> {
    let mut last = None;
    loop {
        let view = wsrep_view(host, port);
        if let Some((cluster, state, n)) = &view {
            if cluster == "Primary" && state == "Synced" && *n >= size {
                return Ok(());
            }
        }
        if view.is_some() {
            last = view;
        }
        if Instant::now() > deadline {
            return Err(format!("{host} never became Primary and Synced: {last:?}"));
        }
        kit::pause(1.0);
    }
}

/// A node waiting in galera-node.sh: its report and the open connection.
struct Waiting {
    node: String,
    report: Value,
    stream: TcpStream,
}

fn await_report(node: &str, port: u16, deadline: Instant) -> Result<Waiting, String> {
    loop {
        let attempt = (|| -> Result<Waiting, String> {
            let stream =
                kit::http::connect(node, port, Duration::from_secs(30)).map_err(|e| e.0)?;
            let mut line = String::new();
            BufReader::new(stream.try_clone().map_err(|e| e.to_string())?)
                .read_line(&mut line)
                .map_err(|e| e.to_string())?;
            let report: Value = serde_json::from_str(line.trim())
                .map_err(|e| format!("{node}: bad report {line:?}: {e}"))?;
            stream.set_read_timeout(None).map_err(|e| e.to_string())?;
            Ok(Waiting {
                node: node.into(),
                report,
                stream,
            })
        })();
        match attempt {
            Ok(waiting) => return Ok(waiting),
            Err(error) if Instant::now() > deadline => {
                return Err(format!("{node}: no report: {error}"))
            }
            Err(_) => kit::pause(1.0),
        }
    }
}

/// The node to bootstrap from: the first node of a fresh cluster, otherwise
/// the highest recovered seqno, then safe_to_bootstrap, then node order.
pub fn choose_bootstrap(reports: &[Value]) -> usize {
    let fresh = |r: &Value| r["fresh"].as_bool().unwrap_or(false);
    if reports.iter().all(fresh) {
        return 0;
    }
    let seqno = |r: &Value| {
        r["seqno"]
            .as_i64()
            .unwrap_or(-1)
            .max(r["grastate_seqno"].as_i64().unwrap_or(-1))
    };
    let mut best = 0;
    for (index, r) in reports.iter().enumerate() {
        let key = |r: &Value| {
            (
                !fresh(r),
                seqno(r),
                r["safe_to_bootstrap"].as_i64().unwrap_or(0),
            )
        };
        if key(r) > key(&reports[best]) {
            best = index;
        }
    }
    best
}

/// Form the cluster from nodes waiting in galera-node.sh; the formation evidence.
fn form_cluster(args: &Args, why: &str, deadline: Instant) -> Result<Value, String> {
    let started = now();
    let mut waiting = Vec::new();
    for node in &args.nodes {
        waiting.push(await_report(node, args.control_port, deadline)?);
    }
    let reports: Vec<Value> = waiting.iter().map(|w| w.report.clone()).collect();
    let chosen = choose_bootstrap(&reports);
    let order: Vec<usize> = std::iter::once(chosen)
        .chain((0..waiting.len()).filter(|&i| i != chosen))
        .collect();
    evidence(
        "bootstrap",
        json!({"why": why, "reports": reports, "bootstrap": waiting[chosen].node}),
    );
    let mut synced = serde_json::Map::new();
    for (position, &index) in order.iter().enumerate() {
        let w = &mut waiting[index];
        let command = if position == 0 {
            "bootstrap\n"
        } else {
            "join\n"
        };
        w.stream
            .write_all(command.as_bytes())
            .map_err(|e| format!("{}: {e}", w.node))?;
        let _ = w.stream.shutdown(std::net::Shutdown::Both);
        wait_synced(&w.node, args.port, position as i64 + 1, deadline)?;
        synced.insert(w.node.clone(), now().into());
    }
    let formed = json!({"why": why, "bootstrap": waiting[chosen].node, "started": started,
                        "synced": synced, "seconds": now() - started});
    evidence("cluster-formed", formed.clone());
    Ok(formed)
}

// ------------------------------------------------------------ the run

struct Run {
    args: Args,
    keys: Mutex<Keys>,
    txns: Mutex<Vec<TxnOp>>,
    stop_at: Instant,
}

impl Run {
    fn client_node(&self, slot: usize) -> &str {
        let nodes = if self.args.client_nodes.is_empty() {
            &self.args.nodes
        } else {
            &self.args.client_nodes
        };
        &nodes[slot % nodes.len()]
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

    fn open(&self, host: &str) -> my::Sql<Conn> {
        let mut conn = my::connect(host, self.args.port, Some("jepsen"), self.args.timeout)?;
        conn.query_drop(format!(
            "SET SESSION TRANSACTION ISOLATION LEVEL {}",
            self.args.isolation.sql()
        ))?;
        if let Some(wait) = self.args.sync_wait {
            conn.query_drop(format!("SET SESSION wsrep_sync_wait = {wait}"))?;
        }
        Ok(conn)
    }

    fn client(&self, slot: usize) {
        let mut process = slot;
        let host = self.client_node(slot).to_string();
        let read_only = self.args.read_only.contains(&host);
        let mut rng = Rng::new(self.args.seed * 1000 + slot as u64);
        let mut conn: Option<Conn> = None;
        let mut failures = 0u64;
        while Instant::now() < self.stop_at {
            if conn.is_none() {
                match self.open(&host) {
                    Ok(c) => {
                        if failures > 0 {
                            evidence(
                                "connected",
                                json!({"process": process, "host": host, "after_failures": failures}),
                            );
                        }
                        failures = 0;
                        conn = Some(c);
                    }
                    Err(error) => {
                        if failures == 0 {
                            evidence(
                                "connect-error",
                                json!({"process": process, "host": host, "error": error.to_string()}),
                            );
                        }
                        failures += 1;
                        kit::pause(1.0);
                        continue;
                    }
                }
            }
            let c = conn.as_mut().expect("connected");
            let (broken, not_ready) = self.append_op(c, process, read_only, &mut rng);
            if broken {
                conn = None;
                process += self.args.clients;
            }
            if not_ready {
                // A node outside a Primary component refuses every statement.
                conn = None;
                kit::pause(1.0);
            }
        }
    }

    /// One list-append transaction; whether the connection must be replaced,
    /// and whether the node refused it as not ready.
    fn append_op(
        &self,
        conn: &mut Conn,
        process: usize,
        read_only: bool,
        rng: &mut Rng,
    ) -> (bool, bool) {
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
        let (sleep, tables) = (self.args.mop_sleep_ms, self.args.tables);
        let txn = ops.len() > 1;
        let result = my::transact(conn, txn, |conn| {
            let mut value = Vec::new();
            for mop in &ops {
                my::pause_ms(rng, sleep);
                match mop {
                    Mop::Read(Atom::Int(k), _) => value.push(Mop::Read(
                        Atom::Int(*k),
                        Some(my::read_value(conn, tables, *k)?),
                    )),
                    Mop::Append(Atom::Int(k), v) => {
                        my::append_value(conn, tables, false, *k, *v, txn)?;
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
        let not_ready = error.as_deref() == Some(WSREP_NOT_READY);
        self.record_txn(json!(process), kind, &ops, value, t0, now_us(), error);
        (broken, not_ready)
    }

    /// Every touched key on `node` in one consistent snapshot, after the
    /// node has applied everything certified before it (wsrep_sync_wait).
    fn final_read(&self, node: &str) -> Option<Vec<Mop>> {
        let keys: Vec<i64> = self.keys.lock().unwrap().touched.iter().copied().collect();
        for attempt in 0..20 {
            let read = (|| -> my::Sql<Vec<Mop>> {
                let mut conn = my::connect(node, self.args.port, Some("jepsen"), 30.0)?;
                conn.query_drop("SET SESSION wsrep_sync_wait = 1")?;
                conn.query_drop("START TRANSACTION WITH CONSISTENT SNAPSHOT, READ ONLY")?;
                let mut value = Vec::new();
                for k in &keys {
                    value.push(Mop::Read(
                        Atom::Int(*k),
                        Some(my::read_value(&mut conn, self.args.tables, *k)?),
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
                        json!({"node": node, "attempt": attempt, "error": error.to_string()}),
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
        json!(counts)
    }
}

// ------------------------------------------------------------ setup

fn server_info(host: &str, port: u16) -> Value {
    let Ok(mut conn) = my::connect(host, port, None, 10.0) else {
        return Value::Null;
    };
    let row: Option<(String, i64, i64, String, i64)> = conn
        .query_first(
            "SELECT @@version, @@innodb_flush_log_at_trx_commit, @@wsrep_sync_wait, \
             @@transaction_isolation, @@innodb_snapshot_isolation",
        )
        .ok()
        .flatten();
    let provider = status(&mut conn, "WSREP_PROVIDER_VERSION");
    match row {
        Some((version, flush, sync_wait, isolation, snapshot)) => json!({"version": version,
            "innodb_flush_log_at_trx_commit": flush, "wsrep_sync_wait": sync_wait,
            "default_isolation": isolation, "innodb_snapshot_isolation": snapshot,
            "wsrep_provider_version": provider}),
        None => Value::Null,
    }
}

fn schema(args: &Args, deadline: Instant) -> Result<(), String> {
    let mut conn = my::wait_ready(&args.nodes[0], args.port, deadline)?;
    let sql = |conn: &mut Conn, q: String| conn.query_drop(&q).map_err(|e| format!("{q}: {e}"));
    sql(&mut conn, "CREATE DATABASE IF NOT EXISTS jepsen".into())?;
    for i in 0..args.tables {
        sql(&mut conn, format!("DROP TABLE IF EXISTS jepsen.txn{i}"))?;
        sql(
            &mut conn,
            format!("CREATE TABLE jepsen.txn{i} (id int NOT NULL PRIMARY KEY, sk int NOT NULL, val text)"),
        )?;
    }
    // DDL replicates in total order; wait for every node to have the tables.
    let check = format!(
        "SELECT COUNT(*) FROM information_schema.tables WHERE table_schema = 'jepsen' AND table_name = 'txn{}'",
        args.tables - 1
    );
    for node in &args.nodes {
        loop {
            let found = my::connect(node, args.port, None, 10.0)
                .and_then(|mut c| my::scalar::<i64>(&mut c, &check))
                .ok()
                .flatten();
            if found == Some(1) {
                break;
            }
            if Instant::now() > deadline {
                return Err(format!("{node}: the schema never replicated"));
            }
            kit::pause(1.0);
        }
    }
    Ok(())
}

fn fault_schedule(args: &Args, started: Instant) {
    let mut ctl = match kit::connect_world(None) {
        Ok(ctl) => ctl,
        Err(error) => {
            evidence("fault-failed", json!({"error": error.0}));
            return;
        }
    };
    kit::pause(args.fault_at - started.elapsed().as_secs_f64());
    let nodes: Vec<&str> = args.nodes.iter().map(String::as_str).collect();
    let mut pace = nemesis::Sleep;
    let applied = match args.fault {
        Fault::None => return,
        Fault::Pause => {
            let hold = args.duration - args.fault_at - args.pause_length;
            let mut rng = Rng::stream(args.seed, 7);
            if let Err(error) = nemesis::random_pauses(
                &mut ctl,
                &mut pace,
                &mut rng,
                &nodes,
                hold,
                args.pause_length,
                args.pause_gap,
                |_| json!({}),
            ) {
                evidence("fault-failed", json!({"error": error.0}));
            }
            return;
        }
        Fault::Crash => nemesis::crash_all(&mut ctl, &mut pace, &nodes, args.fault_down, json!({})),
        Fault::Kill => nemesis::step(&mut ctl, "stop", &nodes).and_then(|()| {
            nemesis::Pace::wait(&mut pace, &mut ctl, args.fault_down);
            nemesis::step(&mut ctl, "restart", &nodes)
        }),
    };
    if let Err(error) = applied {
        evidence("fault-failed", json!({"error": error.0}));
        return;
    }
    let deadline = Instant::now() + Duration::from_secs_f64(args.ready_timeout);
    if let Err(error) = form_cluster(args, args.fault.name(), deadline) {
        evidence("recovery-failed", json!({"error": error}));
    }
}

// ------------------------------------------------------------ checks

/// The verdict over the whole history and each node's final read.
fn check(run: &Run, finals: &[(String, Option<Vec<Mop>>)]) -> (u8, String) {
    let txns = run.txns.lock().unwrap().clone();
    let committed = txns.iter().filter(|t| elle::kind_of(t) == "ok").count();
    let result = match report(&txns, &run.args.model, run.args.examples) {
        Ok(result) => result,
        Err(error) => return (kit::INCONCLUSIVE, format!("checker: {error}")),
    };
    let (stale, lost) = durability(&txns, finals);
    evidence(
        "stale-reads",
        json!({"count": stale.len(), "examples": stale.iter().take(run.args.examples).collect::<Vec<_>>()}),
    );
    let mut lost_counts = BTreeMap::new();
    for (node, lost) in &lost {
        evidence(
            "durability",
            json!({"node": node, "lost": lost.as_ref().map(Vec::len),
                   "examples": lost.iter().flatten().take(run.args.examples).collect::<Vec<_>>()}),
        );
        lost_counts.insert(node.clone(), lost.as_ref().map(Vec::len));
    }
    let agree = finals.windows(2).all(|pair| pair[0].1 == pair[1].1);
    evidence("final-reads-agree", json!({"agree": agree}));
    let any_lost = lost_counts.values().any(|n| n.is_some_and(|n| n > 0));
    let all_read = finals.iter().all(|(_, f)| f.is_some());
    let valid = result.valid && stale.is_empty() && !any_lost;
    let types = json!(result.txns);
    if valid && (committed <= 1 || !all_read) {
        let reason = if committed <= 1 {
            "no transaction committed"
        } else {
            "a final read failed"
        };
        return (kit::INCONCLUSIVE, format!("{reason}: {types}"));
    }
    let word = if valid { "valid" } else { "INVALID" };
    let text = format!(
        "{word} append checked as {}: {} stale {} lost {} agree {agree} txns {types}",
        run.args.model,
        json!(result.counts),
        stale.len(),
        json!(lost_counts),
    );
    (if valid { kit::OK } else { kit::VIOLATION }, text)
}

/// Stale reads among the clients' transactions, and per node the
/// acknowledged appends its final read lacks.
#[allow(clippy::type_complexity)]
pub fn durability(
    txns: &[TxnOp],
    finals: &[(String, Option<Vec<Mop>>)],
) -> (Vec<Value>, Vec<(String, Option<Vec<Value>>)>) {
    let is_final =
        |t: &TxnOp| matches!(&t.op.process, Some(Atom::Str(p)) if p.starts_with("final"));
    let stale = realtime::stale_reads(txns, |t| !is_final(t));
    let clients: Vec<TxnOp> = txns.iter().filter(|t| !is_final(t)).cloned().collect();
    let lost = finals
        .iter()
        .map(|(node, f)| {
            (
                node.clone(),
                f.as_ref().map(|f| my::lost_appends(&clients, f)),
            )
        })
        .collect();
    (stale, lost)
}

pub fn run(args: Args) -> u8 {
    kit::start_clock();
    evidence(
        "config",
        json!({"nodes": args.nodes, "client_nodes": args.client_nodes, "read_only": args.read_only,
               "isolation": args.isolation.name(), "model_checked": args.model,
               "sync_wait": args.sync_wait, "clients": args.clients, "duration": args.duration,
               "tables": args.tables, "key_count": args.key_count,
               "max_writes_per_key": args.max_writes_per_key, "max_txn_length": args.max_txn_length,
               "read_fraction": args.read_fraction, "mop_sleep_ms": args.mop_sleep_ms,
               "timeout": args.timeout, "fault": args.fault.name(), "fault_at": args.fault_at,
               "fault_down": args.fault_down, "pause_length": args.pause_length,
               "pause_gap": args.pause_gap, "seed": args.seed, "label": args.label}),
    );
    let deadline = Instant::now() + Duration::from_secs_f64(args.ready_timeout);
    if let Err(error) = form_cluster(&args, "first-boot", deadline) {
        return verdict(
            kit::INCONCLUSIVE,
            &format!("the cluster never formed: {error}"),
        );
    }
    if let Err(error) = schema(&args, deadline) {
        return verdict(kit::INCONCLUSIVE, &format!("schema setup failed: {error}"));
    }
    let servers: serde_json::Map<String, Value> = args
        .nodes
        .iter()
        .map(|n| (n.clone(), server_info(n, args.port)))
        .collect();
    evidence("ready", json!({"servers": servers}));
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
        stop_at: started + Duration::from_secs_f64(args.duration),
        args: args.clone(),
    });
    let nemesis = (args.fault != Fault::None).then(|| {
        let args = args.clone();
        thread::spawn(move || fault_schedule(&args, started))
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
    // Jepsen's final generator: heal, wait for every node, then read each.
    if let Ok(mut ctl) = kit::connect_world(None) {
        ctl.heal_everything();
    }
    let deadline = Instant::now() + Duration::from_secs_f64(args.ready_timeout);
    for node in &args.nodes {
        if let Err(error) = wait_synced(node, args.port, args.nodes.len() as i64, deadline) {
            evidence("final-not-synced", json!({"node": node, "error": error}));
        }
    }
    let finals: Vec<(String, Option<Vec<Mop>>)> = args
        .nodes
        .iter()
        .map(|node| {
            let value = run.final_read(node);
            if let Some(value) = &value {
                let t = now_us();
                run.record_txn(
                    json!(format!("final-{node}")),
                    "ok",
                    value,
                    Some(value.clone()),
                    t,
                    t,
                    None,
                );
            }
            (node.clone(), value)
        })
        .collect();
    let (code, text) = check(&run, &finals);
    verdict(code, &text)
}

/// Re-check a saved oracle log against `model`.
pub fn recheck(text: &str, model: &str) -> Result<(u8, Value), String> {
    let lines = kit::tagged_lines(text, "H ");
    let txns = elle::from_h_lines(&lines)?;
    let result = elle::check(&txns, model)?;
    let finals: Vec<(String, Option<Vec<Mop>>)> = txns
        .iter()
        .filter_map(|t| match (&t.op.process, &t.outcome) {
            (Some(Atom::Str(p)), wgl::OpOutcome::Completed { ret, .. })
                if p.starts_with("final-") =>
            {
                Some((p["final-".len()..].to_string(), Some(ret.clone())))
            }
            _ => None,
        })
        .collect();
    let (stale, lost) = durability(&txns, &finals);
    let mut out = result.to_json();
    out["stale-reads"] = json!(stale.len());
    out["stale-read-examples"] = json!(stale.iter().take(5).collect::<Vec<_>>());
    out["lost"] = json!(lost
        .iter()
        .map(|(n, l)| (n.clone(), json!(l.as_ref().map(Vec::len))))
        .collect::<serde_json::Map<_, _>>());
    let any_lost = lost
        .iter()
        .any(|(_, l)| l.as_ref().is_some_and(|l| !l.is_empty()));
    let valid = result.valid && stale.is_empty() && !any_lost;
    Ok((if valid { kit::OK } else { kit::VIOLATION }, out))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bootstrap_from_the_most_advanced_node() {
        let r = |fresh: bool, seqno: i64, safe: i64| json!({"fresh": fresh, "seqno": seqno, "grastate_seqno": -1, "safe_to_bootstrap": safe});
        assert_eq!(
            choose_bootstrap(&[r(true, -1, 0), r(true, -1, 0), r(true, -1, 0)]),
            0
        );
        assert_eq!(
            choose_bootstrap(&[r(false, 10, 0), r(false, 12, 0), r(false, 12, 0)]),
            1
        );
        assert_eq!(
            choose_bootstrap(&[r(false, 12, 0), r(false, 12, 1), r(false, 3, 0)]),
            1
        );
        // A node that lost its data dir entirely never wins over a recovered one.
        assert_eq!(
            choose_bootstrap(&[r(true, -1, 0), r(false, 0, 0), r(false, -1, 0)]),
            1
        );
    }

    #[test]
    fn finals_are_neither_stale_readers_nor_their_own_loss() {
        let ok = |id: i64, p: &str, value: Vec<Mop>, t0: u64, t1: u64| {
            elle::txn(id, "ok", value, Some(Atom::from(p)), t0, t1, None).unwrap()
        };
        let txns = [
            ok(0, "1", vec![Mop::Append(1.into(), 1)], 0, 10),
            ok(1, "2", vec![Mop::Append(1.into(), 2)], 0, 11),
            ok(
                2,
                "final-n1",
                vec![Mop::Read(1.into(), Some(vec![1]))],
                20,
                20,
            ),
        ];
        let finals = vec![
            (
                "n1".to_string(),
                Some(vec![Mop::Read(1.into(), Some(vec![1]))]),
            ),
            (
                "n2".to_string(),
                Some(vec![Mop::Read(1.into(), Some(vec![1, 2]))]),
            ),
        ];
        let (stale, lost) = durability(&txns, &finals);
        assert!(stale.is_empty());
        assert_eq!(lost[0].1.as_ref().map(Vec::len), Some(1));
        assert_eq!(lost[1].1.as_ref().map(Vec::len), Some(0));
    }
}
