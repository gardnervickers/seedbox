//! The etcd register oracle: Jepsen's etcd register workload over etcd's v3
//! JSON gateway, from client threads in the oracle itself. Each client picks
//! one of a few keys in use and a random member, and reads the key (a range,
//! linearizable unless `--reads serializable`), writes a small random value
//! (a put), or compares-and-sets it (a txn comparing the value, putting the
//! new one on success). A key is retired after `--ops-per-key` invocations and
//! replaced by a fresh one, so that every key's history stays small enough
//! for an exhaustive linearizability search.
//!
//! A read that fails has no effect; a put or txn without a reply (a timeout,
//! a dropped connection, any gRPC error) may or may not have taken effect and
//! stays pending; one whose connection was refused was never sent. Every
//! key's history must be linearizable ([`RegisterOf`]). Serializable reads may
//! be stale by design, so they stay out of that check; each must still return
//! a value some write could have produced.
//!
//! Revisions give two more checks. Every acknowledged write and every read
//! names a revision of its key: two that name the same revision with
//! different values mean members applied different histories. And for
//! linearizable operations, a revision lower than one a completed operation
//! on the same key already saw is a stale read or a lost write. A watch of
//! every key on each member, resumed from the next revision whenever its
//! stream breaks, must see every revision exactly once and in order (every
//! revision is a register write), with the same values.
//!
//! Once the writes end, every member has to serve a linearizable read again
//! within `--recover` seconds (a member that dies for good fails the run),
//! every key gets a final read, and every member's `HashKV` at a common
//! revision has to agree.
use super::etcd_gateway::{b64, number, unb64, Watch, CLIENT_PORT};
use crate::checker::models::{verdict_word, RegisterOp, RegisterRet};
use crate::kit::{self, http, Rng};
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    sync::{
        atomic::{AtomicI64, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};

#[derive(clap::ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reads {
    /// Linearizable ranges (etcd's default), checked with the writes.
    Linearizable,
    /// Serializable ranges, served from the member's local state.
    Serializable,
}

#[derive(clap::Args, Debug, Clone)]
pub struct Args {
    /// The etcd members, in order.
    #[arg(long, value_delimiter = ',', default_value = "n1,n2,n3,n4,n5")]
    pub members: Vec<String>,
    /// Seconds of operations.
    #[arg(long, default_value_t = 60)]
    pub duration: u64,
    /// Client threads.
    #[arg(long, default_value_t = 10)]
    pub clients: usize,
    /// Keys in use at once.
    #[arg(long, default_value_t = 4)]
    pub keys: usize,
    /// Invocations on a key before it is replaced by a fresh one.
    #[arg(long, default_value_t = 200)]
    pub ops_per_key: usize,
    /// Values are drawn from 0 to this, exclusive.
    #[arg(long, default_value_t = 5)]
    pub values: u64,
    /// Pause between a client's operations, in milliseconds.
    #[arg(long, default_value_t = 10)]
    pub think_ms: u64,
    /// Seconds a request may take.
    #[arg(long, default_value_t = 5.0)]
    pub timeout: f64,
    #[arg(long, value_enum, default_value_t = Reads::Linearizable)]
    pub reads: Reads,
    /// A member whose etcd refuses to start after the faults (exits again
    /// after a power cycle) does not fail the run: for profiles that corrupt
    /// or fail its disk, where etcd stopping is the safe outcome.
    #[arg(long)]
    pub tolerate_exits: bool,
    /// Seconds after the final reads that each member's watch has to catch
    /// up to the last revision; 0 runs no watchers.
    #[arg(long, default_value_t = 30)]
    pub watch: u64,
    /// Power-fail every member together about this often during the
    /// operations, in seconds; 0 never does.
    #[arg(long, default_value_t = 0.0)]
    pub crash_all_every: f64,
    /// Seconds the members stay down after a coordinated power failure.
    #[arg(long, default_value_t = 2.0)]
    pub crash_all_down: f64,
    /// Seconds to wait after the operations (no new generated fault starts)
    /// before the recovery check.
    #[arg(long, default_value_t = 15)]
    pub quiet: u64,
    /// Seconds after the quiet period that every member has to serve a
    /// linearizable read again; one that never does fails the run.
    #[arg(long, default_value_t = 120)]
    pub recover: u64,
}

/// What one gateway call came back with.
#[derive(Debug)]
pub(crate) enum Reply {
    Ok(Value),
    /// The connection was refused or timed out: nothing was sent.
    NotSent(String),
    /// An HTTP error status with the gateway's error message.
    Error(u16, String),
    /// Sent, but no complete reply.
    Lost(String),
}

pub(crate) fn call(member: &str, path: &str, body: &Value, timeout: f64) -> Reply {
    let timeout = Duration::from_secs_f64(timeout);
    let stream = match http::connect(member, CLIENT_PORT, timeout) {
        Ok(stream) => stream,
        Err(error) => return Reply::NotSent(error.0),
    };
    let data = body.to_string();
    match http::exchange(
        stream,
        "POST",
        &format!("{member}:{CLIENT_PORT}"),
        path,
        Some(("application/json", data.as_bytes())),
    ) {
        Ok((200, reply)) => match serde_json::from_slice(&reply) {
            Ok(value) => Reply::Ok(value),
            Err(error) => Reply::Lost(format!("unparseable reply: {error}")),
        },
        Ok((status, reply)) => {
            let message = serde_json::from_slice::<Value>(&reply)
                .ok()
                .and_then(|v| {
                    v["message"]
                        .as_str()
                        .or(v["error"].as_str())
                        .map(str::to_owned)
                })
                .unwrap_or_else(|| String::from_utf8_lossy(&reply[..reply.len().min(120)]).into());
            Reply::Error(status, message)
        }
        Err(error) => Reply::Lost(error.0),
    }
}

impl Reply {
    /// A short label for error tallies.
    pub(crate) fn label(&self) -> String {
        match self {
            Reply::Ok(_) => "ok".into(),
            Reply::NotSent(error) if error.contains("refused") => "not sent: refused".into(),
            Reply::NotSent(error) if error.contains("timed out") => "not sent: timeout".into(),
            Reply::NotSent(error) => format!("not sent: {error}"),
            Reply::Error(status, message) => format!("HTTP {status} {message}"),
            Reply::Lost(error) => {
                if error.contains("timed out") || error.contains("Resource temporarily") {
                    "lost: timeout".into()
                } else {
                    format!("lost: {error}")
                }
            }
        }
    }
}

type KeyHistory = wgl::History<RegisterOp<u8>, RegisterRet<u8>>;

/// One key's record.
#[derive(Default)]
struct KeyLog {
    history: KeyHistory,
    invoked: usize,
    /// Value by revision, from acknowledged writes and reads, with where
    /// each came from.
    versions: BTreeMap<i64, (Option<u8>, String)>,
    /// The highest revision a completed linearizable operation saw.
    floor: i64,
    /// Every value a write or compare-and-set was sent for, by sequence.
    attempted: Vec<(u64, u8)>,
}

#[derive(Default)]
struct Book {
    /// The key each slot uses now.
    slots: Vec<usize>,
    keys: Vec<KeyLog>,
    /// A global order of invocations and completions.
    seq: u64,
    /// Members applied different values at one revision.
    conflicts: Vec<Value>,
    /// A linearizable operation saw a revision below one already seen.
    stale: Vec<Value>,
    /// Serializable reads of a value no write could have produced yet.
    phantom: Vec<Value>,
    /// Serializable reads below the key's floor (allowed; counted).
    stale_serializable: u64,
    unparseable: Vec<Value>,
    outcomes: BTreeMap<String, u64>,
}

impl Book {
    fn new(slots: usize) -> Self {
        Self {
            slots: (0..slots).collect(),
            keys: (0..slots).map(|_| KeyLog::default()).collect(),
            ..Self::default()
        }
    }

    /// The key slot `slot` uses for its next invocation, rotating a full one.
    fn key_for(&mut self, slot: usize, limit: usize) -> usize {
        let key = self.slots[slot];
        if self.keys[key].invoked < limit {
            return key;
        }
        self.keys.push(KeyLog::default());
        self.slots[slot] = self.keys.len() - 1;
        self.keys.len() - 1
    }

    fn tick(&mut self) -> u64 {
        self.seq += 1;
        self.seq
    }

    /// Record that `key` held `value` at `revision`, as `source` saw it.
    fn version(&mut self, key: usize, revision: i64, value: Option<u8>, source: String) {
        if revision <= 0 {
            return;
        }
        match self.keys[key].versions.get(&revision) {
            Some((seen, by)) if *seen != value => {
                let by = by.clone();
                self.conflicts
                    .push(json!({"key": key_name(key), "revision": revision,
                    "first": [seen, by], "then": [value, source]}));
            }
            Some(_) => {}
            None => {
                self.keys[key].versions.insert(revision, (value, source));
            }
        }
    }
}

fn key_name(key: usize) -> String {
    format!("r{key}")
}

/// The value stored for a register value.
fn encode(value: u8) -> String {
    b64(&value.to_string())
}

/// A stored value, or `Err` with the text when no client could have written it.
fn decode(kv: &Value) -> Result<Option<u8>, String> {
    if kv.is_null() {
        return Ok(None);
    }
    let text = unb64(kv["value"].as_str().unwrap_or(""));
    text.parse::<u8>().map(Some).map_err(|_| text)
}

/// One operation, recorded in the book.
struct Op {
    key: usize,
    op: RegisterOp<u8>,
    member: String,
    serializable: bool,
}

fn run_op(book: &Mutex<Book>, args: &Args, op: Op) {
    let name = key_name(op.key);
    let (id, floor) = {
        let mut b = book.lock().unwrap();
        let seq = b.tick();
        let log = &mut b.keys[op.key];
        log.invoked += 1;
        match op.op {
            RegisterOp::Write(v) | RegisterOp::Cas(_, v) => log.attempted.push((seq, v)),
            RegisterOp::Read => {}
        }
        let floor = log.floor;
        let id = (!op.serializable).then(|| log.history.invoke(op.op.clone()));
        (id, floor)
    };
    let (path, body, kind) = match op.op {
        RegisterOp::Read => (
            "/v3/kv/range",
            json!({"key": b64(&name), "serializable": op.serializable}),
            if op.serializable { "sread" } else { "read" },
        ),
        RegisterOp::Write(v) => (
            "/v3/kv/put",
            json!({"key": b64(&name), "value": encode(v)}),
            "write",
        ),
        RegisterOp::Cas(from, to) => (
            "/v3/kv/txn",
            json!({
                "compare": [{"key": b64(&name), "target": "VALUE", "result": "EQUAL", "value": encode(from)}],
                "success": [{"request_put": {"key": b64(&name), "value": encode(to)}}],
            }),
            "cas",
        ),
    };
    let reply = call(&op.member, path, &body, args.timeout);
    let mut b = book.lock().unwrap();
    let completed = b.tick();
    *b.outcomes
        .entry(format!("{kind} {}", reply.label()))
        .or_default() += 1;
    let source = format!("{kind} via {} at {:.3}", op.member, kit::now());
    match (&op.op, reply) {
        (RegisterOp::Read, Reply::Ok(value)) => {
            let kv = &value["kvs"][0];
            let revision = if kv.is_null() {
                0
            } else {
                number(&kv["mod_revision"])
            };
            match decode(kv) {
                Ok(seen) => {
                    if let Some(id) = id {
                        b.keys[op.key]
                            .history
                            .complete(id, RegisterRet::Value(seen));
                    }
                    b.version(op.key, revision, seen, source.clone());
                    if op.serializable {
                        // Some write sent before this read completed must
                        // have been able to produce the value.
                        let possible = seen.is_none_or(|v| {
                            b.keys[op.key]
                                .attempted
                                .iter()
                                .any(|(at, w)| *w == v && *at < completed)
                        });
                        if !possible {
                            b.phantom
                                .push(json!({"key": name, "value": seen, "read": source}));
                        }
                        if revision < floor {
                            b.stale_serializable += 1;
                        }
                    } else {
                        if revision < floor {
                            b.stale.push(json!({"key": name, "read": source, "revision": revision, "floor": floor}));
                        }
                        let log = &mut b.keys[op.key];
                        log.floor = log.floor.max(revision);
                    }
                }
                Err(text) => {
                    if let Some(id) = id {
                        b.keys[op.key]
                            .history
                            .complete(id, RegisterRet::Value(Some(u8::MAX)));
                    }
                    b.unparseable
                        .push(json!({"key": name, "text": text, "read": source}));
                }
            }
        }
        // A failed read has no effect.
        (RegisterOp::Read, _) => {
            if let Some(id) = id {
                b.keys[op.key].history.fail(id);
            }
        }
        (RegisterOp::Write(v), Reply::Ok(value)) => {
            let revision = number(&value["header"]["revision"]);
            if let Some(id) = id {
                b.keys[op.key].history.complete(id, RegisterRet::Wrote);
            }
            write_revision(&mut b, op.key, revision, floor, *v, source);
        }
        (RegisterOp::Cas(_, to), Reply::Ok(value)) => {
            let succeeded = value["succeeded"].as_bool().unwrap_or(false);
            if let Some(id) = id {
                b.keys[op.key]
                    .history
                    .complete(id, RegisterRet::Cas(succeeded));
            }
            if succeeded {
                let revision = number(&value["header"]["revision"]);
                write_revision(&mut b, op.key, revision, floor, *to, source);
            }
        }
        // Never sent: no effect.
        (_, Reply::NotSent(_)) => {
            if let Some(id) = id {
                b.keys[op.key].history.fail(id);
            }
        }
        // Anything else may or may not have taken effect: leave it pending.
        _ => {}
    }
}

/// An acknowledged write at `revision`: it names that revision's value, and
/// must be newer than anything a completed operation saw before it began.
fn write_revision(b: &mut Book, key: usize, revision: i64, floor: i64, value: u8, source: String) {
    b.version(key, revision, Some(value), source.clone());
    if revision <= floor {
        b.stale.push(
            json!({"key": key_name(key), "write": source, "revision": revision, "floor": floor}),
        );
    }
    let log = &mut b.keys[key];
    log.floor = log.floor.max(revision);
}

/// A linearizable read of `key` through `member`.
fn read_probe(member: &str, key: &str, timeout: f64) -> Reply {
    call(member, "/v3/kv/range", &json!({"key": b64(key)}), timeout)
}

/// The outcome of a recovery wait.
pub(crate) struct Recovery {
    pub live: Vec<String>,
    pub down: Vec<String>,
    /// Members whose etcd had exited halfway through and that were power
    /// cycled to start it again.
    pub restarted: Vec<String>,
    /// Members still down whose etcd had exited again by the end: they
    /// refuse to start (etcd fails stop on a corrupt WAL or snapshot).
    pub exited: Vec<String>,
}

/// Each machine's application state as the World reports it (`Running`,
/// `Exited(1)`, ...).
fn states(ctl: &mut kit::Ctl, label: &str) -> BTreeMap<String, String> {
    let Ok(Some(world)) = ctl.inspect() else {
        return BTreeMap::new();
    };
    let states = world
        .get("machines")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|m| {
            Some((
                m["name"].as_str()?.to_string(),
                m["state"].as_str()?.to_string(),
            ))
        })
        .collect::<BTreeMap<_, _>>();
    kit::evidence("states", json!({"label": label, "states": states}));
    states
}

/// Wait up to `seconds` for every member to serve a linearizable read, and
/// split them into those that did and those that never did. With `restart`,
/// halfway through, power-cycle every member still not serving whose etcd
/// has exited, as a service manager would start it again (etcd exits on a
/// disk error by design; the World restarts only processes it stopped). A
/// member that still never serves fails the run. The World connection opens
/// only then: the guest serves one control client at a time, and the quiet
/// request and power-loss schedule need it too.
pub(crate) fn recover(members: &[String], seconds: u64, label: &str, restart: bool) -> Recovery {
    let start = Instant::now();
    let mut waiting = members.to_vec();
    let mut last = BTreeMap::new();
    let mut restarted = Vec::new();
    let mut restart = restart;
    while !waiting.is_empty() {
        waiting.retain(|member| match read_probe(member, "probe", 5.0) {
            Reply::Ok(_) => false,
            other => {
                last.insert(member.clone(), other.label());
                true
            }
        });
        if waiting.is_empty() || start.elapsed() >= Duration::from_secs(seconds) {
            break;
        }
        if restart && start.elapsed() >= Duration::from_secs(seconds / 2) {
            restart = false;
            if let Ok(mut ctl) = kit::connect_world(None) {
                let states = states(&mut ctl, label);
                for member in &waiting {
                    let state = states.get(member).cloned().unwrap_or_default();
                    if !state.starts_with("Exited") {
                        continue;
                    }
                    let result = ctl.crash(member).and_then(|()| ctl.reboot(member));
                    kit::evidence(
                        "power-cycle",
                        json!({"label": label, "member": member, "state": state,
                               "last": last.get(member),
                               "error": result.as_ref().err().map(|e| e.0.clone())}),
                    );
                    if result.is_ok() {
                        restarted.push(member.clone());
                    }
                }
            }
        }
        std::thread::sleep(Duration::from_secs(1));
    }
    let mut exited = Vec::new();
    if !waiting.is_empty() {
        if let Ok(mut ctl) = kit::connect_world(None) {
            let states = states(&mut ctl, label);
            exited = waiting
                .iter()
                .filter(|m| states.get(*m).is_some_and(|s| s.starts_with("Exited")))
                .cloned()
                .collect();
        }
    }
    for member in &waiting {
        kit::evidence(
            "unrecovered",
            json!({"label": label, "member": member, "waited": start.elapsed().as_secs_f64(),
                   "last": last.get(member), "exited": exited.contains(member)}),
        );
    }
    let live = members
        .iter()
        .filter(|m| !waiting.contains(m))
        .cloned()
        .collect();
    Recovery {
        live,
        down: waiting,
        restarted,
        exited,
    }
}

/// The verdict text for members that did not recover, and whether that
/// violates the check: one that refuses to start is tolerated only with
/// `tolerate_exits` (its disk took injected faults).
pub(crate) fn recovery_text(r: &Recovery, tolerate_exits: bool) -> (String, bool) {
    let mut text = String::new();
    if !r.restarted.is_empty() {
        text.push_str(&format!(
            "; {} power-cycled after etcd exited",
            r.restarted.join(", ")
        ));
    }
    let hung: Vec<&String> = r.down.iter().filter(|m| !r.exited.contains(m)).collect();
    if !r.exited.is_empty() {
        text.push_str(&format!(
            "; {} refused to start (etcd exited){}",
            r.exited.join(", "),
            if tolerate_exits {
                ", tolerated after disk faults"
            } else {
                ""
            }
        ));
    }
    // Without a majority of running members nobody can serve a
    // linearizable read: the hung ones are then excused.
    let members = r.live.len() + r.down.len();
    let quorum = members - r.exited.len() > members / 2;
    if !hung.is_empty() {
        let names: Vec<&str> = hung.iter().map(|m| m.as_str()).collect();
        text.push_str(&format!(
            "; {} running but never served a read again after the faults ended{}",
            names.join(", "),
            if quorum { "" } else { " (no quorum left)" }
        ));
    }
    let violated = (!hung.is_empty() && (quorum || !tolerate_exits))
        || (!r.exited.is_empty() && !tolerate_exits);
    (text, violated)
}

/// Every live member's `HashKV` at the lowest revision any of them reports:
/// members that applied the same history agree. Returns a description of a
/// disagreement, or of why there was nothing to compare.
pub(crate) fn compare_hashes(live: &[String]) -> (Option<String>, Value) {
    let mut revisions = BTreeMap::new();
    // Applied raft index beside each revision: a member that applied as many
    // entries as the others but holds fewer revisions skipped some.
    let mut applied = BTreeMap::new();
    for member in live {
        for _ in 0..5 {
            if let Reply::Ok(status) = call(member, "/v3/maintenance/status", &json!({}), 5.0) {
                revisions.insert(member.clone(), number(&status["header"]["revision"]));
                applied.insert(member.clone(), number(&status["raftAppliedIndex"]));
                break;
            }
            kit::pause(1.0);
        }
    }
    let Some(&revision) = revisions.values().min() else {
        return (None, json!({"skipped": "no member status"}));
    };
    let mut hashes = BTreeMap::new();
    for member in revisions.keys() {
        for _ in 0..5 {
            match call(
                member,
                "/v3/maintenance/hashkv",
                &json!({"revision": revision.to_string()}),
                10.0,
            ) {
                Reply::Ok(reply) => {
                    hashes.insert(member.clone(), number(&reply["hash"]));
                    break;
                }
                _ => kit::pause(1.0),
            }
        }
    }
    let details = json!({"revision": revision, "revisions": revisions, "applied": applied,
                         "hashes": hashes});
    let distinct: std::collections::BTreeSet<_> = hashes.values().collect();
    if distinct.len() > 1 {
        (
            Some(format!(
                "members' HashKV at revision {revision} differ: {hashes:?}"
            )),
            details,
        )
    } else {
        (None, details)
    }
}

/// What one member's watch of every register key saw: each event's
/// revision, key and value, reconnecting from the next revision whenever
/// the stream breaks.
struct WatchLog {
    member: String,
    events: Vec<(i64, String, String)>,
    reconnects: u64,
}

/// Watch the register keys on `member` from revision 1 until `target` (0
/// while unknown) is reached or `deadline` (0 while unknown, else seconds
/// on the oracle clock) passes.
fn watcher(member: String, target: Arc<AtomicI64>, deadline: Arc<AtomicI64>) -> WatchLog {
    let mut log = WatchLog {
        member: member.clone(),
        events: Vec::new(),
        reconnects: 0,
    };
    let done = |last: i64| {
        let t = target.load(Ordering::SeqCst);
        let d = deadline.load(Ordering::SeqCst);
        (t > 0 && last >= t) || (d > 0 && kit::now() as i64 >= d)
    };
    loop {
        let last = log.events.last().map_or(0, |e| e.0);
        if done(last) {
            return log;
        }
        let mut watch = match Watch::open(&member, "r", "s", Some(last + 1), 5.0) {
            Ok(watch) => watch,
            Err(_) => {
                log.reconnects += 1;
                kit::pause(1.0);
                continue;
            }
        };
        // Ends with an error when the stream breaks or idles for 5 s.
        let _ = watch.read_until(|w| done(w.events.last().map_or(last, |e| e.mod_revision)));
        log.reconnects += 1;
        log.events.extend(
            watch
                .events
                .into_iter()
                .map(|e| (e.mod_revision, e.key, e.value)),
        );
    }
}

/// Check every watch against the book: each must see every revision from 2
/// to the last one it saw exactly once and in order (every revision is a
/// register write), with the value the writers and readers saw.
fn check_watches(b: &mut Book, logs: &[WatchLog], target: i64) -> (Vec<Value>, Value) {
    let mut problems = Vec::new();
    let mut summary = serde_json::Map::new();
    for log in logs {
        let mut expected = 2;
        let mut gaps = 0;
        for (revision, key, value) in &log.events {
            if *revision != expected {
                gaps += 1;
                if problems.len() < 10 {
                    problems.push(json!({"member": log.member, "expected": expected, "saw": revision, "key": key}));
                }
            }
            expected = revision + 1;
            let parsed = key.strip_prefix('r').and_then(|k| k.parse::<usize>().ok());
            match parsed {
                Some(k) if k < b.keys.len() => {
                    let seen = value.parse::<u8>().ok();
                    if seen.is_none() {
                        b.unparseable
                            .push(json!({"key": key, "text": value, "watch": log.member}));
                    }
                    b.version(k, *revision, seen, format!("watch via {}", log.member));
                }
                _ => problems
                    .push(json!({"member": log.member, "unknown_key": key, "revision": revision})),
            }
        }
        let last = log.events.last().map_or(0, |e| e.0);
        summary.insert(
            log.member.clone(),
            json!({"events": log.events.len(), "last": last, "gaps": gaps,
                   "reconnects": log.reconnects, "caught_up": last >= target}),
        );
    }
    (problems, Value::Object(summary))
}

pub fn run(args: Args) -> u8 {
    kit::start_clock();
    // The cluster forms once a quorum is up; wait for every member.
    let boot = recover(&args.members, 240, "boot", false);
    if !boot.down.is_empty() {
        return kit::verdict(
            kit::INCONCLUSIVE,
            &format!("members never served a read: {}", boot.down.join(", ")),
        );
    }
    let versions: BTreeMap<String, Value> = boot
        .live
        .iter()
        .filter_map(
            |m| match call(m, "/v3/maintenance/status", &json!({}), 5.0) {
                Reply::Ok(s) => Some((m.clone(), s["version"].clone())),
                _ => None,
            },
        )
        .collect();
    let seed = kit::rng::entropy_seed();
    kit::evidence(
        "start",
        json!({"workload": "register", "reads": format!("{:?}", args.reads), "versions": versions,
               "keys": args.keys, "clients": args.clients, "rng_seed": seed}),
    );
    seedbox_sdk::setup_complete(json!({"members": args.members, "workload": "register"}));
    if args.crash_all_every > 0.0 {
        crate::nemesis::crash_all_repeatedly(
            args.members.clone(),
            args.crash_all_every,
            args.crash_all_down,
            0.0,
            args.duration as f64,
        );
    }
    let book = Arc::new(Mutex::new(Book::new(args.keys)));
    let target = Arc::new(AtomicI64::new(0));
    let deadline = Arc::new(AtomicI64::new(0));
    let watchers: Vec<_> = args
        .members
        .iter()
        .filter(|_| args.watch > 0)
        .map(|member| {
            let (member, target, deadline) = (member.clone(), target.clone(), deadline.clone());
            std::thread::spawn(move || watcher(member, target, deadline))
        })
        .collect();
    let start = Instant::now();
    let workers: Vec<_> = (0..args.clients)
        .map(|client| {
            let (book, args) = (book.clone(), args.clone());
            std::thread::spawn(move || {
                let mut rng = Rng::stream(seed, client as u64);
                while start.elapsed() < Duration::from_secs(args.duration) {
                    let slot = rng.below(args.keys as u64) as usize;
                    let key = book.lock().unwrap().key_for(slot, args.ops_per_key);
                    let member = rng.choice(&args.members).clone();
                    let op = match rng.below(3) {
                        0 => RegisterOp::Read,
                        1 => RegisterOp::Write(rng.below(args.values) as u8),
                        _ => {
                            let from = rng.below(args.values) as u8;
                            RegisterOp::Cas(from, rng.below(args.values) as u8)
                        }
                    };
                    let serializable = op == RegisterOp::Read && args.reads == Reads::Serializable;
                    run_op(
                        &book,
                        &args,
                        Op {
                            key,
                            op,
                            member,
                            serializable,
                        },
                    );
                    std::thread::sleep(Duration::from_millis(args.think_ms));
                }
            })
        })
        .collect();
    for worker in workers {
        let _ = worker.join();
    }
    // No generated fault starts from here on; those active end on schedule.
    if kit::under_seedbox() {
        if let Err(error) = seedbox_sdk::control::quiet(Duration::from_secs(3600)) {
            kit::evidence("quiet-failed", json!({"error": error.to_string()}));
        }
    }
    {
        let b = book.lock().unwrap();
        kit::evidence(
            "progress",
            json!({"keys": b.keys.len(), "outcomes": b.outcomes}),
        );
    }
    kit::pause(args.quiet as f64);
    let recovery = recover(&args.members, args.recover, "final", true);
    let live = recovery.live.clone();
    // A final linearizable read of every key, retried on other members.
    let keys = book.lock().unwrap().keys.len();
    for key in (0..keys).filter(|_| !live.is_empty()) {
        'key: for attempt in 0..3u64 {
            for member in &live {
                run_op(
                    &book,
                    &args,
                    Op {
                        key,
                        op: RegisterOp::Read,
                        member: member.clone(),
                        serializable: false,
                    },
                );
                let answered = matches!(
                    book.lock().unwrap().keys[key]
                        .history
                        .operations()
                        .last()
                        .map(|o| &o.outcome),
                    Some(wgl::OpOutcome::Completed { .. })
                );
                if answered {
                    break 'key;
                }
            }
            std::thread::sleep(Duration::from_secs(1 + attempt));
        }
    }
    let (diverged, hashes) = compare_hashes(&live);
    let last = hashes["revisions"]
        .as_object()
        .into_iter()
        .flat_map(|m| m.values())
        .filter_map(Value::as_i64)
        .max()
        .unwrap_or(0);
    kit::evidence("hashkv", hashes);
    deadline.store(kit::now() as i64 + args.watch as i64, Ordering::SeqCst);
    target.store(last.max(1), Ordering::SeqCst);
    let logs: Vec<WatchLog> = watchers.into_iter().filter_map(|w| w.join().ok()).collect();
    let mut book = book.lock().unwrap();
    let (gaps, watches) = check_watches(&mut book, &logs, last);
    kit::evidence(
        "watches",
        json!({"target": last, "members": watches, "gaps": gaps}),
    );
    let (recovered, unrecovered) = recovery_text(&recovery, args.tolerate_exits);
    conclude(&book, &recovered, unrecovered, diverged, &gaps)
}

fn conclude(
    b: &Book,
    recovered: &str,
    unrecovered: bool,
    diverged: Option<String>,
    gaps: &[Value],
) -> u8 {
    let (mut ops, mut pending) = (0, 0);
    for log in &b.keys {
        let operations = log.history.operations();
        ops += operations.len();
        pending += operations
            .iter()
            .filter(|o| matches!(o.outcome, wgl::OpOutcome::Pending))
            .count();
    }
    kit::evidence(
        "histories",
        json!({"keys": b.keys.len(), "ops": ops, "pending": pending}),
    );
    let mut violations = Vec::new();
    let mut exhausted = Vec::new();
    for (key, log) in b.keys.iter().enumerate() {
        let operations = log.history.operations();
        let verdict = crate::checker::models::check_register(&key.to_string(), operations);
        match verdict {
            wgl::Verdict::Linearizable => {}
            wgl::Verdict::NotLinearizable(e) => {
                let blocked = e.blocked_on.map(|i| {
                    let o = &operations[i];
                    format!("#{i} {:?} -> {:?}", o.op, o.outcome)
                });
                let history: Vec<String> = operations
                    .iter()
                    .enumerate()
                    .map(|(i, o)| format!("#{i} {:?} -> {:?}", o.op, o.outcome))
                    .collect();
                kit::evidence(
                    "nonlinearizable",
                    json!({"key": key_name(key), "blocked_on": blocked,
                           "linearized": e.longest_prefix.len(), "ops": operations.len(),
                           "history": history}),
                );
                violations.push(key_name(key));
            }
            wgl::Verdict::BudgetExhausted(_) => {
                kit::evidence(
                    "undecided",
                    json!({"key": key_name(key), "verdict": verdict_word(&verdict),
                           "ops": operations.len()}),
                );
                exhausted.push(key_name(key));
            }
        }
    }
    kit::evidence(
        "details",
        json!({"conflicts": b.conflicts.iter().take(10).collect::<Vec<_>>(),
               "stale": b.stale.iter().take(10).collect::<Vec<_>>(),
               "phantom": b.phantom.iter().take(10).collect::<Vec<_>>(),
               "unparseable": b.unparseable.iter().take(10).collect::<Vec<_>>(),
               "stale_serializable": b.stale_serializable}),
    );
    let mut summary = format!(
        "{} keys, {ops} ops ({pending} pending): {} not linearizable, {} undecided; {} revision conflicts, {} stale, {} phantom serializable reads, {} unparseable values",
        b.keys.len(),
        violations.len(),
        exhausted.len(),
        b.conflicts.len(),
        b.stale.len(),
        b.phantom.len(),
        b.unparseable.len(),
    );
    if let Some(diverged) = &diverged {
        summary.push_str(&format!("; {diverged}"));
    }
    if !gaps.is_empty() {
        summary.push_str(&format!(
            "; watches skipped or reordered revisions: {gaps:?}"
        ));
    }
    summary.push_str(recovered);
    let violated = !violations.is_empty()
        || !gaps.is_empty()
        || !b.conflicts.is_empty()
        || !b.stale.is_empty()
        || !b.phantom.is_empty()
        || !b.unparseable.is_empty()
        || diverged.is_some()
        || unrecovered;
    if violated {
        kit::verdict(kit::VIOLATION, &summary)
    } else if !exhausted.is_empty() || ops == 0 {
        kit::verdict(kit::INCONCLUSIVE, &summary)
    } else {
        kit::verdict(kit::OK, &summary)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_rotate_after_their_invocations() {
        let mut book = Book::new(2);
        assert_eq!(book.key_for(1, 2), 1);
        book.keys[1].invoked = 2;
        assert_eq!(book.key_for(1, 2), 2);
        assert_eq!(book.key_for(0, 2), 0);
        assert_eq!(book.slots, [0, 2]);
    }

    #[test]
    fn revisions_with_two_values_conflict() {
        let mut book = Book::new(1);
        book.version(0, 5, Some(1), "write".into());
        book.version(0, 5, Some(1), "read".into());
        assert!(book.conflicts.is_empty());
        book.version(0, 5, Some(2), "read".into());
        assert_eq!(book.conflicts.len(), 1);
        // Revision 0 is an absent key, not a version.
        book.version(0, 0, None, "read".into());
        assert_eq!(book.conflicts.len(), 1);
    }

    #[test]
    fn values_decode_or_are_reported() {
        assert_eq!(decode(&Value::Null), Ok(None));
        assert_eq!(decode(&json!({"value": encode(3)})), Ok(Some(3)));
        assert_eq!(
            decode(&json!({"value": b64("garbage")})),
            Err("garbage".into())
        );
    }

    #[test]
    fn watches_must_see_every_revision_once() {
        let mut book = Book::new(1);
        book.version(0, 3, Some(1), "write".into());
        let log = |events: Vec<(i64, &str, &str)>| WatchLog {
            member: "n1".into(),
            events: events
                .into_iter()
                .map(|(r, k, v)| (r, k.into(), v.into()))
                .collect(),
            reconnects: 0,
        };
        let good = log(vec![(2, "r0", "4"), (3, "r0", "1")]);
        assert!(check_watches(&mut book, &[good], 3).0.is_empty());
        assert!(book.conflicts.is_empty());
        let skipped = log(vec![(3, "r0", "1")]);
        assert_eq!(check_watches(&mut book, &[skipped], 3).0.len(), 1);
        let wrong = log(vec![(2, "r0", "4"), (3, "r0", "2")]);
        check_watches(&mut book, &[wrong], 3);
        assert_eq!(book.conflicts.len(), 1);
    }

    #[test]
    fn exits_fail_the_run_unless_tolerated() {
        let r = Recovery {
            live: vec!["n2".into()],
            down: vec!["n1".into(), "n3".into()],
            restarted: vec!["n1".into()],
            exited: vec!["n1".into()],
        };
        let (text, violated) = recovery_text(&r, true);
        assert!(violated, "n3 is running but not serving: {text}");
        assert!(text.contains("n1 refused to start"));
        let r = Recovery {
            down: vec!["n1".into()],
            ..r
        };
        assert!(!recovery_text(&r, true).1);
        assert!(recovery_text(&r, false).1);
        // Three of five refuse to start: the other two cannot serve.
        let r = Recovery {
            live: vec![],
            down: ["n1", "n2", "n3", "n4", "n5"].map(String::from).to_vec(),
            restarted: vec![],
            exited: ["n1", "n2", "n3"].map(String::from).to_vec(),
        };
        assert!(!recovery_text(&r, true).1);
    }

    #[test]
    fn writes_must_pass_the_floor() {
        let mut book = Book::new(1);
        write_revision(&mut book, 0, 7, 0, 1, "a".into());
        assert!(book.stale.is_empty());
        // Began after the first write completed, yet ordered before it.
        write_revision(&mut book, 0, 6, 7, 2, "b".into());
        assert_eq!(book.stale.len(), 1);
    }
}
