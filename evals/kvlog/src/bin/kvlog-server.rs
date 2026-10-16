//! `PUT key value` and `GET key`, one request per line. Every PUT is appended
//! to the log. With KVLOG_BUG=1 (the default) a PUT is acknowledged at once and
//! a background thread fsyncs the log every 50ms; otherwise the PUT is fsynced
//! before its acknowledgement.
use seedbox_eval_kvlog::{PORT, WAL};
use serde_json::json;
use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::time::Duration;

struct Store {
    log: Mutex<File>,
    values: Mutex<BTreeMap<String, String>>,
    buggy: bool,
}

/// Replay complete log lines; a torn final line from a crash is ignored.
fn recover() -> BTreeMap<String, String> {
    let mut values = BTreeMap::new();
    let Ok(text) = std::fs::read_to_string(WAL) else {
        return values;
    };
    let complete = text.rfind('\n').map_or("", |end| &text[..end]);
    for line in complete.lines() {
        if let Some((key, value)) = line.split_once(' ') {
            values.insert(key.to_string(), value.to_string());
        }
    }
    values
}

fn serve(store: Arc<Store>, stream: TcpStream) -> std::io::Result<()> {
    let mut writer = stream.try_clone()?;
    for line in BufReader::new(stream).lines() {
        let line = line?;
        let reply = match line.split_whitespace().collect::<Vec<_>>().as_slice() {
            ["PUT", key, value] => {
                let mut log = store.log.lock().unwrap();
                writeln!(log, "{key} {value}")?;
                if !store.buggy {
                    log.sync_data()?;
                }
                store.values.lock().unwrap().insert(key.to_string(), value.to_string());
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
        writer.write_all(reply.as_bytes())?;
        writer.write_all(b"\n")?;
    }
    Ok(())
}

fn main() -> std::io::Result<()> {
    let buggy = std::env::var("KVLOG_BUG").map_or(true, |value| value != "0");
    let values = recover();
    let recovered = values.len();
    let log = OpenOptions::new().create(true).append(true).open(WAL)?;
    let store = Arc::new(Store {
        log: Mutex::new(log),
        values: Mutex::new(values),
        buggy,
    });
    if buggy {
        let store = store.clone();
        std::thread::spawn(move || loop {
            std::thread::sleep(Duration::from_millis(50));
            let _ = store.log.lock().unwrap().sync_data();
        });
    }
    let listener = TcpListener::bind(("0.0.0.0", PORT))?;
    seedbox_sdk::event("recovered", json!({"keys": recovered}));
    seedbox_sdk::setup_complete(json!({"buggy": buggy}));
    for stream in listener.incoming() {
        let store = store.clone();
        let stream = stream?;
        std::thread::spawn(move || {
            if let Err(error) = serve(store, stream) {
                eprintln!("kvlog-server: connection: {error}");
            }
        });
    }
    Ok(())
}
