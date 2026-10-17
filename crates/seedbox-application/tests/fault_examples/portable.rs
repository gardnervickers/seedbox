//! Rust owns the former portable.py matrix and every expectation. Python is
//! used only for the unchanged guest program and a language-call fixture.
use seedbox_test::process::Guarded;
use seedbox_test_time::HangGuard;
use serde_json::{json, Value};
use std::{
    fs,
    io::{BufRead, BufReader, Read, Write},
    net::{TcpListener, TcpStream},
    path::{Path, PathBuf},
    process::{Child, ChildStdin, Command, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc, Mutex,
    },
    thread::{self, JoinHandle},
    time::Duration,
};

const BATCH: usize = 32;
const CONTROL: Duration = Duration::from_secs(30);
fn source() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/faults/workload/main.py")
}
fn addresses() -> (String, String) {
    // Reserve both while choosing: the two roles must not get the same port.
    let server = TcpListener::bind("127.0.0.1:0").unwrap();
    let client = TcpListener::bind("127.0.0.1:0").unwrap();
    (
        server.local_addr().unwrap().to_string(),
        client.local_addr().unwrap().to_string(),
    )
}
fn wire(address: &str, command: &str) -> String {
    let guard = HangGuard::new("actual control socket response", CONTROL);
    let mut socket =
        TcpStream::connect_timeout(&address.parse().unwrap(), guard.remaining().unwrap())
            .unwrap_or_else(|e| panic!("connect {address} for {command}: {e}"));
    socket
        .set_write_timeout(Some(guard.remaining().unwrap()))
        .unwrap();
    writeln!(socket, "{command}").unwrap();
    socket
        .set_read_timeout(Some(guard.remaining().unwrap()))
        .unwrap();
    let mut bytes = Vec::new();
    BufReader::new(socket)
        .take(8193)
        .read_until(b'\n', &mut bytes)
        .unwrap();
    assert!(
        bytes.ends_with(b"\n") && bytes.len() <= 8192,
        "invalid response to {command}: {bytes:?}"
    );
    String::from_utf8(bytes).unwrap().trim().to_owned()
}
fn query(address: &str, command: &str) -> Value {
    serde_json::from_str(&wire(address, command)).unwrap()
}

struct Program {
    child: Child,
    log: PathBuf,
}
impl Program {
    fn start(mode: &str, address: &str, log: &Path, args: &[String]) -> Self {
        let output = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(log)
            .unwrap();
        let child = Command::new("python3")
            .arg("-u")
            .arg(source())
            .arg(mode)
            .args(["--listen", address])
            .args(args)
            .env("PYTHONDONTWRITEBYTECODE", "1")
            .stdout(output.try_clone().unwrap())
            .stderr(output)
            .guarded_spawn()
            .unwrap();
        let mut result = Self {
            child,
            log: log.to_owned(),
        };
        let guard = HangGuard::new("Python service must accept a real connection", CONTROL);
        loop {
            assert!(
                result.child.try_wait().unwrap().is_none(),
                "{mode} exited during startup: {}",
                fs::read_to_string(log).unwrap()
            );
            if TcpStream::connect_timeout(
                &address.parse().unwrap(),
                guard.remaining().unwrap().min(Duration::from_millis(100)),
            )
            .is_ok()
            {
                return result;
            }
            guard.remaining().unwrap_or_else(|error| {
                panic!(
                    "{mode} readiness infrastructure hang: {error}: {}",
                    fs::read_to_string(log).unwrap()
                )
            });
            thread::yield_now();
        }
    }
    fn stop(&mut self) {
        if self.child.try_wait().unwrap().is_none() {
            self.child.kill().unwrap();
        }
        HangGuard::new("owned Python service must exit after stop", CONTROL)
            .wait_for(|| self.child.try_wait().unwrap())
            .unwrap_or_else(|error| {
                panic!(
                    "child exit infrastructure hang: {error}: {}",
                    self.log.display()
                )
            });
    }
}
impl Drop for Program {
    fn drop(&mut self) {
        if thread::panicking() {
            eprintln!(
                "{}:\n{}",
                self.log.display(),
                fs::read_to_string(&self.log).unwrap_or_default()
            );
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

pub(super) fn scenarios() {
    for (broken, restart) in [(false, true), (true, true), (true, false)] {
        scenario(broken, restart);
    }
}
fn scenario(broken: bool, restart: bool) {
    let root = tempfile::tempdir().unwrap();
    println!("case broken={broken} restart={restart}");
    let (server_address, client_address) = addresses();
    let server_log = root.path().join("server.log");
    let client_log = root.path().join("client.log");
    let mut server_args = vec![
        "--state".into(),
        root.path().join("kv.log").display().to_string(),
    ];
    if broken {
        server_args.push("--broken".into());
    }
    let mut server = Program::start("server", &server_address, &server_log, &server_args);
    let mut client = Program::start(
        "client",
        &client_address,
        &client_log,
        &[
            "--address".into(),
            server_address.clone(),
            "--batch".into(),
            BATCH.to_string(),
        ],
    );
    assert_eq!(query(&client_address, "STATUS")["acked"], 0);
    assert_eq!(
        query(&client_address, "LOAD 1"),
        json!({"acked":BATCH,"generation":1,"total":BATCH})
    );
    assert_eq!(wire(&server_address, "GET k/000000"), "VALUE 1 v000000");
    if restart {
        server.stop();
        assert_eq!(
            query(&client_address, "PROBE"),
            json!({"attempts":4,"failed":4,"acked":0})
        );
        server = Program::start("server", &server_address, &server_log, &server_args);
        assert_eq!(
            wire(&server_address, "GET k/000000"),
            if broken {
                "MISSING 2"
            } else {
                "VALUE 2 v000000"
            }
        );
        assert_eq!(query(&client_address, "LOAD 2")["acked"], BATCH);
        assert_eq!(query(&client_address, "LOAD 2")["total"], 3 * BATCH);
        server.stop();
        server = Program::start("server", &server_address, &server_log, &server_args);
        assert_eq!(query(&client_address, "LOAD 3")["total"], 4 * BATCH);
    } else {
        for total in [2 * BATCH, 3 * BATCH, 4 * BATCH] {
            assert_eq!(query(&client_address, "LOAD 1")["total"], total);
        }
    }
    let result = query(&client_address, "VERIFY");
    let missing = if broken && restart { 3 * BATCH } else { 0 };
    assert_eq!(
        result,
        json!({"status":if missing>0 {"fail"} else {"pass"},"acked":4*BATCH,"pre_restart":if restart {BATCH} else {4*BATCH},"post_restart":if restart {BATCH} else {4*BATCH},"generation":if restart {3} else {1},"missing":missing})
    );
    assert_eq!(query(&client_address, "STATUS")["acked"], 4 * BATCH);
    assert_eq!(query(&client_address, "VERIFY"), result);
    client.stop();
    server.stop();
    let client_output = fs::read_to_string(&client_log).unwrap();
    let server_output = fs::read_to_string(&server_log).unwrap();
    assert_eq!(
        client_output.matches("RESULT ").count(),
        1,
        "{client_output}"
    );
    assert_eq!(
        client_output.matches("MISSING_ACK ").count(),
        missing,
        "{client_output}"
    );
    if restart && !broken {
        assert!(
            server_output.contains(&format!("generation=2 recovered={BATCH}")),
            "{server_output}"
        );
        assert!(
            server_output.contains(&format!("generation=3 recovered={}", 3 * BATCH)),
            "{server_output}"
        );
    }
    println!(
        "broken={broken} restart={restart} {} acknowledged={} missing={missing}",
        result["status"].as_str().unwrap(),
        4 * BATCH
    );
}

struct Adapter {
    child: Child,
    input: ChildStdin,
    output: mpsc::Receiver<Result<Value, String>>,
    reader: Option<JoinHandle<()>>,
    errors: Option<JoinHandle<Vec<u8>>>,
}
impl Adapter {
    fn new() -> Self {
        let mut child = Command::new("python3")
            .arg("-u")
            .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/support/fault_workload.py"))
            .env("PYTHONDONTWRITEBYTECODE", "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .guarded_spawn()
            .unwrap();
        let input = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let mut stderr = child.stderr.take().unwrap();
        let (tx, output) = mpsc::channel();
        let reader = thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let value = line
                    .map_err(|e| e.to_string())
                    .and_then(|s| serde_json::from_str(&s).map_err(|e| format!("{e}: {s}")));
                if tx.send(value).is_err() {
                    break;
                }
            }
        });
        let errors = thread::spawn(move || {
            let mut b = Vec::new();
            stderr.read_to_end(&mut b).unwrap();
            b
        });
        Self {
            child,
            input,
            output,
            reader: Some(reader),
            errors: Some(errors),
        }
    }
    fn call(&mut self, request: Value) -> Value {
        self.observe(request, |_| {})
    }
    fn observe(&mut self, request: Value, mut dispatch: impl FnMut(Value)) -> Value {
        writeln!(self.input, "{}", request).unwrap();
        self.input.flush().unwrap();
        let guard = HangGuard::new(
            "Python fixture must produce an actual response",
            Duration::from_secs(15),
        );
        loop {
            let value = guard
                .recv(&self.output)
                .unwrap_or_else(|e| {
                    panic!("Python fixture stalled/disconnected for {request}: {e}")
                })
                .unwrap();
            if value.get("event").is_some() {
                dispatch(value);
            } else {
                if let Some(console) = value["stdout"].as_str() {
                    print!("{console}");
                }
                return value;
            }
        }
    }
}
impl Drop for Adapter {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
        if let Some(errors) = self.errors.take() {
            if let Ok(bytes) = errors.join() {
                if !bytes.is_empty() {
                    eprintln!("Python fixture stderr: {}", String::from_utf8_lossy(&bytes));
                }
            }
        }
    }
}
fn success(value: Value) -> Value {
    assert!(value.get("error").is_none(), "{value}");
    value
}
fn error(value: &Value, kind: &str) {
    assert_eq!(value["error"]["type"], kind, "{value}");
}

struct Peer {
    address: String,
    requests: Arc<Mutex<Vec<String>>>,
    done: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}
impl Peer {
    fn new(
        mut reply: impl FnMut(&str, &mut TcpStream, usize) -> Option<String> + Send + 'static,
    ) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap().to_string();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let seen = requests.clone();
        let done = Arc::new(AtomicBool::new(false));
        let stop = done.clone();
        let worker = thread::spawn(move || {
            while !stop.load(Ordering::SeqCst) {
                let (mut channel, _) = listener.accept().expect("TCP responder accept");
                if stop.load(Ordering::SeqCst) {
                    break;
                }
                let guard = HangGuard::new("actual TCP peer request and reply", CONTROL);
                channel
                    .set_read_timeout(Some(guard.remaining().unwrap()))
                    .unwrap();
                channel
                    .set_write_timeout(Some(guard.remaining().unwrap()))
                    .unwrap();
                let mut line = String::new();
                BufReader::new(channel.try_clone().unwrap())
                    .take(8193)
                    .read_line(&mut line)
                    .unwrap();
                let line = line.trim().to_owned();
                let index = {
                    let mut q = seen.lock().unwrap();
                    q.push(line.clone());
                    q.len()
                };
                if let Some(response) = reply(&line, &mut channel, index) {
                    writeln!(channel, "{response}").unwrap();
                }
            }
        });
        Self {
            address,
            requests,
            done,
            worker: Some(worker),
        }
    }
    fn requests(&self) -> Vec<String> {
        self.requests.lock().unwrap().clone()
    }
    fn finish(mut self) {
        self.join();
    }
    fn join(&mut self) {
        self.done.store(true, Ordering::SeqCst);
        if let Some(w) = self.worker.take() {
            let guard = HangGuard::new("TCP responder must acknowledge explicit shutdown", CONTROL);
            // Publish shutdown before waking blocking accept. A failed wake is
            // harmless if the thread already exited; completion is checked below.
            let _wake = TcpStream::connect_timeout(
                &self.address.parse().unwrap(),
                guard.remaining().unwrap(),
            );
            if let Err(error) = guard.wait_for(|| w.is_finished().then_some(())) {
                if thread::panicking() {
                    eprintln!("test responder shutdown infrastructure hang: {error}");
                    return;
                }
                panic!("test responder shutdown infrastructure hang: {error}");
            }
            if let Err(e) = w.join() {
                if thread::panicking() {
                    eprintln!("TCP responder failed: {e:?}");
                } else {
                    std::panic::resume_unwind(e);
                }
            }
        }
    }
}
impl Drop for Peer {
    fn drop(&mut self) {
        self.join();
    }
}

pub(super) fn retry_controls() {
    lost_reply();
    invalid_replies();
    successful_progress_deadlines();
    io_deadlines();
    println!("pending PUT, invalid reply, successful-progress and I/O deadline controls passed");
}
fn lost_reply() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("lost-reply.log");
    let mut store = Adapter::new();
    success(store.call(json!({"op":"store_open","path":path})));
    let store = Arc::new(Mutex::new(store));
    let peer_store = store.clone();
    let (tx, rx) = mpsc::channel::<Value>();
    let peer = Peer::new(move |request, _, index| {
        let event = HangGuard::new("Rust observer must supply the actual pending PUT", CONTROL)
            .recv(&rx)
            .unwrap();
        assert_eq!(event["command"], request);
        if index <= 2 {
            assert_eq!(event["state"]["sequence"], 0);
            assert_eq!(event["state"]["acks"], json!([]));
        }
        let result = success(
            peer_store
                .lock()
                .unwrap()
                .call(json!({"op":"store_reply","command":request})),
        );
        if index == 1 {
            None
        } else {
            Some(result["value"].as_str().unwrap().to_owned())
        }
    });
    let mut client = Adapter::new();
    success(client.call(json!({"op":"client_open","address":peer.address})));
    let result = success(
        client.observe(json!({"op":"client_reply","command":"LOAD 1"}), |event| {
            tx.send(event).unwrap()
        }),
    );
    assert_eq!(
        serde_json::from_str::<Value>(result["value"].as_str().unwrap()).unwrap(),
        json!({"generation":1,"acked":32,"total":32})
    );
    let requests = peer.requests();
    assert_eq!(requests.len(), 33);
    assert_eq!(requests[0], requests[1]);
    assert_eq!(result["state"]["sequence"], 32);
    let acks = result["state"]["acks"].as_array().unwrap();
    assert_eq!(acks.len(), 32);
    assert_eq!(acks.iter().filter(|a| a[0] == "k/000000").count(), 1);
    peer.finish();
    let mut store = store.lock().unwrap();
    let before = success(store.call(json!({"op":"store_close"})));
    assert_eq!(before["values"].as_object().unwrap().len(), 32);
    let reopened = success(store.call(json!({"op":"store_open","path":path})));
    assert_eq!(reopened["values"].as_object().unwrap().len(), 32);
    assert_eq!(reopened["values"]["k/000000"], "v000000");
}
fn invalid_replies() {
    for invalid in ["NO 1", "OK", "OK one", "OK 2"] {
        let peer =
            Peer::new(move |_, _, index| Some(if index == 1 { invalid } else { "OK 1" }.into()));
        let mut client = Adapter::new();
        success(client.call(json!({"op":"client_open","address":peer.address})));
        let result = client.call(json!({"op":"client_reply","command":"LOAD 1"}));
        error(&result, "ValueError");
        assert_eq!(result["state"]["sequence"], 0);
        assert_eq!(result["state"]["acks"], json!([]));
        assert_eq!(peer.requests().len(), 1);
        let result = success(client.call(json!({"op":"client_put","expected":1,"budget":1})));
        assert_eq!(peer.requests(), vec!["PUT k/000000 v000000"; 2]);
        assert_eq!(result["state"]["sequence"], 1);
        assert_eq!(result["state"]["acks"], json!([["k/000000", "v000000", 1]]));
        peer.finish();
    }
}
fn successful_progress_deadlines() {
    let mut client = Adapter::new();
    success(client.call(json!({"op":"client_open","address":"127.0.0.1:1"})));
    let mut requests = Vec::new();
    let result=client.observe(json!({"op":"client_reply","command":"LOAD 1","exchange_response":"OK 1","advance_seconds":5}),|event|requests.push(event["command"].clone()));
    error(&result, "TimeoutError");
    assert_eq!(
        requests,
        json!(["PUT k/000000 v000000", "PUT k/000001 v000001"])
            .as_array()
            .unwrap()
            .clone()
    );
    assert_eq!(result["state"]["sequence"], 1);
    assert_eq!(result["state"]["acks"], json!([["k/000000", "v000000", 1]]));
    let mut client = Adapter::new();
    success(client.call(json!({"op":"client_open","address":"127.0.0.1:1"})));
    let mut requests = Vec::new();
    let result=client.observe(json!({"op":"client_reply","command":"LOAD 1","clock_reads":[0,0,0,0,10],"exchange_response":"OK 1"}),|event|requests.push(event));
    error(&result, "TimeoutError");
    assert_eq!(requests.len(), 1);
    assert_eq!(result["state"]["sequence"], 1);
    assert_eq!(result["state"]["acks"].as_array().unwrap().len(), 1);
}
fn io_deadlines() {
    // Exact binary fractions are caller-selected logical time. The real guest
    // bounded_exchange still performs connect/send/receive/budget checks;
    // only its clock and socket boundary are supplied by the language fixture.
    for (case, receives, expected_timeouts) in [
        (
            "silent",
            json!([{"op":"recv","advance_seconds":0.125,"error":"timeout"}]),
            json!([0.125, 0.125, 0.125]),
        ),
        (
            "trickle",
            json!([
                {"op":"recv","advance_seconds":0.0625,"bytes":"O"},
                {"op":"recv","advance_seconds":0.0625,"bytes":"O"}
            ]),
            json!([0.125, 0.125, 0.125, 0.0625]),
        ),
        (
            "complete-at-deadline",
            json!([{"op":"recv","advance_seconds":0.125,"bytes":"OK 1\n"}]),
            json!([0.125, 0.125, 0.125]),
        ),
    ] {
        let mut client = Adapter::new();
        success(client.call(json!({"op":"client_open","address":"127.0.0.1:1"})));
        let mut steps = vec![json!({"op":"connect"}), json!({"op":"sendall"})];
        steps.extend(receives.as_array().unwrap().iter().cloned());
        let mut pending = Vec::new();
        let result = client.observe(
            json!({"op":"client_reply","command":"LOAD 1","load_timeout":0.125,"io_steps":steps}),
            |event| pending.push(event),
        );
        error(&result, "TimeoutError");
        assert_eq!(pending.len(), 1, "{case}: {result}");
        assert_eq!(pending[0]["command"], "PUT k/000000 v000000");
        assert_eq!(pending[0]["state"]["sequence"], 0);
        assert_eq!(pending[0]["state"]["acks"], json!([]));
        assert_eq!(result["state"]["sequence"], 0);
        assert_eq!(result["state"]["acks"], json!([]));
        let io = result["io"].as_array().unwrap();
        let sends: Vec<_> = io.iter().filter(|event| event["op"] == "sendall").collect();
        assert_eq!(sends.len(), 1, "{case}: {result}");
        assert_eq!(sends[0]["bytes"], "PUT k/000000 v000000\n");
        let timeouts: Vec<_> = io
            .iter()
            .filter(|event| event["op"] == "settimeout")
            .map(|event| event["timeout"].clone())
            .collect();
        assert_eq!(
            timeouts,
            expected_timeouts.as_array().unwrap().clone(),
            "{case}: {result}"
        );
        assert_eq!(
            io.iter().filter(|event| event["op"] == "recv").count(),
            receives.as_array().unwrap().len()
        );
        assert_eq!(result["clock"], 0.125);
        println!("controlled I/O deadline: {case}; pending PUT retained, no ack, logical deadline exhausted");
    }
}
