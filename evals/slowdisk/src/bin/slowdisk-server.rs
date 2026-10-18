//! `PUT key value` and `GET key`, one request per line. Each PUT appends to
//! the log and waits until the flusher has fsynced past it. With
//! SLOWDISK_BUG=1 (the default) the wait gives up after SLOWDISK_ACK_MS
//! (20 by default) and acknowledges the write before it is durable.
use seedbox_eval_slowdisk::{PORT, WAL};
use serde_json::json;
use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

#[derive(Default)]
struct Progress {
    written: u64,
    synced: u64,
}

struct Store {
    log: Mutex<File>,
    progress: Mutex<Progress>,
    changed: Condvar,
    values: Mutex<BTreeMap<String, String>>,
    /// The bounded wait, when the bug is planted.
    bound: Option<Duration>,
    reported: AtomicBool,
}

impl Store {
    fn put(&self, key: &str, value: &str) -> std::io::Result<()> {
        let mine = {
            let mut log = self.log.lock().unwrap();
            let line = format!("{key} {value}\n");
            log.write_all(line.as_bytes())?;
            let mut progress = self.progress.lock().unwrap();
            progress.written += line.len() as u64;
            progress.written
        };
        self.changed.notify_all();
        let progress = self.progress.lock().unwrap();
        let durable = |p: &mut Progress| p.synced < mine;
        let progress = match self.bound {
            Some(bound) => {
                self.changed
                    .wait_timeout_while(progress, bound, durable)
                    .unwrap()
                    .0
            }
            None => self.changed.wait_while(progress, durable).unwrap(),
        };
        if progress.synced < mine && !self.reported.swap(true, Ordering::Relaxed) {
            seedbox_sdk::event(
                "acknowledged before sync",
                json!({"behind": mine - progress.synced}),
            );
        }
        drop(progress);
        self.values
            .lock()
            .unwrap()
            .insert(key.to_string(), value.to_string());
        Ok(())
    }

    /// Fsync whatever has been written, in batches, forever.
    fn flush(&self, file: File) -> std::io::Result<()> {
        loop {
            let target = {
                let progress = self
                    .changed
                    .wait_while(self.progress.lock().unwrap(), |p| p.written == p.synced);
                progress.unwrap().written
            };
            file.sync_data()?;
            self.progress.lock().unwrap().synced = target;
            self.changed.notify_all();
        }
    }
}

/// Complete `key value` lines; a torn final line from a crash is ignored.
fn recover() -> BTreeMap<String, String> {
    let mut values = BTreeMap::new();
    let text = std::fs::read_to_string(WAL).unwrap_or_default();
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
                store.put(key, value)?;
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
    let buggy = std::env::var("SLOWDISK_BUG").map_or(true, |value| value != "0");
    let bound_ms = std::env::var("SLOWDISK_ACK_MS")
        .ok()
        .and_then(|ms| ms.parse().ok())
        .unwrap_or(20);
    let values = recover();
    let log = OpenOptions::new().create(true).append(true).open(WAL)?;
    let syncer = log.try_clone()?;
    let store = Arc::new(Store {
        log: Mutex::new(log),
        progress: Mutex::new(Progress::default()),
        changed: Condvar::new(),
        values: Mutex::new(values),
        bound: buggy.then(|| Duration::from_millis(bound_ms)),
        reported: AtomicBool::new(false),
    });
    let flusher = store.clone();
    std::thread::spawn(move || {
        if let Err(error) = flusher.flush(syncer) {
            eprintln!("slowdisk-server: flush: {error}");
            std::process::exit(1);
        }
    });
    let listener = TcpListener::bind(("0.0.0.0", PORT))?;
    seedbox_sdk::setup_complete(json!({"buggy": buggy}));
    for stream in listener.incoming() {
        let store = store.clone();
        let stream = stream?;
        std::thread::spawn(move || {
            if let Err(error) = serve(store, stream) {
                eprintln!("slowdisk-server: connection: {error}");
            }
        });
    }
    Ok(())
}
