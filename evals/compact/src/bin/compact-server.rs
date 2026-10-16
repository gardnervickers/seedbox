//! `PUT key value` and `GET key`, one request per line. Every PUT is appended
//! to the log and fsynced before its acknowledgement. On startup the server
//! loads the snapshot, replays the log, and compacts: it writes the merged
//! state to a new snapshot, renames it into place and empties the log. With
//! COMPACT_BUG=1 (the default) it empties the log first, opening a window in
//! which a crash leaves neither the log nor the new snapshot.
use seedbox_eval_compact::{PORT, SNAPSHOT, SNAPSHOT_TEMP, WAL};
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
}

/// Complete `key value` lines; a torn final line from a crash is ignored.
fn load(path: &str, values: &mut BTreeMap<String, String>) -> usize {
    let Ok(text) = std::fs::read_to_string(path) else {
        return 0;
    };
    let complete = text.rfind('\n').map_or("", |end| &text[..end]);
    let mut count = 0;
    for line in complete.lines() {
        if let Some((key, value)) = line.split_once(' ') {
            values.insert(key.to_string(), value.to_string());
            count += 1;
        }
    }
    count
}

fn sync_directory() -> std::io::Result<()> {
    File::open("/data")?.sync_all()
}

/// Write the merged state as the new snapshot. Writing is deliberately
/// paced like a large store's compaction, so it takes guest time.
fn install_snapshot(values: &BTreeMap<String, String>) -> std::io::Result<()> {
    let mut file = File::create(SNAPSHOT_TEMP)?;
    for (i, (key, value)) in values.iter().enumerate() {
        writeln!(file, "{key} {value}")?;
        if i % 8 == 7 {
            std::thread::sleep(Duration::from_millis(2));
        }
    }
    file.sync_all()?;
    std::fs::rename(SNAPSHOT_TEMP, SNAPSHOT)?;
    sync_directory()
}

fn empty_log() -> std::io::Result<()> {
    let log = OpenOptions::new().write(true).create(true).open(WAL)?;
    log.set_len(0)?;
    log.sync_all()
}

fn recover(buggy: bool) -> std::io::Result<BTreeMap<String, String>> {
    let mut values = BTreeMap::new();
    load(SNAPSHOT, &mut values);
    let replayed = load(WAL, &mut values);
    if replayed > 0 {
        seedbox_sdk::event("compacting", json!({"replayed": replayed}));
        if buggy {
            empty_log()?;
            install_snapshot(&values)?;
        } else {
            install_snapshot(&values)?;
            empty_log()?;
        }
        seedbox_sdk::event("compacted", json!({"keys": values.len()}));
    }
    Ok(values)
}

fn serve(store: Arc<Store>, stream: TcpStream) -> std::io::Result<()> {
    let mut writer = stream.try_clone()?;
    for line in BufReader::new(stream).lines() {
        let line = line?;
        let reply = match line.split_whitespace().collect::<Vec<_>>().as_slice() {
            ["PUT", key, value] => {
                let mut log = store.log.lock().unwrap();
                writeln!(log, "{key} {value}")?;
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
        writer.write_all(reply.as_bytes())?;
        writer.write_all(b"\n")?;
    }
    Ok(())
}

fn main() -> std::io::Result<()> {
    let buggy = std::env::var("COMPACT_BUG").map_or(true, |value| value != "0");
    let values = recover(buggy)?;
    let log = OpenOptions::new().create(true).append(true).open(WAL)?;
    let store = Arc::new(Store {
        log: Mutex::new(log),
        values: Mutex::new(values),
    });
    let listener = TcpListener::bind(("0.0.0.0", PORT))?;
    seedbox_sdk::setup_complete(json!({"buggy": buggy}));
    for stream in listener.incoming() {
        let store = store.clone();
        let stream = stream?;
        std::thread::spawn(move || {
            if let Err(error) = serve(store, stream) {
                eprintln!("compact-server: connection: {error}");
            }
        });
    }
    Ok(())
}
