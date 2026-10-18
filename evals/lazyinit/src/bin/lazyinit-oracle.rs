//! Ask the app to run its rounds once, then judge: no round may build the
//! shared value more than once. Exits 0 unless the workload could not run.
use seedbox_eval_lazyinit::PORT;
use serde_json::json;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
use std::time::Duration;

fn main() {
    let address = std::env::var("LAZYINIT_ADDRESS").unwrap_or_else(|_| format!("app:{PORT}"));
    seedbox_sdk::declare("initialized once per round", seedbox_sdk::Kind::Always);
    let stream = (0..250).find_map(|_| {
        TcpStream::connect(&address)
            .map_err(|_| std::thread::sleep(Duration::from_millis(20)))
            .ok()
    });
    let Some(stream) = stream else {
        eprintln!("lazyinit-oracle: app never became reachable");
        std::process::exit(1);
    };
    seedbox_sdk::setup_complete(serde_json::Value::Null);
    // Leave time for faults that start just after setup.
    std::thread::sleep(Duration::from_millis(50));
    let mut writer = stream.try_clone().expect("stream");
    writer.write_all(b"RUN\n").expect("request");
    let mut reply = String::new();
    BufReader::new(stream).read_line(&mut reply).expect("reply");
    let fields: Vec<&str> = reply.split_whitespace().collect();
    let (Some(rounds), Some(doubled)) = (
        fields.get(1).and_then(|v| v.parse::<u64>().ok()),
        fields.get(3).and_then(|v| v.parse::<u64>().ok()),
    ) else {
        eprintln!("lazyinit-oracle: unexpected reply {reply:?}");
        std::process::exit(1);
    };
    seedbox_sdk::always(
        "initialized once per round",
        doubled == 0,
        json!({"rounds": rounds, "doubled": doubled}),
    );
    seedbox_sdk::flush().expect("flush properties");
    println!("rounds {rounds}, doubled {doubled}");
}
