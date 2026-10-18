//! The oracle kit: evidence output, simulated-time clock, seeded randomness,
//! the World connection (or its native engine shim), a single-threaded
//! serve-while-waiting loop, and small wire helpers (HTTP, base64).
//!
//! An oracle prints one `EVIDENCE {json}` line per observation, `H {json}`
//! history lines, and finally exactly one `VERDICT ...` line; its exit status
//! is [`OK`], [`VIOLATION`] or [`INCONCLUSIVE`]. Under Seedbox its declared
//! check ([`declare_check`]) is also a guest `always` property: passed on OK,
//! violated on a violation, and left unevaluated when inconclusive.
pub mod base64;
pub mod http;
pub mod rng;
pub mod serve;
pub mod world;

use serde_json::{Map, Value};
use std::{
    fmt,
    io::Write,
    sync::{Mutex, OnceLock},
    time::Instant,
};

pub use rng::Rng;
pub use world::{connect_world, Ctl};

/// Every check held.
pub const OK: u8 = 0;
/// A checked property was violated: the bug reproduced.
pub const VIOLATION: u8 = 1;
/// The run cannot decide (no acknowledged operation, an unread node, ...).
pub const INCONCLUSIVE: u8 = 3;

/// A failure an oracle step reports; callers decide whether it is fatal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error(pub String);

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for Error {}

impl From<String> for Error {
    fn from(message: String) -> Self {
        Self(message)
    }
}
impl From<&str> for Error {
    fn from(message: &str) -> Self {
        Self(message.into())
    }
}
impl From<std::io::Error> for Error {
    fn from(error: std::io::Error) -> Self {
        Self(error.to_string())
    }
}
impl From<serde_json::Error> for Error {
    fn from(error: serde_json::Error) -> Self {
        Self(error.to_string())
    }
}
impl From<seedbox_control_client::Error> for Error {
    fn from(error: seedbox_control_client::Error) -> Self {
        Self(error.to_string())
    }
}

pub type Result<T> = std::result::Result<T, Error>;

/// Fail with a formatted [`Error`].
#[macro_export]
macro_rules! bail {
    ($($arg:tt)*) => {
        return Err($crate::kit::Error(format!($($arg)*)))
    };
}

fn start() -> Instant {
    static START: OnceLock<Instant> = OnceLock::new();
    *START.get_or_init(Instant::now)
}

/// Start the oracle's clock; [`now`] counts from the first call of either.
pub fn start_clock() {
    start();
}

/// Seconds since the oracle started, in milliseconds (simulated time in a guest).
pub fn now() -> f64 {
    (start().elapsed().as_secs_f64() * 1000.0).round() / 1000.0
}

/// Seconds since the oracle started, in microseconds.
pub fn now_us() -> f64 {
    (start().elapsed().as_secs_f64() * 1e6).round() / 1e6
}

static OUTPUT: Mutex<()> = Mutex::new(());

/// Print one whole line to stdout and flush it, never interleaved with another.
pub fn emit(line: &str) {
    let _guard = OUTPUT.lock().unwrap_or_else(|e| e.into_inner());
    let mut stdout = std::io::stdout().lock();
    let _ = stdout.write_all(line.as_bytes());
    let _ = stdout.write_all(b"\n");
    let _ = stdout.flush();
}

/// `EVIDENCE {"kind": kind, "t": now, ...fields}` with sorted keys.
pub fn evidence(kind: &str, fields: Value) {
    let mut object = match fields {
        Value::Object(object) => object,
        Value::Null => Map::new(),
        other => {
            let mut object = Map::new();
            object.insert("value".into(), other);
            object
        }
    };
    object.insert("kind".into(), kind.into());
    object.insert("t".into(), now().into());
    emit(&format!("EVIDENCE {}", Value::Object(object)));
}

/// One history line, `H {json}`.
pub fn history_line(entry: &Value) {
    emit(&format!("H {entry}"));
}

/// Print the final `VERDICT` line and return the exit status for it.
pub fn verdict(code: u8, text: &str) -> u8 {
    let word = match code {
        OK => "ok",
        VIOLATION => "violation",
        _ => "inconclusive",
    };
    emit(&format!("VERDICT {word}: {text}"));
    if let Some(check) = CHECK.get() {
        if code == OK || code == VIOLATION {
            seedbox_sdk::always(check, code == OK, serde_json::json!({ "verdict": text }));
        }
        let _ = seedbox_sdk::flush();
    }
    code
}

/// Running in a Seedbox guest, whose supervisor serves the property socket.
pub fn under_seedbox() -> bool {
    std::path::Path::new(seedbox_sdk::SOCKET).exists()
}

/// The property an oracle's verdict evaluates, once declared.
static CHECK: OnceLock<String> = OnceLock::new();

/// Declare the oracle's check as a guest `always` property; [`verdict`]
/// evaluates it.
pub fn declare_check(name: &str) {
    if CHECK.set(name.to_owned()).is_ok() {
        seedbox_sdk::declare(name, seedbox_sdk::Kind::Always);
    }
}

/// How much work a run's clients completed. A system that stops serving
/// passes every safety check vacuously, so an oracle compares this with the
/// throughput a healthy run reaches before it calls a clean history valid.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Progress {
    /// Operations that completed successfully (commits, acknowledgements, ...).
    pub done: usize,
    /// Clients issuing operations concurrently.
    pub clients: usize,
    /// Seconds the clients ran.
    pub seconds: f64,
}

impl Progress {
    /// Completed operations per client-second.
    pub fn rate(&self) -> f64 {
        let client_seconds = self.clients as f64 * self.seconds;
        if client_seconds > 0.0 {
            self.done as f64 / client_seconds
        } else {
            0.0
        }
    }

    /// Why the run cannot decide, when it completed fewer than `minimum`
    /// `noun` per client-second; `None` when it reached `minimum` (or
    /// `minimum` is zero, which disables the check).
    pub fn shortfall(&self, minimum: f64, noun: &str) -> Option<String> {
        (minimum > 0.0 && self.rate() < minimum).then(|| {
            format!(
                "insufficient progress: {} {noun} by {} clients in {} s ({:.3} per client-second, minimum {minimum})",
                self.done,
                self.clients,
                self.seconds,
                self.rate()
            )
        })
    }
}

/// Lines of a saved oracle log (or a split `seedbox logs` file) that carry
/// `prefix`, parsed as JSON. Unparseable lines are skipped.
pub fn tagged_lines(text: &str, prefix: &str) -> Vec<Value> {
    text.lines()
        .filter_map(|line| line.strip_prefix(prefix))
        .filter_map(|json| serde_json::from_str(json).ok())
        .collect()
}

/// The evidence document of `kind` in a saved log, if any (the last one).
pub fn evidence_of(text: &str, kind: &str) -> Option<Value> {
    tagged_lines(text, "EVIDENCE ")
        .into_iter()
        .rev()
        .find(|e| e.get("kind").and_then(Value::as_str) == Some(kind))
}

/// Sleep on this thread (simulated time in a guest).
pub fn pause(seconds: f64) {
    if seconds > 0.0 {
        std::thread::sleep(std::time::Duration::from_secs_f64(seconds));
    }
}

/// `[a-b]` ranges of sorted unique integers, as the queue checkers report them.
pub fn ranges(numbers: impl IntoIterator<Item = u64>) -> Vec<Value> {
    let mut numbers: Vec<u64> = numbers.into_iter().collect();
    numbers.sort_unstable();
    numbers.dedup();
    let mut spans: Vec<(u64, u64)> = Vec::new();
    for number in numbers {
        match spans.last_mut() {
            Some((_, end)) if number == *end + 1 => *end = number,
            _ => spans.push((number, number)),
        }
    }
    spans
        .into_iter()
        .map(|(a, b)| {
            if a == b {
                Value::from(a)
            } else {
                Value::from(format!("{a}-{b}"))
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranges_join_consecutive_numbers() {
        assert_eq!(
            Value::from(ranges([5, 1, 2, 3, 7, 3])),
            serde_json::json!(["1-3", 5, 7])
        );
    }

    #[test]
    fn progress_below_the_minimum_rate_is_a_shortfall() {
        let stalled = Progress {
            done: 21,
            clients: 20,
            seconds: 15.0,
        };
        assert!((stalled.rate() - 0.07).abs() < 1e-9);
        assert_eq!(
            stalled.shortfall(0.25, "commits").as_deref(),
            Some("insufficient progress: 21 commits by 20 clients in 15 s (0.070 per client-second, minimum 0.25)")
        );
        assert_eq!(stalled.shortfall(0.0, "commits"), None);
        let healthy = Progress {
            done: 258,
            clients: 20,
            seconds: 15.0,
        };
        assert_eq!(healthy.shortfall(0.25, "commits"), None);
        // Exactly at the minimum suffices; no clients or no time is no progress.
        let edge = Progress {
            done: 75,
            clients: 20,
            seconds: 15.0,
        };
        assert_eq!(edge.shortfall(0.25, "commits"), None);
        let idle = Progress {
            done: 5,
            clients: 0,
            seconds: 15.0,
        };
        assert_eq!(idle.rate(), 0.0);
        assert!(idle.shortfall(0.25, "commits").is_some());
    }
}
