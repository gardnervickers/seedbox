//! A replica. The leader (REPLOG_LEADER=1) takes `WRITE n`, applies it, and
//! acknowledges once one follower also has it, waiting at most 300ms for each
//! follower; a follower that misses a write catches up on the next one it
//! receives. Followers take `SET n` from the leader. `READ` returns the local
//! value with REPLOG_BUG=1 (the default); otherwise a follower asks the leader.
use seedbox_eval_replog::{Connection, PORT};
use serde_json::json;
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

struct Replica {
    value: AtomicU64,
    leader: bool,
    buggy: bool,
    leader_address: String,
    followers: Vec<Mutex<Option<Connection>>>,
    follower_addresses: Vec<String>,
}

impl Replica {
    /// Send the value to one follower once, reconnecting first if the last
    /// attempt failed. A follower that misses it waits for the next write.
    fn replicate(&self, index: usize, value: u64) -> bool {
        let mut slot = self.followers[index].lock().unwrap();
        if slot.is_none() {
            *slot =
                Connection::open(&self.follower_addresses[index], Duration::from_millis(300)).ok();
        }
        let acked = slot
            .as_mut()
            .is_some_and(|c| c.request(&format!("SET {value}")).is_ok_and(|r| r == "OK"));
        if !acked {
            *slot = None;
        }
        acked
    }

    /// Apply locally, replicate to every follower in parallel, each bounded
    /// by its timeouts, and acknowledge if one follower has it: leader plus one
    /// follower is a majority of three. Only an unreachable follower misses a
    /// write; it catches up on the next one it receives.
    fn write(self: &Arc<Self>, value: u64) -> bool {
        self.value.fetch_max(value, Ordering::SeqCst);
        let (sender, receiver) = std::sync::mpsc::channel();
        for index in 0..self.followers.len() {
            let (replica, sender) = (self.clone(), sender.clone());
            std::thread::spawn(move || {
                let _ = sender.send(replica.replicate(index, value));
            });
        }
        drop(sender);
        receiver.iter().filter(|acked| *acked).count() >= 1
    }

    fn read(&self) -> Option<u64> {
        if self.leader || self.buggy {
            return Some(self.value.load(Ordering::SeqCst));
        }
        let mut leader = Connection::open(&self.leader_address, Duration::from_millis(300)).ok()?;
        leader.request("READ").ok()?.parse().ok()
    }
}

fn serve(replica: Arc<Replica>, stream: TcpStream) -> std::io::Result<()> {
    let mut writer = stream.try_clone()?;
    for line in BufReader::new(stream).lines() {
        let line = line?;
        let reply = match line.split_whitespace().collect::<Vec<_>>().as_slice() {
            ["WRITE", n] if replica.leader => match n.parse() {
                Ok(n) if replica.write(n) => "OK".to_string(),
                _ => "NO".to_string(),
            },
            ["SET", n] if !replica.leader => match n.parse::<u64>() {
                Ok(n) => {
                    replica.value.fetch_max(n, Ordering::SeqCst);
                    "OK".to_string()
                }
                Err(_) => "ERR".to_string(),
            },
            ["READ"] => replica
                .read()
                .map_or_else(|| "NO".to_string(), |value| value.to_string()),
            _ => "ERR".to_string(),
        };
        writer.write_all(reply.as_bytes())?;
        writer.write_all(b"\n")?;
    }
    Ok(())
}

fn main() -> std::io::Result<()> {
    let leader = std::env::var("REPLOG_LEADER").is_ok_and(|v| v == "1");
    let buggy = std::env::var("REPLOG_BUG").map_or(true, |v| v != "0");
    let follower_addresses: Vec<String> = std::env::var("REPLOG_FOLLOWERS")
        .unwrap_or_default()
        .split(',')
        .filter(|s| !s.is_empty())
        .map(|host| format!("{host}:{PORT}"))
        .collect();
    let replica = Arc::new(Replica {
        value: AtomicU64::new(0),
        leader,
        buggy,
        leader_address: format!(
            "{}:{PORT}",
            std::env::var("REPLOG_LEADER_HOST").unwrap_or_else(|_| "r1".into())
        ),
        followers: follower_addresses
            .iter()
            .map(|_| Mutex::new(None))
            .collect(),
        follower_addresses,
    });
    let listener = TcpListener::bind(("0.0.0.0", PORT))?;
    seedbox_sdk::setup_complete(json!({"leader": leader, "buggy": buggy}));
    for stream in listener.incoming() {
        let replica = replica.clone();
        let stream = stream?;
        std::thread::spawn(move || {
            let _ = serve(replica, stream);
        });
    }
    Ok(())
}
