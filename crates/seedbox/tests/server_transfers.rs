//! Public CLI/client/server bundle interaction; the native execution path is
//! deliberately unavailable for this catalogue-only fixture.
use seedbox_api::*;
use seedbox_application::{definition, store::ContentStore};
use seedbox_artifacts::{
    archive::{self, Options, Snapshot},
    execution::*,
    record::*,
};
use seedbox_server::{catalogue::Catalogue, manager::Manager};
use std::{
    collections::BTreeMap,
    fs,
    path::Path,
    process::{Child, Command, Output},
    time::{Duration, Instant},
};

const RAW: &[u8] = b"console\0\xffmanagement\nworker error";
fn bundle(path: &Path) {
    let store = ContentStore::new(path.join("objects"));
    store.put(b"catalogue-only-binary").unwrap();
    let manager = Manager::new(Catalogue::in_memory());
    let simulation = manager
        .submit(resolved_request("transfer-source"))
        .unwrap()
        .0;
    let id = simulation.metadata.id;
    manager.begin(&id).unwrap();
    let simulation = manager
        .update(&id, |s| {
            s.status.phase = Phase::Completed;
            s.status.oracle_exit_code = Some(0);
            s.status.termination = Some(ExecutionTermination::OracleExited);
        })
        .unwrap();
    let InputSource::Resolved { definition: def } = &simulation.spec.inputs else {
        unreachable!()
    };
    let identity = RunIdentity {
        version: FORMAT_VERSION,
        seed: def.seed,
        configuration: simulation.spec.configuration.clone(),
        inputs: definition::prepared(def, &store).unwrap(),
        program: Vec::new(),
        interventions: None,
    };
    let digest = store.put_unsynced(RAW).unwrap();
    let record = RunRecord {
        run_id: seedbox_artifacts::store::ContentDigest::of(
            &serde_json::to_vec(&identity).unwrap(),
        ),
        identity,
        attempt: "/never/follow/archived/host/path".into(),
        performance_parameters: BTreeMap::new(),
        allow_engine_change: false,
        log_policy: Default::default(),
        omitted_logs: BTreeMap::from([(
            "vmm.stderr".into(),
            "truncated at retention limit".into(),
        )]),
        outcome: Outcome::Pass,
        report: Some(ExecutionReport {
            host_wait_ns: 0,
            simulated_time_ns: Some(25),
            reason: StopReason::Completed,
            workload_exit: Some(WorkloadExit {
                service: "oracle".into(),
                code: 0,
            }),
            witness: Some(seedbox_artifacts::store::ContentDigest::of(b"witness")),
            full_state: None,
            faults: Vec::new(),
            detail: None,
        }),
        error: None,
        logs: BTreeMap::from([
            ("machines/oracle.console".into(), digest.clone()),
            ("machines/oracle.mgmt".into(), digest.clone()),
            ("machines/oracle.console.chunks".into(), digest.clone()),
            ("vmm.stdout".into(), digest.clone()),
            ("vmm.stderr".into(), digest),
        ]),
        replay_of: None,
        replay: None,
    };
    let mut observations = BTreeMap::new();
    // serde tags belong to the public delivery types.
    observations.insert(
        "logs".into(),
        store
            .put_unsynced(&serde_json::to_vec(&vec![LogDelivery::Closed]).unwrap())
            .unwrap(),
    );
    observations.insert("logs:oracle".into(), observations["logs"].clone());
    observations.insert(
        "events".into(),
        store
            .put_unsynced(&serde_json::to_vec(&vec![EventDelivery::Closed]).unwrap())
            .unwrap(),
    );
    let snapshot = Snapshot {
        world: manager.world(&id).unwrap(),
        simulation,
        record: Some(record),
        checkpoints: BTreeMap::new(),
        controls: Vec::new(),
        observations,
        omissions: BTreeMap::from([("frames".into(), "not retained".into())]),
    };
    archive::export(
        &snapshot,
        &store,
        fs::File::create(path.join("fixture.tar")).unwrap(),
        &Options::default(),
    )
    .unwrap();
}
struct Server(Child);
impl Drop for Server {
    fn drop(&mut self) {
        unsafe {
            libc::kill(self.0.id() as i32, libc::SIGTERM);
        }
        let _ = self.0.wait();
    }
}
fn command(root: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_seedbox"));
    command
        .env_clear()
        .env("HOME", root)
        .env("TMPDIR", root)
        .env("PATH", "")
        .env("SEEDBOX_STATE", root.join("server"))
        .current_dir(root);
    command
}
fn run(root: &Path, args: &[&str]) -> Output {
    command(root).args(args).output().unwrap()
}
fn success(output: Output) -> Output {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

#[test]
fn cli_bundle_roundtrip_diagnostics_and_json_without_execution() {
    let root = tempfile::Builder::new().tempdir_in("/tmp").unwrap();
    bundle(root.path());
    let mut server = command(root.path());
    server
        .args(["serve"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    let _server = Server(server.spawn().unwrap());
    let client =
        seedbox_client::Client::connect_to(&root.path().join("server/server.sock")).unwrap();
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while rt.block_on(client.info()).is_err() {
        assert!(Instant::now() < deadline, "server startup timed out");
        std::thread::sleep(Duration::from_millis(20));
    }
    let imported = success(run(root.path(), &["import", "fixture.tar", "--json"]));
    assert!(imported.stderr.is_empty());
    let simulation: Simulation = serde_json::from_slice(&imported.stdout).unwrap();
    assert_eq!(simulation.status.phase, Phase::Completed);
    let id = simulation.metadata.id.as_str();
    let exported = success(run(root.path(), &["export", id]));
    assert_eq!(
        exported.stdout,
        format!("EXPORTED: {id}.seedbox.tar\n").as_bytes()
    );
    let receipt = success(run(
        root.path(),
        &["export", id, "-o", "second.tar", "--json"],
    ));
    let receipt: serde_json::Value = serde_json::from_slice(&receipt.stdout).unwrap();
    assert_eq!(receipt["simulation"], id);
    assert_eq!(
        receipt["sizeBytes"],
        fs::metadata(root.path().join("second.tar")).unwrap().len()
    );
    let imported_again = success(run(
        root.path(),
        &["import", &format!("{id}.seedbox.tar"), "--json"],
    ));
    let second: Simulation = serde_json::from_slice(&imported_again.stdout).unwrap();
    assert_ne!(simulation.metadata.id, second.metadata.id);
    let comparison = success(run(
        root.path(),
        &["compare", id, second.metadata.id.as_str(), "--json"],
    ));
    let comparison: SimulationComparison = serde_json::from_slice(&comparison.stdout).unwrap();
    assert_eq!(comparison.status, ComparisonStatus::Match);
    let metadata = success(run(root.path(), &["diagnostics", id, "--json"]));
    let metadata: DiagnosticFiles = serde_json::from_slice(&metadata.stdout).unwrap();
    assert_eq!(metadata.files.len(), 5);
    assert_eq!(
        metadata.omissions["vmm.stderr"],
        "truncated at retention limit"
    );
    let table = success(run(root.path(), &["diagnostics", id]));
    assert!(String::from_utf8_lossy(&table.stdout).contains("machines/oracle.console"));
    for name in [
        "machines/oracle.console",
        "machines/oracle.mgmt",
        "machines/oracle.console.chunks",
        "vmm.stdout",
        "vmm.stderr",
    ] {
        let raw = success(run(root.path(), &["diagnostics", id, name]));
        assert_eq!(raw.stdout, RAW);
        assert!(String::from_utf8_lossy(&raw.stderr).contains("omission:"));
        let json = success(run(root.path(), &["diagnostics", id, name, "--json"]));
        assert!(json.stderr.is_empty());
        let document: DiagnosticDocument = serde_json::from_slice(&json.stdout).unwrap();
        let mut pretty = serde_json::to_vec_pretty(&document).unwrap();
        pretty.push(b'\n');
        assert_eq!(
            json.stdout, pretty,
            "named diagnostics must emit one pretty API document"
        );
        assert_eq!(document.file.name, name);
        assert_eq!(
            document.omissions["vmm.stderr"],
            "truncated at retention limit"
        );
        assert_eq!(
            document.payload_hex,
            RAW.iter().map(|b| format!("{b:02x}")).collect::<String>()
        );
    }
    for name in ["missing", "../../etc/passwd", "/etc/passwd"] {
        let rejected = run(root.path(), &["diagnostics", id, name]);
        assert_eq!(rejected.status.code(), Some(2));
        assert!(rejected.stdout.is_empty());
        assert!(String::from_utf8_lossy(&rejected.stderr).contains("choose an exact name"));
        // Exercise the server allowlist directly, not only CLI metadata selection.
        let mut spool = tokio::fs::File::from_std(tempfile::tempfile().unwrap());
        let descriptor = DiagnosticFile {
            name: name.into(),
            sha256: "0".repeat(64),
            size_bytes: 0,
        };
        assert!(rt
            .block_on(client.diagnostic_file(
                &simulation.metadata.id,
                &descriptor,
                &mut spool,
                BUNDLE_MAX_BYTES,
                || false
            ))
            .is_err());
    }
    let diagnostic_limit = run(
        root.path(),
        &["diagnostics", id, "vmm.stdout", "--max-bytes", "1"],
    );
    assert_eq!(diagnostic_limit.status.code(), Some(2));
    assert!(diagnostic_limit.stdout.is_empty());
    assert!(String::from_utf8_lossy(&diagnostic_limit.stderr).contains("wire byte limit"));
    let limited = run(
        root.path(),
        &["export", id, "-o", "limited.tar", "--max-bytes", "1"],
    );
    assert_eq!(limited.status.code(), Some(2));
    assert!(!root.path().join("limited.tar").exists());
    assert!(!fs::read_dir(root.path()).unwrap().any(|e| e
        .unwrap()
        .file_name()
        .to_string_lossy()
        .starts_with(".seedbox-export-")));
    // Shared parent cleanup is conservative, but export and JSON stdout still succeed.
    use std::os::unix::fs::PermissionsExt;
    fs::create_dir(root.path().join("shared")).unwrap();
    fs::set_permissions(
        root.path().join("shared"),
        fs::Permissions::from_mode(0o777),
    )
    .unwrap();
    let shared = success(run(
        root.path(),
        &["export", id, "-o", "shared/result.tar", "--json"],
    ));
    let shared_receipt: serde_json::Value = serde_json::from_slice(&shared.stdout).unwrap();
    assert_eq!(shared_receipt["output"], "shared/result.tar");
    let warning = String::from_utf8(shared.stderr).unwrap();

    assert!(
        warning.starts_with("export staging cleanup skipped for "),
        "{warning}"
    );
    assert!(warning.contains("output parent is shared or its ACL cannot be verified"));
    let retained: Vec<_> = fs::read_dir(root.path().join("shared"))
        .unwrap()
        .filter_map(|entry| {
            let entry = entry.unwrap();
            entry
                .file_name()
                .to_string_lossy()
                .starts_with(".seedbox-export-")
                .then_some(entry.path())
        })
        .collect();
    assert_eq!(retained.len(), 1);
    assert!(fs::read_dir(&retained[0]).unwrap().next().is_none());
    assert!(rt.block_on(client.capacity()).unwrap().occupying.is_empty());
    assert_eq!(rt.block_on(client.list(None)).unwrap().items.len(), 2);
}
use seedbox_api::{InputSource, SimulationSpec, SubmitRequest};

fn request() -> SubmitRequest {
    SubmitRequest {
        metadata: Default::default(),
        options: Default::default(),
        request_id: "submit-123".into(),
        spec: SimulationSpec {
            program: Vec::new(),
            start_paused: false,
            seed: Some(123),
            inputs: InputSource::Project,
            configuration: seedbox_application::config::parse(
                "name: test\nservices:\n  oracle:\n    image: example/oracle\n",
                std::path::Path::new("/project/compose.yaml"),
                &Default::default(),
            )
            .unwrap(),
        },
    }
}

/// A complete input closure for catalogue/transport tests; execution is never launched.
fn resolved_request(id: &str) -> SubmitRequest {
    use seedbox_application::{
        execution::{
            Backend, ExecutionProfile, PreparedImage, PreparedInputs, SemanticParameters,
            WitnessMode,
        },
        image::Launch,
        store::ContentDigest,
    };
    let mut request = request();
    request.request_id = id.into();
    request.metadata.name = "test".into();
    let binary = ContentDigest::of(b"catalogue-only-binary");
    let inputs = PreparedInputs {
        machine_memory_mb: Default::default(),
        wall_clock_epoch: Some(86400),
        kernel: binary.clone(),
        images: std::collections::BTreeMap::from([(
            "oracle".into(),
            PreparedImage {
                manifest: binary.clone(),
                config: binary.clone(),
                layers: Vec::new(),
                launch: Launch {
                    argv: vec!["/test".into()],
                    environment: Default::default(),
                    working_dir: "/".into(),
                    user: "0:0".into(),
                    volumes: Vec::new(),
                    disk_volumes: Vec::new(),
                },
            },
        )]),
        placement: std::collections::BTreeMap::from([("oracle".into(), "oracle".into())]),
        guests: std::collections::BTreeMap::from([("oracle".into(), binary.clone())]),
        profile: ExecutionProfile {
            backend: Backend::Other("catalogue-test".into()),
            engine: binary,
            witness_mode: WitnessMode::Exits,
            semantic_parameters: SemanticParameters::new(1024),
        },
        auxiliary: Default::default(),
        provenance: Default::default(),
    };
    let definition = seedbox_application::definition::resolve(
        &request.spec.configuration,
        &inputs,
        123,
        Vec::new(),
    )
    .unwrap();
    request.spec = SimulationSpec::resolved(definition);
    request
}

#[test]
fn diagnostics_metadata_stall_exits_promptly_on_sigint_without_stdout() {
    use std::{
        io::{BufRead, Read},
        os::unix::net::UnixListener,
        sync::mpsc,
    };
    let root = tempfile::Builder::new().tempdir_in("/tmp").unwrap();
    use std::os::unix::fs::DirBuilderExt;
    fs::DirBuilder::new()
        .mode(0o700)
        .create(root.path().join("server"))
        .unwrap();
    let listener = UnixListener::bind(root.path().join("server/server.sock")).unwrap();
    let (requested, ready) = mpsc::channel();
    let (release, done) = mpsc::channel();
    let thread = std::thread::spawn(move || {
        loop {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = std::io::BufReader::new(stream);
            let mut line = String::new();
            if reader.read_line(&mut line).unwrap() == 0 {
                continue;
            } // Daemon listening probe.
            assert!(line.starts_with("GET /v1/simulations/k7m2p9qx/diagnostics "));
            requested.send(()).unwrap();
            done.recv_timeout(Duration::from_secs(5)).unwrap(); // Intentionally never return metadata.
            break;
        }
    });
    let mut child = command(root.path())
        .args(["diagnostics", "k7m2p9qx", "--json"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    ready.recv_timeout(Duration::from_secs(5)).unwrap();
    assert_eq!(unsafe { libc::kill(child.id() as i32, libc::SIGINT) }, 0);
    let deadline = Instant::now() + Duration::from_secs(2);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if Instant::now() >= deadline {
            child.kill().unwrap();
            child.wait().unwrap();
            release.send(()).unwrap();
            thread.join().unwrap();
            panic!("diagnostics ignored SIGINT while metadata was stalled");
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    let mut stdout = Vec::new();
    let mut stderr = String::new();
    child
        .stdout
        .take()
        .unwrap()
        .read_to_end(&mut stdout)
        .unwrap();
    child
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut stderr)
        .unwrap();
    release.send(()).unwrap();
    thread.join().unwrap();
    assert_eq!(status.code(), Some(2));
    assert!(
        stdout.is_empty(),
        "cancelled metadata must not publish partial stdout"
    );
    assert!(stderr.starts_with("error: transfer cancelled"), "{stderr}");
}
