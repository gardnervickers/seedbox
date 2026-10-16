//! Clients append unique values, riding out server crashes, then read the log
//! back: no value may appear twice and every acknowledged value must be there.
//! Each client's pacing and its response to an append with an unknown outcome
//! (retry it with the same request ID, or give up) are engine choices, so a
//! seed or a branch decides them; RETRY_ALWAYS=1 always retries instead, for
//! validating the checker. Exits 0 unless the workload could not run.
use seedbox_check::duplicates;
use seedbox_eval_retry::PORT;
use seedbox_sdk::workload::Decisions;
use serde_json::json;
use std::collections::BTreeSet;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
use std::time::Duration;

const CLIENTS: u32 = 1;
const APPENDS: u64 = 60;
const ATTEMPTS: usize = 5;

struct Connection {
    reader: BufReader<TcpStream>,
    writer: TcpStream,
}

impl Connection {
    fn open(address: &str) -> std::io::Result<Self> {
        let stream = TcpStream::connect(address)?;
        // Guest time: a crashed server never answers.
        stream.set_read_timeout(Some(Duration::from_millis(500)))?;
        Ok(Self {
            writer: stream.try_clone()?,
            reader: BufReader::new(stream),
        })
    }
    fn request(&mut self, line: &str) -> std::io::Result<String> {
        self.writer.write_all(format!("{line}\n").as_bytes())?;
        let mut reply = String::new();
        if self.reader.read_line(&mut reply)? == 0 {
            return Err(std::io::ErrorKind::UnexpectedEof.into());
        }
        Ok(reply.trim().to_string())
    }
}

/// Reconnect for up to about five seconds of guest time.
fn connect(address: &str) -> Option<Connection> {
    for _ in 0..250 {
        match Connection::open(address) {
            Ok(connection) => return Some(connection),
            Err(_) => std::thread::sleep(Duration::from_millis(20)),
        }
    }
    None
}

#[derive(Default)]
struct Outcome {
    acknowledged: Vec<u64>,
    ambiguous: usize,
    retried: usize,
}

fn client(id: u32, address: String, always: bool) -> Outcome {
    let decisions = Decisions::new(&format!("client-{id}"));
    let mut outcome = Outcome::default();
    let Some(mut connection) = connect(&address) else {
        return outcome;
    };
    for seq in 0..APPENDS {
        let pause = *decisions.pick("pace", &[("fast", 10), ("steady", 50), ("slow", 200)]);
        std::thread::sleep(Duration::from_millis(pause));
        let value = u64::from(id) * 1_000_000 + seq;
        let line = format!("APPEND {id} {seq} {value}");
        for attempt in 0..ATTEMPTS {
            if connection.request(&line).is_ok_and(|reply| reply == "OK") {
                outcome.acknowledged.push(value);
                break;
            }
            if attempt == 0 {
                outcome.ambiguous += 1;
                seedbox_sdk::event("ambiguous", json!({"client": id, "seq": seq}));
            }
            let retry = always || decisions.flag("retry");
            match connect(&address) {
                Some(fresh) => connection = fresh,
                None => return outcome,
            }
            if !retry {
                break;
            }
            outcome.retried += 1;
        }
    }
    outcome
}

fn main() {
    let address = std::env::var("RETRY_ADDRESS").unwrap_or_else(|_| format!("log:{PORT}"));
    seedbox_sdk::declare("no append applied twice", seedbox_sdk::Kind::Always);
    seedbox_sdk::declare("acknowledged appends survive", seedbox_sdk::Kind::Always);
    seedbox_sdk::declare(
        "an append's outcome was unknown",
        seedbox_sdk::Kind::Sometimes,
    );
    seedbox_sdk::declare(
        "an unknown append was retried",
        seedbox_sdk::Kind::Sometimes,
    );
    if connect(&address).is_none() {
        eprintln!("retry-oracle: log never became reachable");
        std::process::exit(1);
    }
    seedbox_sdk::setup_complete(serde_json::Value::Null);
    let always = std::env::var("RETRY_ALWAYS").is_ok_and(|value| value == "1");
    let clients: Vec<_> = (0..CLIENTS)
        .map(|id| {
            let address = address.clone();
            std::thread::spawn(move || client(id, address, always))
        })
        .collect();
    let outcomes: Vec<Outcome> = clients
        .into_iter()
        .map(|c| c.join().unwrap_or_default())
        .collect();
    let ambiguous: usize = outcomes.iter().map(|o| o.ambiguous).sum();
    let retried: usize = outcomes.iter().map(|o| o.retried).sum();
    seedbox_sdk::sometimes(
        "an append's outcome was unknown",
        ambiguous > 0,
        json!(null),
    );
    seedbox_sdk::sometimes("an unknown append was retried", retried > 0, json!(null));

    let reply = connect(&address).and_then(|mut c| c.request("READ").ok());
    let Some(reply) = reply else {
        eprintln!("retry-oracle: final read failed");
        seedbox_sdk::flush().expect("flush properties");
        return;
    };
    let values: Vec<u64> = reply.split(',').filter_map(|v| v.parse().ok()).collect();
    let twice = duplicates(&values);
    seedbox_sdk::always(
        "no append applied twice",
        twice.is_empty(),
        json!({"values": values.len(), "duplicated": twice.len(), "first": twice.first()}),
    );
    let present: BTreeSet<u64> = values.iter().copied().collect();
    let lost: Vec<u64> = outcomes
        .iter()
        .flat_map(|o| o.acknowledged.iter().copied())
        .filter(|v| !present.contains(v))
        .collect();
    seedbox_sdk::always(
        "acknowledged appends survive",
        lost.is_empty(),
        json!({"lost": lost.len(), "first": lost.first()}),
    );
    seedbox_sdk::flush().expect("flush properties");
    println!(
        "values {}, unknown {ambiguous}, retried {retried}, duplicated {}",
        values.len(),
        twice.len()
    );
}
