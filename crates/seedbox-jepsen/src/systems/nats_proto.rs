//! A synchronous NATS client bound to one server: no discovery, no reconnect.
use crate::kit::Rng;
use serde_json::Value;
use std::{
    collections::VecDeque,
    io::{ErrorKind, Read, Write},
    net::TcpStream,
    sync::Mutex,
    time::{Duration, Instant},
};

pub const CLIENT_PORT: u16 = 4222;
pub const HTTP_PORT: u16 = 8222;

#[derive(Debug, Clone)]
pub enum NatsError {
    Timeout(String),
    Other(String),
}

impl std::fmt::Display for NatsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Timeout(m) | Self::Other(m) => f.write_str(m),
        }
    }
}

pub type Result<T> = std::result::Result<T, NatsError>;

/// One delivered message.
#[derive(Debug, Clone)]
pub struct Msg {
    pub subject: String,
    pub sid: String,
    pub reply: Option<String>,
    pub headers: Vec<u8>,
    pub body: Vec<u8>,
}

enum Op {
    Info(Value),
    Pong,
    Msg(Msg),
}

static IDS: Mutex<Option<Rng>> = Mutex::new(None);

/// A random 64-bit identifier from the oracle's seeded stream.
pub fn random_id() -> u64 {
    let mut ids = IDS.lock().unwrap_or_else(|e| e.into_inner());
    ids.get_or_insert_with(|| Rng::new(crate::kit::rng::entropy_seed()))
        .next_u64()
}

pub struct Nats {
    pub host: String,
    stream: TcpStream,
    buf: Vec<u8>,
    pub token: u64,
    pub inbox: String,
    /// Messages that arrived while a reply was awaited.
    pub queue: VecDeque<Msg>,
    pub info: Value,
}

impl Nats {
    pub fn connect(host: &str, timeout: f64) -> Result<Self> {
        let deadline = Instant::now() + Duration::from_secs_f64(timeout);
        let stream = crate::kit::http::connect(host, CLIENT_PORT, Duration::from_secs_f64(timeout))
            .map_err(|e| NatsError::Other(e.0))?;
        let mut nats = Self {
            host: host.into(),
            stream,
            buf: Vec::new(),
            token: 0,
            inbox: format!("_INBOX.{:016x}", random_id()),
            queue: VecDeque::new(),
            info: Value::Null,
        };
        match nats.next(deadline)? {
            Op::Info(info) => nats.info = info,
            _ => return Err(NatsError::Other(format!("{host}: expected INFO"))),
        }
        let connect = serde_json::json!({"verbose": false, "pedantic": false, "headers": true,
            "no_responders": true, "protocol": 1, "lang": "rust-raw", "version": "1", "name": "jepsen-oracle"});
        nats.send(format!("CONNECT {connect}\r\nPING\r\n").as_bytes())?;
        loop {
            if let Op::Pong = nats.next(deadline)? {
                break;
            }
        }
        let sub = format!("SUB {}.> 1\r\n", nats.inbox);
        nats.send(sub.as_bytes())?;
        Ok(nats)
    }

    pub fn send(&mut self, data: &[u8]) -> Result<()> {
        self.stream
            .write_all(data)
            .map_err(|e| NatsError::Other(format!("{}: send failed: {e}", self.host)))
    }

    fn fill(&mut self, deadline: Instant) -> Result<()> {
        let now = Instant::now();
        if now >= deadline {
            return Err(NatsError::Timeout(format!("{}: timeout", self.host)));
        }
        let _ = self.stream.set_read_timeout(Some(deadline - now));
        let mut chunk = vec![0u8; 1 << 16];
        match self.stream.read(&mut chunk) {
            Ok(0) => Err(NatsError::Other(format!(
                "{}: connection closed",
                self.host
            ))),
            Ok(n) => {
                self.buf.extend_from_slice(&chunk[..n]);
                Ok(())
            }
            Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                Err(NatsError::Timeout(format!("{}: timeout", self.host)))
            }
            Err(e) => Err(NatsError::Other(format!("{}: recv failed: {e}", self.host))),
        }
    }

    /// The next protocol operation; payload frames are consumed whole.
    fn next(&mut self, deadline: Instant) -> Result<Op> {
        loop {
            let Some(end) = self.buf.windows(2).position(|w| w == b"\r\n") else {
                self.fill(deadline)?;
                continue;
            };
            let line = String::from_utf8_lossy(&self.buf[..end]).to_string();
            let word = line.split(' ').next().unwrap_or("").to_ascii_uppercase();
            if word == "MSG" || word == "HMSG" {
                let parts: Vec<&str> = line.split(' ').collect();
                let (subject, sid) = (
                    parts.get(1).copied().unwrap_or(""),
                    parts.get(2).copied().unwrap_or(""),
                );
                let number =
                    |s: Option<&&str>| s.and_then(|s| s.parse::<usize>().ok()).unwrap_or(0);
                let (reply, header_len, total) = if word == "MSG" {
                    (
                        if parts.len() == 5 {
                            Some(parts[3])
                        } else {
                            None
                        },
                        0,
                        number(parts.last()),
                    )
                } else {
                    (
                        if parts.len() == 6 {
                            Some(parts[3])
                        } else {
                            None
                        },
                        number(parts.get(parts.len().saturating_sub(2))),
                        number(parts.last()),
                    )
                };
                if self.buf.len() < end + 2 + total + 2 {
                    self.fill(deadline)?;
                    continue;
                }
                let body = self.buf[end + 2..end + 2 + total].to_vec();
                let header_len = header_len.min(body.len());
                let message = Msg {
                    subject: subject.into(),
                    sid: sid.into(),
                    reply: reply.map(str::to_owned),
                    headers: body[..header_len].to_vec(),
                    body: body[header_len..].to_vec(),
                };
                self.buf.drain(..end + 2 + total + 2);
                return Ok(Op::Msg(message));
            }
            self.buf.drain(..end + 2);
            match word.as_str() {
                "PING" => self.send(b"PONG\r\n")?,
                "INFO" => {
                    return Ok(Op::Info(
                        serde_json::from_str(line.get(5..).unwrap_or("{}")).unwrap_or(Value::Null),
                    ))
                }
                "PONG" => return Ok(Op::Pong),
                "-ERR" => return Err(NatsError::Other(format!("{}: {line}", self.host))),
                _ => {}
            }
        }
    }

    pub fn next_message(&mut self, deadline: Instant) -> Result<Msg> {
        if let Some(message) = self.queue.pop_front() {
            return Ok(message);
        }
        loop {
            if let Op::Msg(message) = self.next(deadline)? {
                return Ok(message);
            }
        }
    }

    /// Publish with a reply subject and await the reply; other messages wait in `queue`.
    pub fn request(
        &mut self,
        subject: &str,
        payload: &[u8],
        timeout: f64,
    ) -> Result<(Option<u16>, Vec<u8>)> {
        self.token += 1;
        let reply = format!("{}.{}", self.inbox, self.token);
        let mut frame = format!("PUB {subject} {reply} {}\r\n", payload.len()).into_bytes();
        frame.extend_from_slice(payload);
        frame.extend_from_slice(b"\r\n");
        self.send(&frame)?;
        let deadline = Instant::now() + Duration::from_secs_f64(timeout);
        loop {
            if let Op::Msg(message) = self.next(deadline)? {
                if message.subject == reply {
                    return Ok((status(&message.headers), message.body));
                }
                self.queue.push_back(message);
            }
        }
    }

    /// A JetStream API request; 503 (no responders) is an error.
    pub fn js(&mut self, api: &str, body: Option<&Value>, timeout: f64) -> Result<Value> {
        let payload = body.map(Value::to_string).unwrap_or_default();
        let (code, reply) = self.request(&format!("$JS.API.{api}"), payload.as_bytes(), timeout)?;
        if code == Some(503) {
            return Err(NatsError::Other(format!(
                "{}: no responders for {api}",
                self.host
            )));
        }
        if reply.is_empty() {
            return Ok(serde_json::json!({}));
        }
        serde_json::from_slice(&reply).map_err(|e| NatsError::Other(format!("{}: {e}", self.host)))
    }
}

/// The status code of a `NATS/1.0 503` header block.
pub fn status(headers: &[u8]) -> Option<u16> {
    if headers.is_empty() {
        return None;
    }
    let text = String::from_utf8_lossy(headers);
    let first = text.split("\r\n").next().unwrap_or("");
    first
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse().ok())
}

impl Drop for Nats {
    fn drop(&mut self) {
        let _ = self.stream.shutdown(std::net::Shutdown::Both);
    }
}
