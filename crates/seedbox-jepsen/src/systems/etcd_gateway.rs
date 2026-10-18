//! etcd's v3 JSON gateway (`grpc-gateway`): unary calls and streamed watches.
use crate::kit::{base64, http, Error, Result};
use serde_json::{json, Value};
use std::{
    io::{BufRead, BufReader},
    net::TcpStream,
    time::Duration,
};

pub const CLIENT_PORT: u16 = 2379;

pub fn post(member: &str, path: &str, body: &Value, timeout: f64) -> Result<Value> {
    http::post_json(
        member,
        CLIENT_PORT,
        path,
        body,
        Duration::from_secs_f64(timeout),
    )
}

pub fn b64(text: &str) -> String {
    base64::encode_str(text)
}

pub fn unb64(text: &str) -> String {
    base64::decode_str(text).unwrap_or_default()
}

/// A revision or ID field, which the gateway encodes as a decimal string.
pub fn number(value: &Value) -> i64 {
    value
        .as_i64()
        .or_else(|| value.as_str().and_then(|s| s.parse().ok()))
        .unwrap_or(0)
}

/// One watch event.
#[derive(Clone, Debug, PartialEq)]
pub struct Event {
    pub kind: String,
    pub key: String,
    pub mod_revision: i64,
    pub create_revision: i64,
    pub lease: Value,
    /// The value, decoded from base64 (empty for a delete).
    pub value: String,
}

impl Event {
    pub fn to_json(&self) -> Value {
        json!({"type": self.kind, "key": self.key, "mod_revision": self.mod_revision,
               "create_revision": self.create_revision, "lease": self.lease})
    }
}

/// A gateway watch on `[key, range_end)` from `start_revision`, read on the calling thread.
pub struct Watch {
    body: BufReader<http::Body<BufReader<TcpStream>>>,
    pub events: Vec<Event>,
    pub created: bool,
}

impl Watch {
    pub fn open(
        member: &str,
        key: &str,
        range_end: &str,
        start_revision: Option<i64>,
        timeout: f64,
    ) -> Result<Self> {
        let mut request = json!({"key": b64(key), "range_end": b64(range_end)});
        if let Some(revision) = start_revision {
            request["start_revision"] = json!(revision.to_string());
        }
        let data = json!({"create_request": request}).to_string();
        let mut stream = http::connect(member, CLIENT_PORT, Duration::from_secs_f64(timeout))?;
        http::write_request(
            &mut stream,
            "POST",
            &format!("{member}:{CLIENT_PORT}"),
            "/v3/watch",
            Some(("application/json", data.as_bytes())),
        )?;
        let mut reader = BufReader::new(stream);
        let head = http::read_head(&mut reader)?;
        if head.status != 200 {
            return Err(Error(format!("{member}/v3/watch: HTTP {}", head.status)));
        }
        let body = BufReader::new(http::Body::new(reader, &head));
        Ok(Self {
            body,
            events: Vec::new(),
            created: false,
        })
    }

    /// Read responses until `until` holds; error on a closed stream or a timeout.
    pub fn read_until(&mut self, mut until: impl FnMut(&Self) -> bool) -> Result<()> {
        while !until(self) {
            let mut line = String::new();
            if self.body.read_line(&mut line)? == 0 {
                return Err(Error("watch stream ended".into()));
            }
            if line.trim().is_empty() {
                continue;
            }
            let document: Value = serde_json::from_str(&line)?;
            let result = &document["result"];
            self.created = self.created || result["created"].as_bool().unwrap_or(false);
            for event in result["events"].as_array().into_iter().flatten() {
                let kv = &event["kv"];
                self.events.push(Event {
                    kind: event["type"].as_str().unwrap_or("PUT").into(),
                    key: unb64(kv["key"].as_str().unwrap_or("")),
                    mod_revision: number(&kv["mod_revision"]),
                    create_revision: number(&kv["create_revision"]),
                    lease: kv["lease"].clone(),
                    value: unb64(kv["value"].as_str().unwrap_or("")),
                });
            }
        }
        Ok(())
    }
}
