use std::{
    collections::BTreeMap,
    io::{BufRead, BufReader, Read, Write},
    net::{TcpListener, TcpStream},
    path::Path,
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

#[test]
fn live_synthetic_projects_share_client_and_faults_and_have_restart_dependent_control() {
    let temp = tempfile::tempdir().unwrap();
    let destination = temp.path().join("projects");
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("../examples/faults/generate.py");
    assert!(Command::new("python3")
        .arg(script)
        .arg(&destination)
        .status()
        .unwrap()
        .success());
    let fixed =
        seedbox::config::load(&destination.join("fixed/compose.yaml"), &BTreeMap::new()).unwrap();
    let broken =
        seedbox::config::load(&destination.join("broken/compose.yaml"), &BTreeMap::new()).unwrap();
    assert_eq!(fixed.services.len(), 2);
    assert_eq!(fixed.faults, broken.faults);
    assert_eq!(fixed.faults.len(), 8);
    assert_eq!(fixed.seed, broken.seed);
    assert_eq!(
        fixed.services["client"].command,
        broken.services["client"].command
    );
    assert!(!fixed.services["server"]
        .command
        .as_ref()
        .unwrap()
        .contains(&"--broken".into()));
    assert!(broken.services["server"]
        .command
        .as_ref()
        .unwrap()
        .contains(&"--broken".into()));
    assert_eq!(fixed.schedule.stop_tolerance_ns, 2_000_000);
    let no_faults = seedbox::config::load(
        &destination.join("broken-no-faults/compose.yaml"),
        &BTreeMap::new(),
    )
    .unwrap();
    assert!(no_faults.faults.is_empty());
    assert!(no_faults.services["server"]
        .command
        .as_ref()
        .unwrap()
        .contains(&"--broken".into()));
    for variant in ["fixed", "broken", "broken-no-faults"] {
        assert_eq!(
            std::fs::read(destination.join(variant).join("workload/main.rs")).unwrap(),
            include_bytes!("../../examples/faults/workload/main.rs")
        );
    }
}

struct Server(Child);
impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
fn compile(directory: &Path) -> std::path::PathBuf {
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("../examples/faults/workload/main.rs");
    let binary = directory.join("synthetic");
    assert!(Command::new("rustc")
        .args(["--edition=2021", "-C", "opt-level=1"])
        .arg(source)
        .arg("-o")
        .arg(&binary)
        .status()
        .unwrap()
        .success());
    binary
}
fn wire(address: &str, request: &str) -> String {
    let mut stream = TcpStream::connect(address).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    writeln!(stream, "{request}").unwrap();
    let mut response = String::new();
    BufReader::new(stream).read_line(&mut response).unwrap();
    response.trim().into()
}
fn start(binary: &Path, address: &str, path: &Path, broken: bool) -> Server {
    let mut command = Command::new(binary);
    command
        .args(["server", "--listen", address, "--state"])
        .arg(path)
        .stdout(Stdio::null());
    if broken {
        command.arg("--broken");
    }
    let mut server = Server(command.spawn().unwrap());
    let deadline = Instant::now() + Duration::from_secs(3);
    while TcpStream::connect(address).is_err() {
        assert!(
            server.0.try_wait().unwrap().is_none(),
            "server failed to start"
        );
        assert!(Instant::now() < deadline, "server readiness timeout");
        std::thread::sleep(Duration::from_millis(5));
    }
    server
}
fn address() -> String {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .to_string()
}

#[test]
fn actual_servers_only_lose_acknowledged_keys_after_broken_restart() {
    let temp = tempfile::tempdir().unwrap();
    let binary = compile(temp.path());
    for broken in [false, true] {
        let state = temp.path().join(if broken {
            "memory/kv.log"
        } else {
            "durable/kv.log"
        });
        let address = address();
        let server = start(&binary, &address, &state, broken);
        assert_eq!(wire(&address, "PUT k/first value"), "OK 1");
        assert_eq!(wire(&address, "GET k/first"), "VALUE 1 value"); // Broken without restart is sound.
        drop(server); // Real abrupt process kill, separate from a Store unit test.
        let server = start(&binary, &address, &state, broken);
        assert_eq!(wire(&address, "PING"), "PONG 2");
        assert_eq!(
            wire(&address, "GET k/first"),
            if broken { "MISSING 2" } else { "VALUE 2 value" }
        );
        assert_eq!(wire(&address, "PUT k/second next"), "OK 2");
        drop(server);
        let server = start(&binary, &address, &state, broken);
        assert_eq!(
            wire(&address, "GET k/second"),
            if broken { "MISSING 3" } else { "VALUE 3 next" }
        );
        if !broken {
            assert_eq!(wire(&address, "GET k/first"), "VALUE 3 value");
        }
        drop(server);
    }
}

#[test]
fn actual_independent_client_detects_broken_loss_and_memory_control_passes_without_restart() {
    let temp = tempfile::tempdir().unwrap();
    let binary = compile(temp.path());
    for (broken, restart, expected) in [(false, true, 0), (true, true, 1), (true, false, 0)] {
        let state = temp.path().join(format!("case-{broken}-{restart}/kv.log"));
        let address = address();
        let mut server = Some(start(&binary, &address, &state, broken));
        let mut client = Command::new(&binary)
            .args([
                "client",
                "--address",
                &address,
                "--duration-ms",
                "500",
                "--verify-timeout-ms",
                "2000",
                "--required-generation",
                if restart { "2" } else { "1" },
            ])
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut stdout = BufReader::new(client.stdout.take().unwrap());
        let mut first = String::new();
        stdout.read_line(&mut first).unwrap();
        assert!(first.starts_with("ACK_PHASE generation=1"), "{first}");
        if restart {
            drop(server.take());
            server = Some(start(&binary, &address, &state, broken));
        }
        let mut rest = String::new();
        stdout.read_to_string(&mut rest).unwrap();
        assert_eq!(
            client.wait().unwrap().code(),
            Some(expected),
            "{first}{rest}"
        );
        assert!(
            rest.contains(if expected == 1 {
                "RESULT fail"
            } else {
                "RESULT pass"
            }),
            "{rest}"
        );
        if restart {
            assert!(rest.contains("generation=2"), "{rest}");
        }
        if expected == 1 {
            assert!(rest.contains("MISSING_ACK key="), "{rest}");
        }
        drop(server);
    }
}

#[test]
fn refusing_reused_evidence_output_keeps_the_previous_report_and_logs_unchanged() {
    let temp = tempfile::tempdir().unwrap();
    let report = temp.path().join("acceptance.json");
    let previous = b"previous completed evidence must remain unchanged\n";
    std::fs::write(&report, previous).unwrap();
    std::fs::create_dir(temp.path().join("projects")).unwrap();
    std::fs::create_dir(temp.path().join("commands")).unwrap();
    let log = temp.path().join("commands/01.stdout");
    std::fs::write(&log, b"retained previous output").unwrap();
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("../examples/faults/run.py");
    let result = Command::new("python3")
        .arg(script)
        .args([
            "--seedbox",
            "unused",
            "--vmm",
            "unused",
            "--kernel",
            "unused",
            "--guest-init",
            "unused",
            "--cpu",
            "14",
            "--state",
            "unused",
            "--output",
        ])
        .arg(temp.path())
        .output()
        .unwrap();
    assert!(!result.status.success());
    assert!(
        String::from_utf8_lossy(&result.stderr).contains("refusing output with existing evidence")
    );
    assert_eq!(std::fs::read(&report).unwrap(), previous);
    assert_eq!(std::fs::read(log).unwrap(), b"retained previous output");
    assert!(!temp.path().join("acceptance.json.tmp").exists());
}

#[test]
fn output_reservation_allows_supplied_init_binaries_and_retains_subsequent_failure() {
    let temp = tempfile::tempdir().unwrap();
    let init = temp.path().join("compose-init");
    let bytes = b"\x7fELFprecreated integration input";
    std::fs::write(&init, bytes).unwrap();
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("../examples/faults/run.py");
    // Invalid generation seed fails before any host inspection or execution.
    let result = Command::new("python3")
        .arg(script)
        .args([
            "--seedbox",
            "unused",
            "--vmm",
            "unused",
            "--kernel",
            "unused",
            "--cpu",
            "14",
            "--state",
            "unused",
            "--seed",
            "-1",
            "--guest-init",
        ])
        .arg(&init)
        .arg("--output")
        .arg(temp.path())
        .output()
        .unwrap();
    assert!(!result.status.success());
    let report: serde_json::Value =
        serde_json::from_slice(&std::fs::read(temp.path().join("acceptance.json")).unwrap())
            .unwrap();
    assert_eq!(report["status"], "failed");
    assert!(report["error"].as_str().unwrap().contains("unsigned64-bit"));
    assert_eq!(std::fs::read(init).unwrap(), bytes);
}
