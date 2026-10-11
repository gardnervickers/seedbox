//! Retry and deadline contracts against disposable TCP peers.
use serde_json::{json, Value};
use std::{
    io::{BufRead, BufReader, Write},
    net::TcpListener,
    path::Path,
    process::Command,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread,
    time::{Duration, Instant},
};

const CLIENT: &str = r#"
import json,sys,time
from main import Client
import main
main.LOAD_TIMEOUT=float(sys.argv[2])
client=Client(sys.argv[1])
results=[]
for action in json.loads(sys.argv[3]):
    try:
        if action=='load': value=json.loads(client.reply('LOAD 1'))
        else: value=client.put(expected=1,deadline=time.monotonic()+1)
        results.append({'value':value,'sequence':client.sequence,'acks':list(client.acks)})
    except Exception as error:
        results.append({'error':type(error).__name__,'sequence':client.sequence,'acks':list(client.acks)})
print(json.dumps(results))
"#;
fn exercise(mode: &str, actions: Value, timeout: f64) -> (Value, Vec<String>, Duration) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    listener.set_nonblocking(true).unwrap();
    let done = Arc::new(AtomicBool::new(false));
    let stop = done.clone();
    let mode = mode.to_owned();
    let worker = thread::spawn(move || {
        let mut requests = vec![];
        while !stop.load(Ordering::Relaxed) {
            let (mut stream, _) = match listener.accept() {
                Ok(pair) => pair,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(2));
                    continue;
                }
                Err(error) => panic!("accept: {error}"),
            };
            stream.set_nonblocking(false).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(1)))
                .unwrap();
            let mut request = String::new();
            BufReader::new(stream.try_clone().unwrap())
                .read_line(&mut request)
                .unwrap();
            requests.push(request.trim().to_owned());
            if mode == "unanswered" {
                thread::sleep(Duration::from_millis(250));
                continue;
            }
            if mode == "trickle" {
                for _ in 0..30 {
                    if stream.write_all(b"O").is_err() {
                        break;
                    }
                    thread::sleep(Duration::from_millis(10));
                }
                continue;
            }
            let response = if requests.len() == 1 {
                mode.as_str()
            } else {
                "OK 1"
            };
            stream
                .write_all(format!("{response}\n").as_bytes())
                .unwrap();
        }
        requests
    });
    let start = Instant::now();
    let output = Command::new("python3")
        .args(["-c", CLIENT])
        .arg(address.to_string())
        .arg(timeout.to_string())
        .arg(actions.to_string())
        .env(
            "PYTHONPATH",
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/faults/workload"),
        )
        .output()
        .unwrap();
    let elapsed = start.elapsed();
    done.store(true, Ordering::Relaxed);
    let requests = worker.join().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = String::from_utf8(output.stdout).unwrap();
    let values = serde_json::from_str(text.lines().last().unwrap()).unwrap();
    (values, requests, elapsed)
}
#[test]
fn malformed_acknowledgements_do_not_advance_the_ledger_and_retry_keeps_the_pending_key() {
    for invalid in ["NO 1", "OK", "OK one", "OK 2"] {
        let (values, requests, _) = exercise(invalid, json!(["load", "put"]), 1.0);
        assert_eq!(values[0]["error"], "ValueError");
        assert_eq!(values[0]["sequence"], 0);
        assert_eq!(values[0]["acks"], json!([]));
        assert_eq!(values[1]["sequence"], 1);
        assert_eq!(values[1]["acks"], json!([["k/000000", "v000000", 1]]));
        assert_eq!(requests, vec!["PUT k/000000 v000000"; 2]);
    }
}
#[test]
fn unanswered_and_trickling_peers_cannot_extend_the_total_operation_deadline() {
    for mode in ["unanswered", "trickle"] {
        let (values, requests, elapsed) = exercise(mode, json!(["load"]), 0.12);
        assert_eq!(values[0]["error"], "TimeoutError");
        assert_eq!(values[0]["sequence"], 0);
        assert_eq!(values[0]["acks"], json!([]));
        assert_eq!(requests, ["PUT k/000000 v000000"]);
        assert!(elapsed < Duration::from_secs(1), "{mode}: {elapsed:?}");
    }
}
#[test]
fn accepted_replies_cannot_extend_load_past_its_fixed_deadline() {
    // A Rust-owned clock/transport script makes the two boundary cases exact.
    // The adapter only supplies those observations to the real client method.
    let script = r#"
import json,sys
import main
observations=iter(json.loads(sys.argv[1]))
dispatched=[]
def monotonic(): return next(observations)
def exchange(address,command,**arguments):
    dispatched.append(command)
    return 'OK 1'
client=main.Client('127.0.0.1:1')
main.time.monotonic=monotonic
main.exchange=exchange
try:
    client.reply('LOAD 1')
    error=None
except Exception as failure: error=type(failure).__name__
print(json.dumps({'error':error,'dispatched':dispatched,'sequence':client.sequence,'acks':list(client.acks)}))
"#;
    for (times, count) in [
        (vec![0., 0., 0., 5., 5., 5., 5., 10., 10.], 2),
        (vec![0., 0., 0., 0., 10.], 1),
    ] {
        let output = Command::new("python3")
            .args(["-c", script])
            .arg(json!(times).to_string())
            .env(
                "PYTHONPATH",
                Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/faults/workload"),
            )
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let value: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(value["error"], "TimeoutError");
        assert_eq!(value["sequence"], 1);
        assert_eq!(value["acks"], json!([["k/000000", "v000000", 1]]));
        assert_eq!(value["dispatched"].as_array().unwrap().len(), count);
    }
}
