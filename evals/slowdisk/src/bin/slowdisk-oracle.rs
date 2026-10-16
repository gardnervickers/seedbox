//! Bursts of writes with pauses between, riding out server crashes, then a
//! check that every acknowledged write is still there. Exits 0 unless the
//! workload itself could not run; correctness is reported through properties.
use seedbox_eval_slowdisk::PORT;
use serde_json::json;
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
use std::time::Duration;

const BURSTS: usize = 40;
const BURST: usize = 5;

struct Connection {
    reader: BufReader<TcpStream>,
    writer: TcpStream,
}

impl Connection {
    fn open(address: &str) -> std::io::Result<Self> {
        let stream = TcpStream::connect(address)?;
        // Guest time: a crashed server never answers, so give up and reconnect.
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

/// Reconnect for up to about five seconds of guest time. A store whose disk
/// keeps failing may stop for good; that is fail-stop, not lost data.
fn connect(address: &str) -> Option<Connection> {
    for _ in 0..250 {
        match Connection::open(address) {
            Ok(connection) => return Some(connection),
            Err(_) => std::thread::sleep(Duration::from_millis(20)),
        }
    }
    None
}

fn main() {
    let address = std::env::var("SLOWDISK_ADDRESS").unwrap_or_else(|_| format!("store:{PORT}"));
    seedbox_sdk::declare(
        "acknowledged writes survive crashes",
        seedbox_sdk::Kind::Always,
    );
    seedbox_sdk::declare("a write was interrupted", seedbox_sdk::Kind::Sometimes);
    seedbox_sdk::declare(
        "acknowledged writes were verified",
        seedbox_sdk::Kind::Sometimes,
    );
    let Some(mut connection) = connect(&address) else {
        eprintln!("slowdisk-oracle: store never became reachable");
        std::process::exit(1);
    };
    seedbox_sdk::setup_complete(serde_json::Value::Null);
    let mut acknowledged = BTreeMap::new();
    let mut interrupted = 0;
    let mut available = true;
    'bursts: for burst in 0..BURSTS {
        for i in 0..BURST {
            let key = format!("k{burst}-{i}");
            let value = format!("v{burst}-{i}");
            match connection.request(&format!("PUT {key} {value}")) {
                Ok(reply) if reply == "OK" => {
                    acknowledged.insert(key, value);
                }
                _ => {
                    // Unacknowledged: it may or may not have been applied.
                    interrupted += 1;
                    match connect(&address) {
                        Some(next) => connection = next,
                        None => {
                            available = false;
                            break 'bursts;
                        }
                    }
                }
            }
        }
        std::thread::sleep(Duration::from_millis(400));
    }
    seedbox_sdk::sometimes(
        "a write was interrupted",
        interrupted > 0,
        json!({"interrupted": interrupted}),
    );
    // Read back without new generated faults; any still active end on schedule.
    if let Err(error) = seedbox_sdk::control::quiet(Duration::from_secs(60)) {
        eprintln!("slowdisk-oracle: no quiet period: {error}");
    }
    let mut lost = Vec::new();
    for (key, value) in &acknowledged {
        let reply = loop {
            if !available {
                break None;
            }
            match connection.request(&format!("GET {key}")) {
                Ok(reply) => break Some(reply),
                Err(_) => match connect(&address) {
                    Some(next) => connection = next,
                    None => available = false,
                },
            }
        };
        match reply {
            Some(reply) if &reply != value => lost.push(key.clone()),
            Some(_) => {}
            None => break,
        }
    }
    seedbox_sdk::sometimes(
        "acknowledged writes were verified",
        available,
        json!({"acknowledged": acknowledged.len()}),
    );
    if !available {
        // A partial read-back proves nothing, except a write already
        // found lost.
        if !lost.is_empty() {
            seedbox_sdk::always(
                "acknowledged writes survive crashes",
                false,
                json!({"lost": lost.len(), "acknowledged": acknowledged.len(), "first": lost.first(), "partial": true}),
            );
        }
        seedbox_sdk::flush().expect("flush properties");
        println!(
            "store unavailable; acknowledged {}, interrupted {interrupted}",
            acknowledged.len()
        );
        return;
    }
    seedbox_sdk::always(
        "acknowledged writes survive crashes",
        lost.is_empty(),
        json!({"lost": lost.len(), "acknowledged": acknowledged.len(), "first": lost.first()}),
    );
    seedbox_sdk::flush().expect("flush properties");
    println!(
        "acknowledged {}, lost {}, interrupted {interrupted}",
        acknowledged.len(),
        lost.len()
    );
}
