//! `APPEND client seq value` and `READ`, one request per line. An append whose
//! `(client, seq)` was already applied is acknowledged without applying it
//! again. Each applied append is a log line `client seq value`, fsynced before
//! the acknowledgement. On startup the server replays the log; with
//! RETRY_BUG=0 it also rebuilds the applied request IDs from it, and with
//! RETRY_BUG=1 (the default) it starts with none. RETRY_ACK_DELAY_MS widens
//! the window between an append becoming durable and its acknowledgement,
//! for validating the checker against a known failing schedule.
use seedbox_eval_retry::{LOG, PORT};
use serde_json::json;
use std::collections::BTreeSet;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::time::Duration;

struct Store {
    log: File,
    applied: BTreeSet<(u32, u64)>,
    values: Vec<u64>,
}

/// Complete `client seq value` lines; a torn final line from a crash is ignored.
fn recover(buggy: bool) -> (BTreeSet<(u32, u64)>, Vec<u64>) {
    let (mut applied, mut values) = (BTreeSet::new(), Vec::new());
    let text = std::fs::read_to_string(LOG).unwrap_or_default();
    let complete = text.rfind('\n').map_or("", |end| &text[..end]);
    for line in complete.lines() {
        let fields: Vec<&str> = line.split(' ').collect();
        let [client, seq, value] = fields[..] else {
            continue;
        };
        let (Ok(client), Ok(seq), Ok(value)) = (client.parse(), seq.parse(), value.parse()) else {
            continue;
        };
        if !buggy {
            applied.insert((client, seq));
        }
        values.push(value);
    }
    (applied, values)
}

fn append(store: &Mutex<Store>, client: u32, seq: u64, value: u64) -> std::io::Result<()> {
    let mut store = store.lock().unwrap();
    if store.applied.contains(&(client, seq)) {
        return Ok(());
    }
    writeln!(store.log, "{client} {seq} {value}")?;
    store.log.sync_data()?;
    store.applied.insert((client, seq));
    store.values.push(value);
    Ok(())
}

fn serve(store: Arc<Mutex<Store>>, stream: TcpStream, delay: Duration) -> std::io::Result<()> {
    let mut writer = stream.try_clone()?;
    for line in BufReader::new(stream).lines() {
        let line = line?;
        let reply = match line.split_whitespace().collect::<Vec<_>>().as_slice() {
            ["APPEND", client, seq, value] => match (client.parse(), seq.parse(), value.parse()) {
                (Ok(client), Ok(seq), Ok(value)) => {
                    append(&store, client, seq, value)?;
                    std::thread::sleep(delay);
                    "OK".to_string()
                }
                _ => "ERR".to_string(),
            },
            ["READ"] => {
                let values = &store.lock().unwrap().values;
                if values.is_empty() {
                    "EMPTY".to_string()
                } else {
                    let values: Vec<String> = values.iter().map(u64::to_string).collect();
                    values.join(",")
                }
            }
            _ => "ERR".to_string(),
        };
        writer.write_all(reply.as_bytes())?;
        writer.write_all(b"\n")?;
    }
    Ok(())
}

fn main() -> std::io::Result<()> {
    let buggy = std::env::var("RETRY_BUG").map_or(true, |value| value != "0");
    let delay = std::env::var("RETRY_ACK_DELAY_MS")
        .ok()
        .and_then(|ms| ms.parse().ok())
        .map_or(Duration::ZERO, Duration::from_millis);
    seedbox_sdk::state("phase", "recovering");
    let (applied, values) = recover(buggy);
    seedbox_sdk::event(
        "recovered",
        json!({"values": values.len(), "request_ids": applied.len()}),
    );
    let log = OpenOptions::new().create(true).append(true).open(LOG)?;
    let store = Arc::new(Mutex::new(Store {
        log,
        applied,
        values,
    }));
    let listener = TcpListener::bind(("0.0.0.0", PORT))?;
    seedbox_sdk::state("phase", "serving");
    seedbox_sdk::setup_complete(json!({"buggy": buggy}));
    for stream in listener.incoming() {
        let store = store.clone();
        let stream = stream?;
        std::thread::spawn(move || {
            if let Err(error) = serve(store, stream, delay) {
                eprintln!("retry-server: connection: {error}");
            }
        });
    }
    Ok(())
}
