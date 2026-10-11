//! The pinned guest SDK against a private Rust HTTP peer, without MinIO or a VM.
//! Supply SEEDBOX_MINIO_WHEELS, then select these optional integration tests with --ignored.
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs,
    io::{BufRead, BufReader, Read, Write},
    net::TcpListener,
    path::{Path, PathBuf},
    process::Command,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};
const A: &[u8] = b"acknowledged-value-A";
fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap()
}
fn sdk(directory: &Path) {
    let wheels = PathBuf::from(
        std::env::var_os("SEEDBOX_MINIO_WHEELS")
            .expect("set SEEDBOX_MINIO_WHEELS to the pinned example wheels"),
    );
    let pins: Value =
        serde_json::from_slice(&fs::read(root().join("examples/minio/inputs.lock.json")).unwrap())
            .unwrap();
    for pin in pins["wheels"].as_array().unwrap() {
        let path = wheels.join(pin["filename"].as_str().unwrap());
        let bytes = fs::read(&path).unwrap();
        assert_eq!(bytes.len() as u64, pin["size"].as_u64().unwrap());
        assert_eq!(
            hex::encode(Sha256::digest(&bytes)),
            pin["sha256"].as_str().unwrap()
        );
        let output = Command::new("python3")
            .args(["-m", "zipfile", "-e"])
            .arg(path)
            .arg(directory)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
#[derive(Debug)]
struct Request {
    method: String,
    path: String,
    headers: BTreeMap<String, String>,
}
#[derive(Default)]
struct Store {
    objects: BTreeMap<String, (Vec<u8>, String)>,
    requests: Vec<Request>,
    lists: usize,
}
struct Peer {
    endpoint: String,
    store: Arc<Mutex<Store>>,
    done: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}
impl Peer {
    fn new(options: Value) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        listener.set_nonblocking(true).unwrap();
        let store = Arc::new(Mutex::new(Store::default()));
        let done = Arc::new(AtomicBool::new(false));
        let state = store.clone();
        let stop = done.clone();
        let worker = thread::spawn(move || {
            let mut handlers = vec![];
            while !stop.load(Ordering::Relaxed) {
                let (channel, _) = match listener.accept() {
                    Ok(pair) => pair,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(2));
                        continue;
                    }
                    Err(error) => panic!("accept: {error}"),
                };
                let state = state.clone();
                let options = options.clone();
                handlers.push(thread::spawn(move || serve(channel, state, options)));
            }
            for handler in handlers {
                handler.join().unwrap();
            }
        });
        Self {
            endpoint,
            store,
            done,
            thread: Some(worker),
        }
    }
}
fn serve(mut channel: std::net::TcpStream, state: Arc<Mutex<Store>>, options: Value) {
    channel.set_nonblocking(false).unwrap();
    channel
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    channel
        .set_write_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    let mut reader = BufReader::new(channel.try_clone().unwrap());
    let mut first = String::new();
    reader.read_line(&mut first).unwrap();
    let mut words = first.split_whitespace();
    let method = words.next().unwrap().to_owned();
    let path = words.next().unwrap().to_owned();
    let mut headers = BTreeMap::new();
    loop {
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        if line == "\r\n" {
            break;
        }
        let (key, value) = line.split_once(':').unwrap();
        headers.insert(key.to_ascii_lowercase(), value.trim().to_owned());
    }
    if headers
        .get("expect")
        .is_some_and(|value| value.eq_ignore_ascii_case("100-continue"))
    {
        channel.write_all(b"HTTP/1.1 100 Continue\r\n\r\n").unwrap();
    }
    let mut body = vec![
        0;
        headers
            .get("content-length")
            .map(|length| length.parse::<usize>().unwrap())
            .unwrap_or(0)
    ];
    reader.read_exact(&mut body).unwrap();
    let mut store = state.lock().unwrap();
    store.requests.push(Request {
        method: method.clone(),
        path: path.clone(),
        headers: headers.clone(),
    });
    let mut status = 200;
    let mut response = vec![];
    let mut etag = None;
    if method == "GET" && path == "/" {
        store.lists += 1;
        let ready_after = options["ready_after"].as_u64().unwrap_or(1);
        if (store.lists as u64) < ready_after || options.get("readiness_status").is_some() {
            status = options["readiness_status"].as_u64().unwrap_or(503) as u16;
            if (store.lists as u64) >= ready_after && options.get("ready_after").is_some() {
                status = 200;
            }
        }
        response = b"<ListAllMyBucketsResult><Buckets/></ListAllMyBucketsResult>".to_vec();
    } else if method == "PUT" && path == "/seedbox-cas" {
    } else if method == "GET" {
        if let Some((value, tag)) = store.objects.get(&path) {
            response = value.clone();
            etag = Some(tag.clone());
            if options["wrong_winner"].as_bool() == Some(true) && path.ends_with("/concurrent") {
                response = b"unacknowledged".to_vec();
            }
        } else {
            status = 404;
        }
    } else if method == "PUT" {
        if let Some(forced) = options["put_status"].as_u64() {
            status = forced as u16;
        } else if options["ignore_conditions"].as_bool() != Some(true) {
            if headers.get("if-none-match").map(String::as_str) == Some("*")
                && store.objects.contains_key(&path)
            {
                status = 412;
            } else if let Some(expected) = headers.get("if-match") {
                status = match store.objects.get(&path) {
                    None => options["missing_match_status"].as_u64().unwrap_or(404) as u16,
                    Some((_, tag)) if tag != expected => 412,
                    _ => 200,
                };
            }
        }
        if status == 200
            || (status == 412 && options["overwrite_on_conflict"].as_bool() == Some(true))
        {
            let tag = format!("\"opaque-{}\"", &hex::encode(Sha256::digest(&body))[..10]);
            store.objects.insert(path.clone(), (body, tag.clone()));
            if status == 200 {
                etag = Some(tag);
            }
        }
    } else {
        panic!("unexpected HTTP request {method} {path}");
    }
    let lost = options["lose_put_reply"].as_bool() == Some(true)
        && method == "PUT"
        && path != "/seedbox-cas";
    drop(store);
    if lost {
        return;
    }
    if status >= 400 {
        let code = match status {
            404 => "NoSuchKey",
            412 => "PreconditionFailed",
            _ => "ServiceUnavailable",
        };
        response =
            format!("<Error><Code>{code}</Code><Message>fixture</Message></Error>").into_bytes();
    }
    let extra = etag
        .map(|etag| format!("ETag: {etag}\r\n"))
        .unwrap_or_default();
    let header = format!("HTTP/1.1 {status} fixture\r\nContent-Length: {}\r\nContent-Type: application/xml\r\nConnection: close\r\n{extra}\r\n", response.len());
    channel.write_all(header.as_bytes()).unwrap();
    channel.write_all(&response).unwrap();
}
impl Drop for Peer {
    fn drop(&mut self) {
        self.done.store(true, Ordering::Relaxed);
        self.thread.take().unwrap().join().unwrap();
    }
}
const DRIVER: &str = r#"
import json,sys,types
from pathlib import Path
import boto3,botocore,cas
options=json.loads(sys.argv[4])
clock={'now':0.0,'sleeps':[],'calls':0}
observed=cas.observed
def observe(call,**arguments):
    try: return observed(call,**arguments)
    finally:
        clock['now']+=options.get('request_seconds',0)
        clock['calls']+=1
def sleep(seconds):
    clock['now']+=seconds
    clock['sleeps'].append(seconds)
cas.time=types.SimpleNamespace(monotonic=lambda:clock['now'],sleep=sleep)
cas.observed=observe
constructed=0
def make():
    global constructed
    constructed+=1
    if constructed==options.get('fail_client_number'): raise RuntimeError('fixture client setup failed')
    return cas.client(sys.argv[1],'fixture-access','fixture-secret-only')
ledger=Path(sys.argv[3])
try:
    s3=make()
    action=sys.argv[2]
    if action=='ready': value=cas.ready(s3)
    elif action=='concurrent': value=cas.concurrent_creates(make)
    elif action=='headers':
        status,response=cas.put(s3,'header',cas.A,IfNoneMatch='*')
        value=[status,cas.put(s3,'header',cas.B,IfMatch=response['ETag'])[0]]
    elif action=='put': value=cas.put(s3,'slot',cas.A,IfNoneMatch='*')[0]
    elif action=='qualification':
        cas.ready(s3);cas.create_bucket(s3);value=cas.qualify(s3,make)
    else:
        cas.ready(s3);cas.create_bucket(s3);value=cas.planted(s3,make,action,ledger)
    result={'value':value}
except Exception as error: result={'error':type(error).__name__,'message':str(error)}
result.update(clock=clock,boto3=boto3.__version__,botocore=botocore.__version__,config={'connect':s3.meta.config.connect_timeout,'read':s3.meta.config.read_timeout,'attempts':s3.meta.config.retries['total_max_attempts']})
print('RUST_RESULT '+json.dumps(result,default=lambda value:value.hex()))
"#;
fn execute(
    sdk: &Path,
    peer: &Peer,
    action: &str,
    ledger: &Path,
    options: Value,
) -> (Value, String, Duration) {
    let start = Instant::now();
    let output = Command::new("python3")
        .args(["-c", DRIVER])
        .arg(&peer.endpoint)
        .arg(action)
        .arg(ledger)
        .arg(options.to_string())
        .env(
            "PYTHONPATH",
            format!(
                "{}:{}",
                sdk.display(),
                root().join("examples/minio/client").display()
            ),
        )
        .env("AWS_EC2_METADATA_DISABLED", "true")
        .env("PYTHONDONTWRITEBYTECODE", "1")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = String::from_utf8(output.stdout).unwrap();
    let result = serde_json::from_str(
        text.lines()
            .find_map(|line| line.strip_prefix("RUST_RESULT "))
            .unwrap(),
    )
    .unwrap();
    (result, text, start.elapsed())
}

#[test]
#[ignore = "requires the pinned local SDK wheels in SEEDBOX_MINIO_WHEELS"]
fn sdk_headers_conditionals_concurrent_winners_and_planted_acknowledgements() {
    let directory = tempfile::tempdir().unwrap();
    let packages = directory.path().join("sdk");
    sdk(&packages);
    for (mode, options, expected) in [
        ("qualification", json!({}), 0),
        ("fixed", json!({}), 0),
        ("broken", json!({}), 1),
        ("headers", json!({}), 0),
        ("qualification", json!({"overwrite_on_conflict":true}), 2),
        ("qualification", json!({"missing_match_status":412}), 2),
        ("qualification", json!({"ignore_conditions":true}), 2),
        ("qualification", json!({"wrong_winner":true}), 2),
        ("fixed", json!({"put_status":412}), 2),
        ("fixed", json!({"lose_put_reply":true}), 2),
        ("concurrent", json!({}), 0),
        ("concurrent", json!({"fail_client_number":2}), 2),
    ] {
        let peer = Peer::new(options.clone());
        let ledger = directory.path().join(format!("ledger-{mode}-{expected}"));
        let _ = fs::remove_file(&ledger);
        let (result, console, elapsed) = execute(&packages, &peer, mode, &ledger, options.clone());
        assert_eq!(result["boto3"], "1.40.45");
        assert_eq!(result["botocore"], "1.40.45");
        assert_eq!(result["config"], json!({"connect":1,"read":2,"attempts":1}));
        let state = peer.store.lock().unwrap();
        let writes = state
            .requests
            .iter()
            .filter(|request| request.method == "PUT" && request.path != "/seedbox-cas")
            .collect::<Vec<_>>();
        if expected == 2 {
            assert!(result.get("error").is_some(), "{result}\n{console}");
            if mode == "concurrent" {
                assert!(elapsed < Duration::from_secs(3));
            }
            if mode == "fixed" {
                assert!(!ledger.exists());
                assert_eq!(writes.len(), 1);
            }
        } else if mode == "headers" {
            assert_eq!(result["value"], json!([200, 200]));
            assert_eq!(writes[0].headers["if-none-match"], "*");
            assert_eq!(
                writes[1].headers["if-match"],
                format!("\"opaque-{}\"", &hex::encode(Sha256::digest(A))[..10])
            );
        } else if mode == "concurrent" {
            let mut statuses = result["value"]
                .as_array()
                .unwrap()
                .iter()
                .map(|row| row[0].as_u64().unwrap())
                .collect::<Vec<_>>();
            statuses.sort();
            assert_eq!(statuses, [200, 412]);
            assert_eq!(writes.len(), 2);
        } else {
            assert_eq!(result["value"], expected);
            assert!(console.contains("MINIO_RESULT"));
            if mode != "qualification" {
                assert_eq!(fs::read(&ledger).unwrap(), A);
                assert_eq!(writes.len(), if mode == "broken" { 3 } else { 2 });
                if mode == "broken" {
                    assert!(!writes.last().unwrap().headers.contains_key("if-none-match"));
                }
            }
        }
    }
}

#[test]
#[ignore = "requires the pinned local SDK wheels in SEEDBOX_MINIO_WHEELS"]
fn readiness_counts_total_request_time_bounds_sleep_and_refuses_late_or_unauthorized_success() {
    let directory = tempfile::tempdir().unwrap();
    let packages = directory.path().join("sdk");
    sdk(&packages);
    for (status, ready_after, request_seconds, calls, now, error) in [
        (503, Some(120), 0.0, 120, 29.75, None),
        (500, Some(120), 0.0, 120, 29.75, None),
        (503, None, 0.0, 120, 30.0, Some("InfrastructureError")),
        (500, None, 0.0, 120, 30.0, Some("InfrastructureError")),
        (503, None, 1.9, 14, 30.0, Some("InfrastructureError")),
        (503, Some(14), 2.0, 14, 31.25, Some("InfrastructureError")),
        (403, None, 0.0, 1, 0.0, Some("QualificationError")),
    ] {
        let mut options = json!({"readiness_status":status,"request_seconds":request_seconds});
        if let Some(count) = ready_after {
            options["ready_after"] = count.into();
        }
        let peer = Peer::new(options.clone());
        let (result, console, _) = execute(
            &packages,
            &peer,
            "ready",
            &directory.path().join("ledger"),
            options,
        );
        assert_eq!(result["clock"]["calls"], calls);
        assert!((result["clock"]["now"].as_f64().unwrap() - now).abs() < 1e-8);
        assert_eq!(
            result["error"],
            error.map(|value| json!(value)).unwrap_or(Value::Null)
        );
        assert!(!console.contains("MINIO_RESULT"));
        assert_eq!(peer.store.lock().unwrap().requests.len(), calls as usize);
        if request_seconds == 1.9 {
            assert!(
                (result["clock"]["sleeps"]
                    .as_array()
                    .unwrap()
                    .last()
                    .unwrap()
                    .as_f64()
                    .unwrap()
                    - 0.15)
                    .abs()
                    < 1e-8
            );
        }
        if status == 403 {
            assert_eq!(result["clock"]["sleeps"], json!([]));
        }
    }
    let peer = Peer::new(json!({"put_status":503}));
    let (result, _, _) = execute(
        &packages,
        &peer,
        "put",
        &directory.path().join("ledger"),
        json!({}),
    );
    assert_eq!(result["value"], 503);
    assert_eq!(peer.store.lock().unwrap().requests.len(), 1);
}
