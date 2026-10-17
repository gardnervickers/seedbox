//! One lock-set client for the etcd oracle, bound to one member's JSON
//! gateway. For every operation it grants a fresh lease (TTL `--ttl`) and
//! renews it every TTL/3, takes the lock through etcd's Lock RPC
//! (`/v3/lock/lock`, the server-side `concurrency.Mutex` Jepsen's jetcd
//! client uses), reads the set, waits `--hold` seconds, writes the set plus
//! one unique element, then releases the lock and revokes the lease. Nothing
//! reacts to a lost lease, exactly like a lock user that trusts the lock.
//!
//! The client is one thread: it renews the lease while it waits for the Lock
//! reply and while it holds the lock, so a pause of this process stops
//! renewals too, which is how a GC pause loses the lease. Every step is
//! reported to the oracle's store (`/event`).
use super::etcd::Guard;
use super::etcd_gateway::{b64, number, post, unb64, CLIENT_PORT};
use crate::kit::{self, http, Error, Result};
use serde_json::{json, Value};
use std::{
    io::BufReader,
    os::fd::AsRawFd,
    time::{Duration, Instant},
};

#[derive(clap::Args, Debug, Clone)]
pub struct Args {
    #[arg(long)]
    pub name: String,
    #[arg(long)]
    pub member: String,
    #[arg(long, default_value = "oracle:8000")]
    pub store: String,
    #[arg(long, value_enum, default_value = "none")]
    pub guard: Guard,
    #[arg(long, default_value_t = 2)]
    pub ttl: i64,
    /// Seconds between the read and the write.
    #[arg(long, default_value_t = 1.0)]
    pub hold: f64,
    #[arg(long, default_value_t = 30.0)]
    pub lock_timeout: f64,
}

/// A lease renewed every TTL/3 from the client's only thread; a failed
/// renewal is retried at once.
struct Lease {
    id: i64,
    interval: f64,
    due: Instant,
    ok: u64,
    expired: u64,
    error: u64,
}

impl Lease {
    fn new(id: i64, ttl: i64) -> Self {
        let interval = ttl as f64 / 3.0;
        Self {
            id,
            interval,
            due: Instant::now() + Duration::from_secs_f64(interval),
            ok: 0,
            expired: 0,
            error: 0,
        }
    }

    fn stats(&self) -> Value {
        json!({"ok": self.ok, "expired": self.expired, "error": self.error})
    }

    fn renew_if_due(&mut self, member: &str) {
        if Instant::now() < self.due {
            return;
        }
        let sent = Instant::now();
        match post(
            member,
            "/v3/lease/keepalive",
            &json!({"ID": self.id.to_string()}),
            self.interval,
        ) {
            Ok(reply) => {
                if number(&reply["result"]["TTL"]) > 0 {
                    self.ok += 1;
                } else {
                    self.expired += 1;
                }
                self.due = sent + Duration::from_secs_f64(self.interval);
            }
            Err(_) => {
                self.error += 1;
                self.due = Instant::now() + Duration::from_millis(50);
            }
        }
    }

    /// Renew while waiting `seconds`; with `fd`, return true as soon as it is readable.
    fn wait(&mut self, member: &str, seconds: f64, fd: Option<i32>) -> bool {
        let deadline = Instant::now() + Duration::from_secs_f64(seconds);
        loop {
            self.renew_if_due(member);
            let now = Instant::now();
            if now >= deadline {
                return false;
            }
            let pause = (deadline - now).min(self.due.saturating_duration_since(now));
            match fd {
                Some(fd) => {
                    let mut poll = [libc::pollfd {
                        fd,
                        events: libc::POLLIN,
                        revents: 0,
                    }];
                    let millis = pause.as_millis().min(i32::MAX as u128) as i32;
                    // SAFETY: one valid pollfd.
                    if unsafe { libc::poll(poll.as_mut_ptr(), 1, millis) } > 0 {
                        return true;
                    }
                }
                None => std::thread::sleep(pause),
            }
        }
    }
}

struct Client {
    args: Args,
    store_host: String,
    store_port: u16,
}

impl Client {
    fn etcd(&self, path: &str, body: Value, timeout: f64) -> Result<Value> {
        post(&self.args.member, path, &body, timeout)
    }

    /// The Lock RPC, renewing the lease while it blocks.
    fn lock(&self, lease: &mut Lease) -> Result<Value> {
        let body = json!({"name": b64("lock"), "lease": lease.id.to_string()}).to_string();
        let mut stream = http::connect(&self.args.member, CLIENT_PORT, Duration::from_secs(2))?;
        http::write_request(
            &mut stream,
            "POST",
            &self.args.member,
            "/v3/lock/lock",
            Some(("application/json", body.as_bytes())),
        )?;
        if !lease.wait(
            &self.args.member,
            self.args.lock_timeout,
            Some(stream.as_raw_fd()),
        ) {
            return Err(Error("lock timed out".into()));
        }
        stream.set_read_timeout(Some(Duration::from_secs(5)))?;
        let mut reader = BufReader::new(stream);
        let head = http::read_head(&mut reader)?;
        let mut data = Vec::new();
        std::io::Read::read_to_end(&mut http::Body::new(&mut reader, &head), &mut data)?;
        if head.status != 200 {
            let text = String::from_utf8_lossy(&data[..data.len().min(160)]).to_string();
            return Err(Error(format!("lock: HTTP {} {text:?}", head.status)));
        }
        Ok(serde_json::from_slice(&data)?)
    }

    /// Report to the oracle; retried because the oracle's link is never faulted.
    fn store(&self, body: Value) -> Result<Value> {
        let deadline = Instant::now() + Duration::from_secs(60);
        let mut body = body.as_object().cloned().unwrap_or_default();
        body.insert("client".into(), self.args.name.clone().into());
        let data = Value::Object(body).to_string();
        loop {
            let attempt = http::connect(&self.store_host, self.store_port, Duration::from_secs(10))
                .and_then(|stream| {
                    let (_, reply) = http::exchange(
                        stream,
                        "POST",
                        &format!("{}:{}", self.store_host, self.store_port),
                        "/event",
                        Some(("application/json", data.as_bytes())),
                    )?;
                    Ok(serde_json::from_slice::<Value>(&reply)?)
                });
            match attempt {
                Ok(reply) => return Ok(reply),
                Err(error) if Instant::now() > deadline => return Err(error),
                Err(_) => kit::pause(0.2),
            }
        }
    }

    fn event(&self, fields: Value) -> Value {
        // Each step of a locked update is a guest state too, so a campaign
        // can tell where a client was and place a fault after it.
        let phase = match fields["type"].as_str() {
            Some("invoke") => Some("waiting"),
            Some("acquired") => Some("holding"),
            Some("read") => Some("read"),
            Some("write") => Some("written"),
            Some("releasing") => Some("released"),
            Some("fail") => Some("failed"),
            _ => None,
        };
        if let Some(phase) = phase {
            seedbox_sdk::state("lock", phase);
        }
        self.store(fields).unwrap_or(Value::Null)
    }

    fn read_set(&self, token: Option<i64>, key: &str) -> Result<(Vec<Value>, bool)> {
        if self.args.guard == Guard::Txn {
            let kvs = self.etcd("/v3/kv/range", json!({"key": b64("set")}), 5.0)?;
            let value: Vec<Value> = kvs["kvs"]
                .get(0)
                .and_then(|kv| {
                    serde_json::from_str(&unb64(kv["value"].as_str().unwrap_or(""))).ok()
                })
                .unwrap_or_default();
            self.event(json!({"type": "read", "key": key, "size": value.len()}));
            return Ok((value, true));
        }
        let reply = self.store(json!({"type": "read", "key": key, "token": token}))?;
        let set = reply["set"].as_array().cloned().unwrap_or_default();
        Ok((set, reply["ok"].as_bool().unwrap_or(false)))
    }

    fn write_set(
        &self,
        value: &[Value],
        token: Option<i64>,
        key: &str,
        element: &str,
    ) -> Result<bool> {
        if self.args.guard == Guard::Txn {
            let txn = json!({
                "compare": [{"key": b64(key), "target": "VERSION", "result": "GREATER", "version": "0"}],
                "success": [{"requestPut": {"key": b64("set"), "value": b64(&Value::from(value.to_vec()).to_string())}}],
            });
            let reply = self.etcd("/v3/kv/txn", txn, 5.0)?;
            let ok = reply["succeeded"].as_bool().unwrap_or(false);
            self.event(
                json!({"type": "write", "key": key, "element": element, "ok": ok,
                              "revision": number(&reply["header"]["revision"])}),
            );
            return Ok(ok);
        }
        let reply = self.store(
            json!({"type": "write", "key": key, "token": token, "element": element, "set": value}),
        )?;
        Ok(reply["ok"].as_bool().unwrap_or(false))
    }

    fn operation(&self, number_: u64) {
        let element = format!("{}-{number_}", self.args.name);
        let short = |e: &Error| -> String { e.0.chars().take(120).collect() };
        let mut lease = match self.etcd("/v3/lease/grant", json!({"TTL": self.args.ttl}), 2.0) {
            Ok(reply) => Lease::new(number(&reply["ID"]), self.args.ttl),
            Err(error) => {
                self.event(json!({"type": "fail", "op": number_, "element": element, "stage": "grant", "error": short(&error)}));
                kit::pause(0.5);
                return;
            }
        };
        let mut key: Option<String> = None;
        self.event(json!({"type": "invoke", "op": number_, "element": element, "lease": lease.id}));
        self.hold(number_, &element, &mut lease, &mut key);
        if let Some(key) = &key {
            self.event(json!({"type": "releasing", "op": number_, "key": key, "keepalives": lease.stats()}));
            let _ = self.etcd("/v3/lock/unlock", json!({"key": b64(key)}), 2.0);
        }
        let _ = self.etcd("/v3/lease/revoke", json!({"ID": lease.id.to_string()}), 2.0);
    }

    fn hold(&self, number_: u64, element: &str, lease: &mut Lease, key_slot: &mut Option<String>) {
        let short = |e: &Error| -> String { e.0.chars().take(120).collect() };
        let reply = match self.lock(lease) {
            Ok(reply) => reply,
            Err(error) => {
                self.event(json!({"type": "fail", "op": number_, "element": element, "stage": "lock", "error": short(&error)}));
                return;
            }
        };
        let key = unb64(reply["key"].as_str().unwrap_or(""));
        *key_slot = Some(key.clone());
        let granted = number(&reply["header"]["revision"]);
        // Evidence only: does the lock key still exist? Unguarded clients ignore the answer.
        let (present, create) = match self.etcd("/v3/kv/range", json!({"key": b64(&key)}), 2.0) {
            Ok(reply) => match reply["kvs"].get(0) {
                Some(kv) => (Some(true), Some(number(&kv["create_revision"]))),
                None => (Some(false), None),
            },
            Err(_) => (None, None),
        };
        let token = create;
        self.event(json!({"type": "acquired", "op": number_, "element": element, "key": key, "lease": lease.id,
                          "granted_revision": granted, "key_present": present, "create_revision": create}));
        if self.args.guard == Guard::Token && token.is_none() {
            self.event(json!({"type": "fail", "op": number_, "element": element, "stage": "token", "key": key}));
            return;
        }
        let rmw = (|| -> Result<bool> {
            lease.renew_if_due(&self.args.member);
            let (value, ok) = self.read_set(token, &key)?;
            if !ok {
                return Ok(false);
            }
            lease.wait(&self.args.member, self.args.hold, None);
            let mut set: Vec<String> = value
                .iter()
                .filter_map(|v| v.as_str().map(str::to_owned))
                .collect();
            set.push(element.into());
            set.sort();
            set.dedup();
            let set: Vec<Value> = set.into_iter().map(Value::from).collect();
            if self.write_set(&set, token, &key, element)? {
                Ok(true)
            } else {
                Err(Error("write-fenced".into()))
            }
        })();
        match rmw {
            Ok(true) => {
                self.event(json!({"type": "ok", "op": number_, "element": element, "key": key}));
            }
            Ok(false) => {
                self.event(json!({"type": "fail", "op": number_, "element": element, "stage": "read-fenced", "key": key}));
            }
            Err(error) if error.0 == "write-fenced" => {
                self.event(json!({"type": "fail", "op": number_, "element": element, "stage": "write-fenced", "key": key}));
            }
            Err(error) => {
                self.event(json!({"type": "info", "op": number_, "element": element, "stage": "rmw", "key": key, "error": short(&error)}));
            }
        }
    }

    fn run(&self) -> ! {
        loop {
            if let Ok(reply) = self.store(json!({"type": "hello", "member": self.args.member})) {
                if reply["start"] == true {
                    break;
                }
            }
            kit::pause(0.5);
        }
        let mut number_ = 0;
        while self.event(json!({"type": "next"}))["stop"] != true {
            number_ += 1;
            self.operation(number_);
        }
        self.event(json!({"type": "done", "ops": number_}));
        loop {
            std::thread::sleep(Duration::from_secs(3600));
        }
    }
}

pub fn run(args: Args) -> u8 {
    let (host, port) = args.store.split_once(':').unwrap_or((&args.store, "8000"));
    let client = Client {
        store_host: host.into(),
        store_port: port.parse().unwrap_or(8000),
        args: args.clone(),
    };
    client.run()
}
