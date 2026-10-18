//! Jepsen's etcd lock-set workload as a Seedbox oracle.
//!
//! Five members n1..n5 form one cluster; five client machines c1..c5 run
//! `seedbox-jepsen etcd-client`, client ci bound to member ni. Each client
//! operation grants a lease (TTL 2 s) kept alive every TTL/3, takes etcd's
//! lock `lock` with it, reads a set, waits a second, writes the set plus a
//! unique element and releases the lock. This oracle hosts the set (Jepsen
//! keeps it in the test process) in a small HTTP store on port 8000, and
//! every client step is reported to it, so the store's log is one total
//! order of the history. The oracle and the clients each run on one thread.
//!
//! Fault modes: `none`; `pause-holder` (SIGSTOP the holder's client after it
//! reads, for longer than the TTL); `pause-waiter-member` and
//! `partition-waiter` (etcd#11456: pause or isolate the member of a client
//! waiting for the lock, not the leader and not the holder's member); and
//! `pause-random-member` (Jepsen's nemesis). `--guard` must match the
//! clients': `none`, `token` (the store fences lower lock-key revisions) or
//! `txn` (the set lives in etcd behind a transaction on the lock key).
//! [`crate::checker::lockset`] checks the history; the run also records
//! etcd#11496 (a watch from revision 0 starts at the current revision).
use super::etcd_gateway::{b64, number, post, unb64, Event, Watch};
use crate::checker::lockset;
use crate::kit::{
    self, evidence, history_line, now,
    serve::{http_request, http_response, Loop, Outcome, Service},
    verdict, Ctl, Rng,
};
use crate::nemesis::Pace;
use serde_json::{json, Map, Value};
use std::{
    collections::BTreeMap,
    time::{Duration, Instant},
};

pub const MEMBERS: [&str; 5] = ["n1", "n2", "n3", "n4", "n5"];

fn member_of(client: &str) -> String {
    format!("n{}", client.trim_start_matches('c'))
}

#[derive(clap::Args, Debug, Clone)]
pub struct Args {
    #[arg(long, value_enum)]
    pub mode: Mode,
    #[arg(long, value_enum, default_value = "none")]
    pub guard: Guard,
    /// Seconds of workload.
    #[arg(long, default_value_t = 120.0)]
    pub duration: f64,
    /// Seconds each fault lasts.
    #[arg(long, default_value_t = 3.0)]
    pub pause: f64,
    /// Seconds between faults.
    #[arg(long, default_value_t = 2.0)]
    pub gap: f64,
    #[arg(long, default_value_t = 8000)]
    pub port: u16,
    /// Docker-compatible engine socket: run against plain containers, no Seedbox.
    #[arg(long)]
    pub native: Option<std::path::PathBuf>,
}

#[derive(clap::ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// No faults.
    None,
    /// SIGSTOP the lock holder's client process after it reads the set.
    PauseHolder,
    /// Pause the member of a client waiting for the lock (etcd#11456).
    PauseWaiterMember,
    /// Jepsen's nemesis: pause a random member's process.
    PauseRandomMember,
    /// Isolate the member of a client waiting for the lock (etcd#11456).
    PartitionWaiter,
}

#[derive(clap::ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum Guard {
    /// Plain reads and writes.
    None,
    /// The store fences operations carrying a lower lock-key revision.
    Token,
    /// The set lives in etcd; writes are transactions on the lock key's existence.
    Txn,
}

impl Mode {
    fn name(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::PauseHolder => "pause-holder",
            Self::PauseWaiterMember => "pause-waiter-member",
            Self::PauseRandomMember => "pause-random-member",
            Self::PartitionWaiter => "partition-waiter",
        }
    }
}

impl Guard {
    pub fn name(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Token => "token",
            Self::Txn => "txn",
        }
    }
}

// ------------------------------------------------------------ the store

/// The protected set, the shared history and the client registry.
pub struct Store {
    guard: Guard,
    pub history: Vec<Value>,
    pub set: Vec<Value>,
    max_token: i64,
    pub hello: BTreeMap<String, Value>,
    pub done: BTreeMap<String, Value>,
    pub start: bool,
    pub stop: bool,
    /// client -> (key, op, read)
    pub holders: BTreeMap<String, (Value, Value, bool)>,
    /// client -> op
    pub waiting: BTreeMap<String, Value>,
    /// (history index, client)
    pub last_read: Option<(usize, String)>,
}

impl Store {
    pub fn new(guard: Guard) -> Self {
        Self {
            guard,
            history: Vec::new(),
            set: Vec::new(),
            max_token: 0,
            hello: BTreeMap::new(),
            done: BTreeMap::new(),
            start: false,
            stop: false,
            holders: BTreeMap::new(),
            waiting: BTreeMap::new(),
            last_read: None,
        }
    }

    pub fn handle(&mut self, body: Map<String, Value>) -> Value {
        let kind = body
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let client = body
            .get("client")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        match kind.as_str() {
            "hello" => {
                self.hello
                    .insert(client, body.get("member").cloned().unwrap_or(Value::Null));
                return json!({"start": self.start});
            }
            "next" => return json!({"stop": self.stop}),
            "done" => {
                self.done
                    .insert(client, body.get("ops").cloned().unwrap_or(Value::Null));
                return json!({});
            }
            _ => {}
        }
        let mut entry = body.clone();
        entry.insert("t".into(), now().into());
        entry.insert("i".into(), self.history.len().into());
        let mut reply = json!({"ok": true});
        let plain = self.guard != Guard::Txn;
        if kind == "read" && plain {
            if !self.fence(&mut entry) {
                reply = json!({"ok": false});
            } else {
                reply["set"] = Value::Array(self.set.clone());
            }
            entry.insert("size".into(), self.set.len().into());
        } else if kind == "write" && plain {
            if self.fence(&mut entry) {
                self.set = body
                    .get("set")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
            } else {
                reply = json!({"ok": false});
            }
            entry.remove("set");
        }
        if (kind == "read" || kind == "write") && plain {
            entry.insert("ok".into(), reply["ok"].clone());
        } else {
            // txn clients report their own transaction's outcome
            let ok = entry.get("ok").cloned().unwrap_or(Value::Null);
            entry.insert("ok".into(), ok);
        }
        let op = body.get("op").cloned().unwrap_or(Value::Null);
        match kind.as_str() {
            "invoke" => {
                self.waiting.insert(client.clone(), op.clone());
            }
            "acquired" => {
                self.waiting.remove(&client);
                let key = body.get("key").cloned().unwrap_or(Value::Null);
                self.holders
                    .insert(client.clone(), (key, op.clone(), false));
            }
            "read" => {
                if let Some(holder) = self.holders.get_mut(&client) {
                    holder.2 = true;
                    self.last_read = Some((self.history.len(), client.clone()));
                }
            }
            "releasing" | "fail" | "info" if self.waiting.get(&client) == Some(&op) => {
                self.waiting.remove(&client);
            }
            _ => {}
        }
        if kind == "releasing" {
            self.holders.remove(&client);
        }
        let entry = Value::Object(entry);
        history_line(&entry);
        self.history.push(entry);
        reply
    }

    fn fence(&mut self, entry: &mut Map<String, Value>) -> bool {
        if self.guard != Guard::Token {
            return true;
        }
        match entry.get("token").and_then(Value::as_i64) {
            Some(token) if token >= self.max_token => {
                self.max_token = token;
                true
            }
            _ => {
                entry.insert("fenced".into(), true.into());
                false
            }
        }
    }

    /// A client holding the lock that read the set at history index `since`
    /// or later and still holds it.
    fn holder_reading(&self, since: usize) -> Option<String> {
        let (index, client) = self.last_read.as_ref()?;
        (*index >= since && self.holders.contains_key(client)).then(|| client.clone())
    }
}

impl Service for Store {
    fn receive(&mut self, buffer: &mut Vec<u8>) -> Outcome {
        let Some((_, _, body)) = http_request(buffer) else {
            return Outcome::Wait;
        };
        let parsed = if body.is_empty() {
            Ok(json!({}))
        } else {
            serde_json::from_slice::<Value>(&body)
        };
        match parsed {
            Ok(Value::Object(body)) => {
                let reply = self.handle(body);
                Outcome::Reply(http_response("200 OK", reply.to_string().as_bytes()))
            }
            Ok(_) => Outcome::Reply(http_response(
                "400 Bad Request",
                br#"{"error":"not an object"}"#,
            )),
            Err(error) => Outcome::Reply(http_response(
                "400 Bad Request",
                json!({"error": error.to_string()}).to_string().as_bytes(),
            )),
        }
    }
}

/// Waits serve the store.
struct Serving<'a> {
    server: &'a mut Loop,
    store: &'a mut Store,
}

impl Pace for Serving<'_> {
    fn wait(&mut self, _: &mut Ctl, seconds: f64) {
        self.server.serve(self.store, seconds);
    }
}

// ------------------------------------------------------------ etcd

fn status(member: &str, timeout: f64) -> Option<Value> {
    post(member, "/v3/maintenance/status", &json!({}), timeout).ok()
}

/// The member name of the current leader, as reachable members report it.
fn leader_member(timeout: f64) -> Option<String> {
    let mut ids: BTreeMap<String, String> = BTreeMap::new();
    let mut leader: Option<String> = None;
    for member in MEMBERS {
        if let Some(reply) = status(member, timeout) {
            ids.insert(
                reply["header"]["member_id"].as_str().unwrap_or("").into(),
                member.into(),
            );
            if leader.is_none() {
                leader = reply["leader"].as_str().map(str::to_owned);
            }
        }
    }
    leader.and_then(|l| ids.get(&l).cloned())
}

fn wait_cluster(server: &mut Loop, store: &mut Store, seconds: f64, label: &str) -> bool {
    let deadline = Instant::now() + Duration::from_secs_f64(seconds);
    while Instant::now() < deadline {
        let replies: Vec<Option<Value>> = MEMBERS.iter().map(|m| status(m, 2.0)).collect();
        let leaders: std::collections::BTreeSet<Option<String>> = replies
            .iter()
            .flatten()
            .map(|r| r["leader"].as_str().map(str::to_owned))
            .collect();
        if replies.iter().all(Option::is_some)
            && leaders.len() == 1
            && !leaders.contains(&None)
            && !leaders.contains(&Some("0".into()))
        {
            let revision = number(&replies[0].as_ref().unwrap()["header"]["revision"]);
            evidence(
                "cluster",
                json!({"label": label, "leader": leader_member(0.5), "revision": revision}),
            );
            return true;
        }
        server.serve(store, 0.5);
    }
    let reachable: Map<String, Value> = MEMBERS
        .iter()
        .map(|m| (m.to_string(), json!(status(m, 2.0).is_some())))
        .collect();
    evidence(
        "cluster-unhealthy",
        json!({"label": label, "replies": reachable}),
    );
    false
}

fn any_member(path: &str, body: &Value, timeout: f64) -> kit::Result<Value> {
    let mut last = String::new();
    for _ in 0..20 {
        for member in MEMBERS {
            match post(member, path, body, timeout) {
                Ok(reply) => return Ok(reply),
                Err(error) => last = error.0,
            }
        }
        kit::pause(0.5);
    }
    Err(kit::Error(format!("{path}: no member answered: {last}")))
}

/// Wait until every member renews a lease promptly: after boot, followers
/// can take seconds to forward renewals, which by itself expires 2 s leases.
fn wait_keepalives(server: &mut Loop, store: &mut Store, seconds: f64) -> bool {
    let deadline = Instant::now() + Duration::from_secs_f64(seconds);
    let Ok(grant) = any_member("/v3/lease/grant", &json!({"TTL": 10}), 5.0) else {
        evidence("keepalives-not-ready", json!({}));
        return false;
    };
    let lease = number(&grant["ID"]);
    let mut streak = 0;
    while Instant::now() < deadline {
        let renewed = MEMBERS.iter().all(|m| {
            post(
                m,
                "/v3/lease/keepalive",
                &json!({"ID": lease.to_string()}),
                0.3,
            )
            .is_ok()
        });
        streak = if renewed { streak + 1 } else { 0 };
        if streak >= 3 {
            let _ = any_member("/v3/lease/revoke", &json!({"ID": lease.to_string()}), 5.0);
            evidence("keepalives-ready", json!({}));
            return true;
        }
        server.serve(store, 0.5);
    }
    evidence("keepalives-not-ready", json!({}));
    false
}

/// etcd#11496: watch(rev 0) streams only new events; watch(rev 1) replays all of them.
fn watch_revision_zero() {
    let put = |key: &str, value: &str| {
        any_member(
            "/v3/kv/put",
            &json!({"key": b64(key), "value": b64(value)}),
            5.0,
        )
    };
    let _ = put("w/1", "1");
    let _ = put("w/2", "2");
    let mut watches: Vec<(i64, Watch)> = Vec::new();
    let mut errors = Map::new();
    let result = (|| -> kit::Result<()> {
        for revision in [0, 1] {
            let mut watch = Watch::open("n1", "w/", "w0", Some(revision), 20.0)?;
            watch.read_until(|w| w.created)?;
            watches.push((revision, watch));
        }
        put("w/3", "3")?;
        for (_, watch) in &mut watches {
            watch.read_until(|w| w.events.iter().any(|e| e.key == "w/3"))?;
        }
        Ok(())
    })();
    if let Err(error) = result {
        errors.insert("watch".into(), error.0.into());
    }
    let keys = |revision: i64| -> Option<Vec<String>> {
        watches
            .iter()
            .find(|(r, _)| *r == revision)
            .map(|(_, w)| w.events.iter().map(|e| e.key.clone()).collect())
    };
    let (rev0, rev1) = (keys(0), keys(1));
    evidence(
        "watch",
        json!({"rev0_keys": rev0, "rev1_keys": rev1, "errors": errors,
               "rev0_starts_after_current": rev0.as_deref() == Some(&["w/3".to_string()][..]),
               "rev1_replays": rev1.as_deref() == Some(&["w/1".to_string(), "w/2".to_string(), "w/3".to_string()][..])}),
    );
}

/// Every event in the cluster's history, through a watch from revision 1 up to a marker put.
fn revision_history() -> (Option<Vec<Event>>, i64) {
    let marker = any_member(
        "/v3/kv/put",
        &json!({"key": b64("zz-end"), "value": b64("end")}),
        5.0,
    )
    .map(|r| number(&r["header"]["revision"]))
    .unwrap_or(0);
    for member in MEMBERS {
        let read = Watch::open(member, "\0", "\0", Some(1), 60.0).and_then(|mut watch| {
            watch.read_until(|w| w.events.iter().any(|e| e.mod_revision >= marker))?;
            Ok(watch.events)
        });
        match read {
            Ok(events) => return (Some(events), marker),
            Err(error) => evidence(
                "history-watch-failed",
                json!({"member": member, "error": error.0}),
            ),
        }
    }
    (None, marker)
}

// ------------------------------------------------------------ nemesis

fn pause(
    ctl: &mut Ctl,
    serving: &mut Serving,
    target: &str,
    seconds: f64,
    context: Value,
) -> kit::Result<()> {
    crate::nemesis::step_with(ctl, "pause_process", &[target], context)?;
    serving.wait(ctl, seconds);
    crate::nemesis::step(ctl, "resume_process", &[target])
}

/// Run the fault schedule until `end`; the store is served whenever it waits.
fn nemesis(
    ctl: &mut Ctl,
    server: &mut Loop,
    store: &mut Store,
    args: &Args,
    rng: &mut Rng,
    end: Instant,
) -> kit::Result<u64> {
    let mut count = 0;
    while Instant::now() < end {
        let window = end
            .saturating_duration_since(Instant::now())
            .as_secs_f64()
            .clamp(0.1, 10.0);
        match args.mode {
            Mode::None => {
                server.serve(
                    store,
                    end.saturating_duration_since(Instant::now()).as_secs_f64(),
                );
                break;
            }
            Mode::PauseRandomMember => {
                rng.refresh();
                let target = *rng.choice(&MEMBERS);
                let leader = leader_member(0.5);
                let mut serving = Serving { server, store };
                pause(
                    ctl,
                    &mut serving,
                    target,
                    args.pause,
                    json!({"leader": leader}),
                )?;
                count += 1;
                serving.wait(ctl, args.gap);
                continue;
            }
            Mode::PauseHolder => {
                let since = store.history.len();
                let Some(holder) = server.wait_for(store, window, |s| s.holder_reading(since))
                else {
                    continue;
                };
                let key = store.holders.get(&holder).map(|h| h.0.clone());
                let mut serving = Serving { server, store };
                pause(
                    ctl,
                    &mut serving,
                    &holder,
                    args.pause,
                    json!({"holder": holder, "key": key}),
                )?;
                count += 1;
                serving.wait(ctl, args.gap);
                continue;
            }
            Mode::PauseWaiterMember | Mode::PartitionWaiter => {}
        }
        let since = store.history.len();
        let leader = leader_member(0.5);
        let pick = |s: &Store| -> Option<(String, String)> {
            let holder = s.holder_reading(since)?;
            let leader = leader.as_ref()?;
            let holder_member = member_of(&holder);
            let waiter = s
                .waiting
                .keys()
                .filter(|c| {
                    **c != holder && member_of(c) != *leader && member_of(c) != holder_member
                })
                .min()?
                .clone();
            Some((holder, waiter))
        };
        let Some((holder, waiter)) = server.wait_for(store, window, pick) else {
            continue;
        };
        let waiter_op = store.waiting.get(&waiter).cloned();
        let context =
            json!({"holder": holder, "waiter": waiter, "leader": leader, "waiter_op": waiter_op});
        let isolated = member_of(&waiter);
        let mut serving = Serving { server, store };
        if args.mode == Mode::PauseWaiterMember {
            pause(ctl, &mut serving, &isolated, args.pause, context)?;
        } else {
            let others: Vec<&str> = MEMBERS.iter().copied().filter(|m| *m != isolated).collect();
            let cut = crate::nemesis::cut(ctl, &[isolated.as_str()], &others, context)?;
            serving.wait(ctl, args.pause);
            crate::nemesis::heal(ctl, &cut)?;
        }
        count += 1;
        serving.wait(ctl, args.gap);
    }
    Ok(count)
}

// ------------------------------------------------------------ main

pub fn run(args: Args) -> u8 {
    kit::start_clock();
    let rng_seed = kit::rng::entropy_seed();
    let mut rng = Rng::new(rng_seed);
    evidence(
        "start",
        json!({"mode": args.mode.name(), "guard": args.guard.name(), "rng_seed": rng_seed,
               "duration": args.duration, "pause": args.pause, "gap": args.gap}),
    );
    let mut store = Store::new(args.guard);
    let mut server = match Loop::bind(args.port) {
        Ok(server) => server,
        Err(error) => return verdict(kit::INCONCLUSIVE, &error.0),
    };
    let mut ctl = match kit::connect_world(args.native.clone()) {
        Ok(ctl) => ctl,
        Err(error) => return verdict(kit::INCONCLUSIVE, &format!("no controller: {error}")),
    };
    if !wait_cluster(&mut server, &mut store, 240.0, "boot") {
        return verdict(kit::INCONCLUSIVE, "cluster never became healthy");
    }
    wait_keepalives(&mut server, &mut store, 120.0);
    watch_revision_zero();
    if args.guard == Guard::Txn {
        let _ = any_member(
            "/v3/kv/put",
            &json!({"key": b64("set"), "value": b64("[]")}),
            5.0,
        );
    }
    let clients = 5;
    if !server.wait_until(&mut store, 240.0, |s| s.hello.len() == clients) {
        evidence(
            "clients",
            json!({"hello": store.hello.keys().collect::<Vec<_>>()}),
        );
        return verdict(kit::INCONCLUSIVE, "not every client reached the store");
    }
    store.start = true;
    evidence("clients", json!({"hello": store.hello}));
    let end = Instant::now() + Duration::from_secs_f64(args.duration);
    let faults = match nemesis(&mut ctl, &mut server, &mut store, &args, &mut rng, end) {
        Ok(faults) => faults,
        Err(error) => {
            evidence("nemesis-failed", json!({"error": error.0}));
            0
        }
    };
    evidence("nemesis", json!({"faults": faults}));
    ctl.heal_everything();
    store.stop = true;
    server.wait_until(&mut store, 90.0, |s| s.done.len() == clients);
    evidence("clients-done", json!({"done": store.done}));
    if let Ok(Some(world)) = ctl.inspect() {
        evidence("inspect", json!({"world": world}));
    }
    wait_cluster(&mut server, &mut store, 120.0, "final");
    let final_set: Vec<Value> = if args.guard == Guard::Txn {
        any_member("/v3/kv/range", &json!({"key": b64("set")}), 5.0)
            .ok()
            .and_then(|r| r["kvs"].get(0).cloned())
            .and_then(|kv| serde_json::from_str(&unb64(kv["value"].as_str().unwrap_or(""))).ok())
            .unwrap_or_default()
    } else {
        store.set.clone()
    };
    evidence("final-set", json!({"set": final_set}));
    let (lock_events, marker) = revision_history();
    let lock_events: Option<Vec<Value>> =
        lock_events.map(|events| events.iter().map(Event::to_json).collect());
    if let Some(events) = &lock_events {
        let locks: Vec<&Value> = events
            .iter()
            .filter(|e| e["key"].as_str().is_some_and(|k| k.starts_with("lock/")))
            .collect();
        evidence("lock-keys", json!({"events": locks, "revision": marker}));
    }
    conclude(
        &store.history,
        &final_set,
        args.guard.name(),
        lock_events.as_deref(),
        Some(faults),
        args.mode != Mode::None,
    )
}

fn conclude(
    history: &[Value],
    final_set: &[Value],
    guard: &str,
    lock_events: Option<&[Value]>,
    faults: Option<u64>,
    faulted: bool,
) -> u8 {
    let set: Vec<String> = final_set
        .iter()
        .filter_map(|v| v.as_str().map(str::to_owned))
        .collect();
    let checked = lockset::check(history, &set, guard, lock_events);
    let mut result = checked.result.clone();
    result["faults"] = json!(faults);
    result["history_complete"] = json!(lock_events.is_some());
    evidence("check", result.clone());
    evidence("details", checked.details.clone());
    if result["acked"] == 0 {
        return verdict(kit::INCONCLUSIVE, "no update was acknowledged");
    }
    let summary = format!(
        "{}/{} acked updates lost ({}%), {} overlapping holds, {} interleaved, {} stale grants, {} fenced, mutex {}, {} faults",
        result["lost"], result["acked"], result["lost_pct"], result["overlap_pairs"], result["interleaved_rmw"],
        result["stale_grants"], result["fenced"], result["mutex"].as_str().unwrap_or(""),
        faults.map(|f| f.to_string()).unwrap_or_else(|| "?".into())
    );
    if checked.violation {
        return verdict(kit::VIOLATION, &summary);
    }
    if faulted && faults == Some(0) {
        return verdict(
            kit::INCONCLUSIVE,
            &format!("no fault was applied; {summary}"),
        );
    }
    verdict(kit::OK, &summary)
}

/// Re-check a saved oracle log: its `H` lines, `lock-keys` evidence and
/// `final-set` evidence. Logs from before the final set was recorded give it
/// as the acknowledged elements their own check did not report lost.
pub fn recheck(text: &str) -> (u8, Value) {
    let history = kit::tagged_lines(text, "H ");
    let start = kit::evidence_of(text, "start").unwrap_or_default();
    let guard = start["guard"].as_str().unwrap_or("none").to_string();
    let lock_events =
        kit::evidence_of(text, "lock-keys").and_then(|e| e["events"].as_array().cloned());
    let set: Vec<Value> = match kit::evidence_of(text, "final-set") {
        Some(recorded) => recorded["set"].as_array().cloned().unwrap_or_default(),
        None => {
            let details = kit::evidence_of(text, "details").unwrap_or_default();
            let lost: Vec<Value> = details["lost_elements"]
                .as_array()
                .cloned()
                .unwrap_or_default();
            history
                .iter()
                .filter(|e| e["type"] == "ok")
                .map(|e| e["element"].clone())
                .filter(|element| !lost.contains(element))
                .collect()
        }
    };
    let strings: Vec<String> = set
        .iter()
        .filter_map(|v| v.as_str().map(str::to_owned))
        .collect();
    let checked = lockset::check(&history, &strings, &guard, lock_events.as_deref());
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

#[cfg(test)]
mod tests {
    use super::*;

    fn body(value: Value) -> Map<String, Value> {
        value.as_object().cloned().unwrap()
    }

    #[test]
    fn token_store_fences_lower_tokens() {
        let mut store = Store::new(Guard::Token);
        assert_eq!(
            store.handle(body(json!({"type": "read", "client": "c2", "token": 12})))["ok"],
            true
        );
        assert_eq!(
            store.handle(body(
                json!({"type": "write", "client": "c1", "token": 10, "set": ["c1-1"]})
            ))["ok"],
            false
        );
        assert_eq!(
            store.handle(body(
                json!({"type": "write", "client": "c2", "token": 12, "set": ["c2-1"]})
            ))["ok"],
            true
        );
        assert_eq!(store.set, vec![json!("c2-1")]);
    }

    #[test]
    fn txn_store_keeps_the_reported_outcome() {
        let mut store = Store::new(Guard::Txn);
        store.handle(body(json!({"type": "write", "client": "c1", "key": "lock/a", "ok": false, "element": "c1-1"})));
        assert_eq!(store.history.last().unwrap()["ok"], false);
    }

    #[test]
    fn holders_and_waiters_follow_the_reports() {
        let mut store = Store::new(Guard::None);
        store.handle(body(json!({"type": "invoke", "client": "c1", "op": 1})));
        store.handle(body(json!({"type": "invoke", "client": "c2", "op": 1})));
        store.handle(body(
            json!({"type": "acquired", "client": "c1", "op": 1, "key": "lock/a"}),
        ));
        assert_eq!(store.holder_reading(0), None);
        store.handle(body(
            json!({"type": "read", "client": "c1", "key": "lock/a"}),
        ));
        assert_eq!(store.holder_reading(0).as_deref(), Some("c1"));
        assert_eq!(store.waiting.keys().collect::<Vec<_>>(), ["c2"]);
        store.handle(body(
            json!({"type": "releasing", "client": "c1", "op": 1, "key": "lock/a"}),
        ));
        assert_eq!(store.holder_reading(0), None);
    }
}
