//! Real guest programs against private processes and sockets; no VM needed.
#[path = "support/process.rs"]
mod process;
use serde_json::{json, Value};
use std::{
    fs,
    io::{BufRead, BufReader, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    os::unix::{fs::PermissionsExt, net::UnixListener},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc, Mutex,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};
fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap()
}
fn python() -> Command {
    let mut command = Command::new("python3");
    command
        .env(
            "PYTHONPATH",
            format!(
                "{}:{}",
                root().join("python").display(),
                root().join("examples/faults/workload").display()
            ),
        )
        .env("PYTHONDONTWRITEBYTECODE", "1");
    command
}
struct Process {
    child: Child,
    log: Option<JoinHandle<String>>,
    address: SocketAddr,
}
impl Process {
    fn start(arguments: &[String]) -> Self {
        let mut child = python()
            .arg("-u")
            .args([
                "-c",
                r#"
import main
class Listener(main.Listener):
    def __init__(self, *args, **kwargs):
        super().__init__(*args, **kwargs)
        print('LISTENING %s:%s' % self.server_address, flush=True)
main.Listener = Listener
raise SystemExit(main.main())
"#,
            ])
            .args(arguments)
            .args(["--listen", "127.0.0.1:0"])
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let output = child.stdout.take().unwrap();
        let (send, receive) = mpsc::channel();
        let log = thread::spawn(move || {
            let mut text = String::new();
            for line in BufReader::new(output).lines() {
                let line = line.unwrap();
                if let Some(address) = line.strip_prefix("LISTENING ") {
                    send.send(address.parse::<SocketAddr>().unwrap()).unwrap();
                }
                text += &line;
                text.push('\n');
            }
            text
        });
        let address = match receive.recv_timeout(Duration::from_secs(5)) {
            Ok(address) => address,
            Err(error) => {
                child.kill().unwrap();
                child.wait().unwrap();
                panic!(
                    "guest program failed to listen: {error}: {}",
                    log.join().unwrap()
                );
            }
        };
        Self {
            child,
            log: Some(log),
            address,
        }
    }
    fn stop(&mut self) -> String {
        if self.child.try_wait().unwrap().is_none() {
            self.child.kill().unwrap();
        }
        self.child.wait().unwrap();
        self.log
            .take()
            .map(|log| log.join().unwrap())
            .unwrap_or_default()
    }
}
impl Drop for Process {
    fn drop(&mut self) {
        self.stop();
    }
}

/// A test-owned address stays reserved during backend restarts. This avoids the
/// bind-close-rebind race of choosing a supposedly free fixed server port.
struct Proxy {
    address: SocketAddr,
    backend: Arc<Mutex<Option<SocketAddr>>>,
    done: Arc<AtomicBool>,
    thread: Option<JoinHandle<Vec<String>>>,
    lose_first: Arc<AtomicBool>,
}
impl Proxy {
    fn new() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        listener.set_nonblocking(true).unwrap();
        let backend = Arc::new(Mutex::new(None));
        let done = Arc::new(AtomicBool::new(false));
        let lose_first = Arc::new(AtomicBool::new(false));
        let target = backend.clone();
        let stop = done.clone();
        let lose = lose_first.clone();
        let worker = thread::spawn(move || {
            let mut requests = vec![];
            while !stop.load(Ordering::Relaxed) {
                let (mut stream, _) = match listener.accept() {
                    Ok(pair) => pair,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(2));
                        continue;
                    }
                    Err(error) => panic!("proxy accept: {error}"),
                };
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let Some(address) = *target.lock().unwrap() else {
                    continue;
                };
                let mut request = String::new();
                if BufReader::new(stream.try_clone().unwrap())
                    .read_line(&mut request)
                    .is_err()
                {
                    continue;
                }
                requests.push(request.trim().to_owned());
                let Ok(mut destination) =
                    TcpStream::connect_timeout(&address, Duration::from_millis(100))
                else {
                    continue;
                };
                destination
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                if destination.write_all(request.as_bytes()).is_err() {
                    continue;
                }
                let mut response = String::new();
                if BufReader::new(destination)
                    .read_line(&mut response)
                    .is_err()
                {
                    continue;
                }
                if !lose.swap(false, Ordering::Relaxed) {
                    let _ = stream.write_all(response.as_bytes());
                }
            }
            requests
        });
        Self {
            address,
            backend,
            done,
            thread: Some(worker),
            lose_first,
        }
    }
    fn point(&self, process: Option<&Process>) {
        *self.backend.lock().unwrap() = process.map(|p| p.address);
    }
    fn stop(&mut self) -> Vec<String> {
        self.done.store(true, Ordering::Relaxed);
        self.thread
            .take()
            .map(|worker| worker.join().unwrap())
            .unwrap_or_default()
    }
}
impl Drop for Proxy {
    fn drop(&mut self) {
        self.stop();
    }
}
fn wire(address: SocketAddr, request: &str) -> String {
    let mut stream = TcpStream::connect_timeout(&address, Duration::from_secs(2)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(15)))
        .unwrap();
    stream.write_all(format!("{request}\n").as_bytes()).unwrap();
    let mut response = String::new();
    BufReader::new(stream).read_line(&mut response).unwrap();
    response.trim().to_owned()
}
fn json_wire(address: SocketAddr, request: &str) -> Value {
    serde_json::from_str(&wire(address, request)).unwrap()
}
fn server(root: &Path, broken: bool) -> Process {
    let mut arguments = vec![
        "server".into(),
        "--state".into(),
        root.join("kv.log").to_str().unwrap().into(),
    ];
    if broken {
        arguments.push("--broken".into());
    }
    Process::start(&arguments)
}

struct OwnedThread<T>(Option<thread::JoinHandle<T>>);
impl<T> OwnedThread<T> {
    fn join(mut self) -> T {
        self.0.take().unwrap().join().unwrap()
    }
}
impl<T> Drop for OwnedThread<T> {
    fn drop(&mut self) {
        if let Some(task) = self.0.take() {
            let result = task.join();
            if !thread::panicking() {
                result.unwrap();
            }
        }
    }
}

#[test]
fn acknowledgement_checker_distinguishes_durable_restarts_from_lost_memory_and_no_fault_control() {
    for (broken, restart) in [(false, true), (true, true), (true, false)] {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        let mut proxy = Proxy::new();
        let mut service = server(root, broken);
        proxy.point(Some(&service));
        let mut client = Process::start(&[
            "client".into(),
            "--address".into(),
            proxy.address.to_string(),
            "--batch".into(),
            "32".into(),
        ]);
        assert_eq!(json_wire(client.address, "STATUS")["acked"], 0);
        assert_eq!(
            json_wire(client.address, "LOAD 1"),
            json!({"acked":32,"generation":1,"total":32})
        );
        assert_eq!(wire(proxy.address, "GET k/000000"), "VALUE 1 v000000");
        let mut service_log = String::new();
        if restart {
            proxy.point(None);
            service_log += &service.stop();
            assert_eq!(
                json_wire(client.address, "PROBE"),
                json!({"attempts":4,"failed":4,"acked":0})
            );
            service = server(root, broken);
            proxy.point(Some(&service));
            assert_eq!(
                wire(proxy.address, "GET k/000000"),
                if broken {
                    "MISSING 2"
                } else {
                    "VALUE 2 v000000"
                }
            );
            assert_eq!(json_wire(client.address, "LOAD 2")["acked"], 32);
            assert_eq!(json_wire(client.address, "LOAD 2")["total"], 96);
            proxy.point(None);
            service_log += &service.stop();
            service = server(root, broken);
            proxy.point(Some(&service));
            assert_eq!(json_wire(client.address, "LOAD 3")["total"], 128);
        } else {
            for total in [64, 96, 128] {
                assert_eq!(json_wire(client.address, "LOAD 1")["total"], total);
            }
        }
        let missing = if broken && restart { 96 } else { 0 };
        let expected = json!({"status":if missing>0{"fail"}else{"pass"},"acked":128,"pre_restart":if restart{32}else{128},"post_restart":if restart{32}else{128},"generation":if restart{3}else{1},"missing":missing});
        assert_eq!(json_wire(client.address, "VERIFY"), expected);
        assert_eq!(json_wire(client.address, "STATUS")["acked"], 128);
        assert_eq!(json_wire(client.address, "VERIFY"), expected);
        let client_log = client.stop();
        proxy.point(None);
        service_log += &service.stop();
        proxy.stop();
        assert_eq!(client_log.matches("RESULT ").count(), 1);
        assert_eq!(client_log.matches("MISSING_ACK ").count(), missing);
        if restart && !broken {
            assert!(service_log.contains("generation=2 recovered=32"));
            assert!(service_log.contains("generation=3 recovered=96"));
        }
    }
}

#[test]
fn a_lost_put_response_retries_the_same_key_without_manufacturing_an_acknowledgement() {
    let directory = tempfile::tempdir().unwrap();
    let mut service = server(directory.path(), false);
    let mut proxy = Proxy::new();
    proxy.point(Some(&service));
    proxy.lose_first.store(true, Ordering::Relaxed);
    let mut client = Process::start(&[
        "client".into(),
        "--address".into(),
        proxy.address.to_string(),
    ]);
    assert_eq!(
        json_wire(client.address, "LOAD 1"),
        json!({"generation":1,"acked":32,"total":32})
    );
    client.stop();
    proxy.point(None);
    service.stop();
    let requests = proxy.stop();
    assert!(requests.len() >= 33, "{requests:?}");
    assert_eq!(requests[0], requests[1]);
    // Host delays can cause additional retries. Every retry must repeat the
    // current unacknowledged write, preserving the complete ordered workload.
    let mut writes = requests;
    writes.dedup();
    assert_eq!(
        writes,
        (0..32)
            .map(|sequence| format!("PUT k/{sequence:06} v{sequence:06}"))
            .collect::<Vec<_>>()
    );
    let mut reopened = server(directory.path(), false);
    assert_eq!(wire(reopened.address, "GET k/000000"), "VALUE 2 v000000");
    let log = reopened.stop();
    assert!(log.contains("recovered=32"));
}

#[test]
fn volume_oracle_reads_acknowledged_values_after_an_actual_process_restart() {
    for broken in [false, true] {
        let directory = tempfile::tempdir_in("/tmp").unwrap();
        let state_root = directory.path().to_owned();
        let proxy = Proxy::new();
        let mut service = server(&state_root, broken);
        proxy.point(Some(&service));
        let listener = UnixListener::bind(state_root.join("control.sock")).unwrap();
        listener.set_nonblocking(true).unwrap();
        let backend = proxy.backend.clone();
        let controller = OwnedThread(Some(thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(5);
            let (mut channel, _) = loop {
                match listener.accept() {
                    Ok(connected) => break connected,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(Instant::now() < deadline, "volume oracle did not connect");
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => panic!("volume control accept: {error}"),
                }
            };
            channel.set_nonblocking(false).unwrap();
            channel
                .set_write_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            channel
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut reader = BufReader::new(channel.try_clone().unwrap());
            let mut operations = vec![];
            for id in 1..=3 {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                let request: Value = serde_json::from_str(&line).unwrap();
                operations.push(request["op"].clone());
                match request["op"].as_str().unwrap() {
                    "crash" => {
                        *backend.lock().unwrap() = None;
                        service.stop();
                    }
                    "sleep" => assert_eq!(request["duration_ns"], 500_000_000),
                    "reboot" => {
                        service = server(&state_root, broken);
                        *backend.lock().unwrap() = Some(service.address);
                    }
                    other => panic!("unexpected operation {other}"),
                };
                channel.write_all(format!("{}\n",json!({"version":1,"id":id,"ok":true,"result":{},"requested_at_ns":10,"admitted_at_ns":20,"effect_at_ns":30,"replied_at_ns":40})).as_bytes()).unwrap();
            }
            (service, operations)
        })));
        let script="import sys; sys.path.insert(0,sys.argv[1]); import volume; from seedbox import World; sys.exit(volume.main(sys.argv[2],lambda:World.connect(sys.argv[3])))";
        let output = python()
            .args(["-c", script])
            .arg(root().join("harness/acceptance/driver"))
            .arg(proxy.address.to_string())
            .arg(directory.path().join("control.sock"))
            .output()
            .unwrap();
        let (mut service, operations) = controller.join();
        assert_eq!(
            operations,
            json!(["crash", "sleep", "reboot"])
                .as_array()
                .unwrap()
                .as_slice()
        );
        if broken {
            assert!(!output.status.success());
            assert!(String::from_utf8_lossy(&output.stderr).contains("lost or changed"));
        } else {
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(
                String::from_utf8_lossy(&output.stdout).contains("acknowledged_and_recovered=20")
            );
        }
        proxy.point(None);
        service.stop();
    }
}

#[test]
fn race_oracle_requires_the_atomic_control_to_preserve_every_update() {
    if !process::isolated(concat!(
        module_path!(),
        "::race_oracle_requires_the_atomic_control_to_preserve_every_update"
    )) {
        return;
    }
    let directory = tempfile::tempdir().unwrap();
    let executable = directory.path().join("race");
    fs::write(&executable,"#!/bin/sh\nif [ \"$2\" = atomic ]; then printf 'RACE mode=atomic counter=%s expected=10 lost=%s\\n' \"$SEEDBOX_ATOMIC_COUNT\" \"$SEEDBOX_ATOMIC_LOST\"; else printf 'RACE mode=racy counter=7 expected=10 lost=3\\n'; fi\n").unwrap();
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o755)).unwrap();
    for lost in [0, 1] {
        let output = python()
            .args([
                "-c",
                "import sys; sys.path.insert(0,sys.argv[1]); import race; race.main(sys.argv[2])",
            ])
            .arg(root().join("harness/acceptance/driver"))
            .arg(&executable)
            .env("SEEDBOX_ATOMIC_COUNT", (10 - lost).to_string())
            .env("SEEDBOX_ATOMIC_LOST", lost.to_string())
            .output()
            .unwrap();
        assert_eq!(output.status.success(), lost == 0);
        if lost > 0 {
            assert!(String::from_utf8_lossy(&output.stderr).contains("atomic control lost updates"));
        }
    }
}
