//! Write increasing values through the leader, then read from a random
//! replica after each acknowledged write. A read older than the latest
//! acknowledged write is a stale read: the register is not linearizable.
use seedbox_eval_replog::{Connection, PORT};
use serde_json::json;
use std::io::Read;
use std::time::Duration;

const OPERATIONS: u64 = 200;

fn random_bytes(count: usize) -> Vec<u8> {
    let mut bytes = vec![0; count];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut bytes))
        .expect("urandom");
    bytes
}

fn connect(host: &str) -> Option<Connection> {
    (0..50).find_map(|_| {
        Connection::open(&format!("{host}:{PORT}"), Duration::from_millis(500))
            .map_err(|_| std::thread::sleep(Duration::from_millis(20)))
            .ok()
    })
}

fn main() {
    let replicas = ["r1", "r2", "r3"];
    seedbox_sdk::declare("reads see acknowledged writes", seedbox_sdk::Kind::Always);
    seedbox_sdk::declare("a follower answered a read", seedbox_sdk::Kind::Sometimes);
    for host in replicas {
        if connect(host).is_none() {
            eprintln!("replog-oracle: {host} never became reachable");
            std::process::exit(1);
        }
    }
    seedbox_sdk::setup_complete(serde_json::Value::Null);
    let choices = random_bytes(OPERATIONS as usize);
    let mut acknowledged = 0u64;
    let (mut stale, mut follower_reads, mut failed) = (0u64, 0u64, 0u64);
    let mut leader = None;
    for (n, choice) in (1..=OPERATIONS).zip(choices) {
        if leader.is_none() {
            leader = connect("r1");
        }
        match leader.as_mut().map(|c| c.request(&format!("WRITE {n}"))) {
            Some(Ok(reply)) if reply == "OK" => acknowledged = n,
            Some(Ok(_)) => failed += 1,
            _ => {
                failed += 1;
                leader = None;
            }
        }
        // Most reads go to the leader; one in four goes to a follower.
        let host = match choice % 8 {
            0 => "r2",
            1 => "r3",
            _ => "r1",
        };
        let Some(mut replica) = connect(host) else {
            continue;
        };
        if let Ok(Ok(value)) = replica.request("READ").map(|r| r.parse::<u64>()) {
            follower_reads += u64::from(host != "r1");
            if value < acknowledged {
                stale += 1;
                seedbox_sdk::always(
                    "reads see acknowledged writes",
                    false,
                    json!({"replica": host, "read": value, "acknowledged": acknowledged}),
                );
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    seedbox_sdk::always(
        "reads see acknowledged writes",
        stale == 0,
        json!({"stale": stale}),
    );
    seedbox_sdk::sometimes(
        "a follower answered a read",
        follower_reads > 0,
        json!({"reads": follower_reads}),
    );
    seedbox_sdk::flush().expect("flush properties");
    println!("acknowledged {acknowledged}, failed {failed}, stale {stale}, follower reads {follower_reads}");
}
