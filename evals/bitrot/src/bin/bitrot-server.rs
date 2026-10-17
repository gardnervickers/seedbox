//! `PUT key value` and `GET key`, one request per line. Every PUT is appended
//! to the log with its CRC-32 and fsynced before `OK`. On start the log is
//! replayed; with BITROT_BUG=1 (the default) the checksums are not checked.
use seedbox_eval_bitrot::{parse, record, LOG, PORT};
use serde_json::json;
use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};

struct Store {
    log: Mutex<File>,
    values: Mutex<BTreeMap<String, String>>,
}

/// Replay complete lines; a torn final line is ignored, and with `checked`
/// so is any line whose checksum fails.
fn recover(checked: bool) -> (BTreeMap<String, String>, usize) {
    let mut values = BTreeMap::new();
    let mut rejected = 0;
    let Ok(bytes) = std::fs::read(LOG) else {
        return (values, 0);
    };
    let text = String::from_utf8_lossy(&bytes);
    let complete = text.rfind('\n').map_or("", |end| &text[..end]);
    for line in complete.lines() {
        match parse(line, checked) {
            Some((key, value)) => {
                values.insert(key.to_string(), value.to_string());
            }
            None => rejected += 1,
        }
    }
    (values, rejected)
}

fn serve(store: Arc<Store>, stream: TcpStream) -> std::io::Result<()> {
    let mut writer = stream.try_clone()?;
    for line in BufReader::new(stream).lines() {
        let line = line?;
        let reply = match line.split_whitespace().collect::<Vec<_>>().as_slice() {
            ["PUT", key, value] => {
                let mut log = store.log.lock().unwrap();
                log.write_all(record(key, value).as_bytes())?;
                log.sync_data()?;
                store
                    .values
                    .lock()
                    .unwrap()
                    .insert(key.to_string(), value.to_string());
                "OK".to_string()
            }
            ["GET", key] => store
                .values
                .lock()
                .unwrap()
                .get(*key)
                .cloned()
                .unwrap_or_else(|| "NONE".into()),
            _ => "ERR".to_string(),
        };
        writer.write_all(reply.replace('\n', " ").as_bytes())?;
        writer.write_all(b"\n")?;
    }
    Ok(())
}

fn main() -> std::io::Result<()> {
    let buggy = std::env::var("BITROT_BUG").map_or(true, |value| value != "0");
    let (values, rejected) = recover(!buggy);
    let recovered = values.len();
    let log = OpenOptions::new().create(true).append(true).open(LOG)?;
    let store = Arc::new(Store {
        log: Mutex::new(log),
        values: Mutex::new(values),
    });
    let listener = TcpListener::bind(("0.0.0.0", PORT))?;
    seedbox_sdk::event("recovered", json!({"keys": recovered, "rejected": rejected}));
    seedbox_sdk::setup_complete(json!({"buggy": buggy}));
    for stream in listener.incoming() {
        let store = store.clone();
        let stream = stream?;
        std::thread::spawn(move || {
            if let Err(error) = serve(store, stream) {
                eprintln!("bitrot-server: connection: {error}");
            }
        });
    }
    Ok(())
}
