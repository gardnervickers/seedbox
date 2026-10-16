//! Bursts of writes with pauses between, riding out server crashes, then a
//! check that every acknowledged write is still there. Exits 0 unless the
//! workload itself could not run; correctness is reported through properties.
use seedbox_eval_kvlog::PORT;
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
        Ok(Self { writer: stream.try_clone()?, reader: BufReader::new(stream) })
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

fn connect(address: &str) -> Connection {
    loop {
        match Connection::open(address) {
            Ok(connection) => return connection,
            Err(_) => std::thread::sleep(Duration::from_millis(20)),
        }
    }
}

fn main() {
    let address = std::env::var("KVLOG_ADDRESS").unwrap_or_else(|_| format!("kv:{PORT}"));
    seedbox_sdk::declare("acknowledged writes survive crashes", seedbox_sdk::Kind::Always);
    seedbox_sdk::declare("a write was interrupted", seedbox_sdk::Kind::Sometimes);
    let mut connection = connect(&address);
    seedbox_sdk::setup_complete(serde_json::Value::Null);
    let mut acknowledged = BTreeMap::new();
    let mut interrupted = 0;
    for burst in 0..BURSTS {
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
                    connection = connect(&address);
                }
            }
        }
        std::thread::sleep(Duration::from_millis(400));
    }
    seedbox_sdk::sometimes("a write was interrupted", interrupted > 0, json!({"interrupted": interrupted}));
    let mut lost = Vec::new();
    for (key, value) in &acknowledged {
        let reply = loop {
            match connection.request(&format!("GET {key}")) {
                Ok(reply) => break reply,
                Err(_) => connection = connect(&address),
            }
        };
        if &reply != value {
            lost.push(key.clone());
        }
    }
    seedbox_sdk::always(
        "acknowledged writes survive crashes",
        lost.is_empty(),
        json!({"lost": lost.len(), "acknowledged": acknowledged.len(), "first": lost.first()}),
    );
    seedbox_sdk::flush().expect("flush properties");
    println!("acknowledged {}, lost {}, interrupted {interrupted}", acknowledged.len(), lost.len());
}
