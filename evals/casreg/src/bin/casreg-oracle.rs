//! Concurrent clients drive reads, writes and compare-and-sets over a small
//! value range, recording every invocation and response; the history must be
//! linearizable. A request that times out keeps an unknown outcome.
use seedbox_check::{check_register, Call, History, Ret, Verdict};
use seedbox_eval_casreg::PORT;
use serde_json::json;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::sync::Arc;
use std::time::Duration;

const CLIENTS: u32 = 3;
const OPERATIONS: usize = 120;
/// Search budget for the history check; beyond it the check is inconclusive.
const CHECK_STEPS: u64 = 5_000_000;

fn connect(address: &str) -> Option<(BufReader<TcpStream>, TcpStream)> {
    for _ in 0..250 {
        if let Ok(stream) = TcpStream::connect(address) {
            stream
                .set_read_timeout(Some(Duration::from_millis(500)))
                .ok()?;
            return Some((BufReader::new(stream.try_clone().ok()?), stream));
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    None
}

/// The guest's controlled entropy seeds each client's workload.
fn seed() -> u64 {
    let mut bytes = [0u8; 8];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut bytes))
        .expect("guest entropy");
    u64::from_le_bytes(bytes) | 1
}

fn client(id: u32, address: String, history: Arc<History>) -> usize {
    let Some((mut reader, mut writer)) = connect(&address) else {
        return 0;
    };
    let mut state = seed();
    let mut next = move |bound: u64| {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state % bound
    };
    let mut unknown = 0;
    for _ in 0..OPERATIONS {
        let call = match next(10) {
            0..=2 => Call::Read,
            3 => Call::Write(next(3) as i64),
            _ => Call::Cas {
                expected: next(3) as i64,
                new: next(3) as i64,
            },
        };
        let line = match call {
            Call::Read => "READ".to_string(),
            Call::Write(v) => format!("WRITE {v}"),
            Call::Cas { expected, new } => format!("CAS {expected} {new}"),
        };
        let token = history.invoke(id, call);
        let mut reply = String::new();
        let answered = writer.write_all(format!("{line}\n").as_bytes()).is_ok()
            && reader.read_line(&mut reply).is_ok_and(|n| n > 0);
        let reply = reply.trim();
        let ret = match (call, reply) {
            (_, _) if !answered => None,
            (Call::Read, "NONE") => Some(Ret::Read(None)),
            (Call::Read, value) => value.parse().ok().map(|v| Ret::Read(Some(v))),
            (Call::Write(_), "OK") => Some(Ret::Write),
            (Call::Cas { .. }, "OK") => Some(Ret::Cas(true)),
            (Call::Cas { .. }, "FAIL") => Some(Ret::Cas(false)),
            _ => None,
        };
        match ret {
            Some(ret) => history.respond(token, ret),
            None => {
                // Unknown outcome; continue on a fresh connection.
                unknown += 1;
                match connect(&address) {
                    Some((r, w)) => (reader, writer) = (r, w),
                    None => break,
                }
            }
        }
    }
    unknown
}

fn main() {
    let address = std::env::var("CASREG_ADDRESS").unwrap_or_else(|_| format!("register:{PORT}"));
    seedbox_sdk::declare("history is linearizable", seedbox_sdk::Kind::Always);
    seedbox_sdk::declare("compare-and-sets raced", seedbox_sdk::Kind::Sometimes);
    if connect(&address).is_none() {
        eprintln!("casreg-oracle: register never became reachable");
        std::process::exit(1);
    }
    seedbox_sdk::setup_complete(serde_json::Value::Null);
    let history = Arc::new(History::new());
    let clients: Vec<_> = (0..CLIENTS)
        .map(|id| {
            let (address, history) = (address.clone(), history.clone());
            std::thread::spawn(move || client(id, address, history))
        })
        .collect();
    let unknown: usize = clients.into_iter().map(|c| c.join().unwrap_or(0)).sum();
    let ops = history.ops();
    // Overlapping successful CASes: the contention the check needs.
    let raced = ops.iter().enumerate().any(|(i, a)| {
        ops[i + 1..].iter().any(|b| {
            matches!((a.call, b.call), (Call::Cas { .. }, Call::Cas { .. }))
                && a.response.is_some_and(|(end, _)| b.invoke < end)
        })
    });
    seedbox_sdk::sometimes("compare-and-sets raced", raced, json!(null));
    match check_register(None, &ops, CHECK_STEPS) {
        Verdict::Linearizable => {
            seedbox_sdk::always("history is linearizable", true, json!({"ops": ops.len()}))
        }
        Verdict::Violation { stuck } => seedbox_sdk::always(
            "history is linearizable",
            false,
            json!({"ops": ops.len(), "stuck": stuck.len(), "first": stuck.first()}),
        ),
        // Leaves the obligation unmet: not passed, not violated.
        Verdict::Inconclusive { steps } => {
            eprintln!("casreg-oracle: check inconclusive after {steps} steps")
        }
    }
    seedbox_sdk::flush().expect("flush properties");
    println!("operations {}, unknown {unknown}", ops.len());
}
