use std::{
    fs,
    path::Path,
    process::{Command, Output},
};

/// Run the CLI with a scrubbed environment: no `SEEDBOX_*`, `CARGO*` or
/// `SUDO_USER` variables, `HOME` and scratch storage at `cwd`, and `PATH` set to
/// `path` (empty if `None`), so no default can come from the developer's machine.
fn seedbox_with(args: &[&str], cwd: &Path, path: Option<&Path>) -> Output {
    Command::new(env!("CARGO_BIN_EXE_seedbox"))
        .args(args)
        .current_dir(cwd)
        .env_clear()
        .env("HOME", cwd)
        .env("TMPDIR", cwd)
        .env("PATH", path.map(Path::as_os_str).unwrap_or_default())
        .output()
        .unwrap()
}

fn seedbox(args: &[&str], cwd: &Path) -> Output {
    seedbox_with(args, cwd, None)
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn host_pause_commands_acknowledge_boundaries_and_reject_inactive_runs() {
    let directory = tempfile::Builder::new().tempdir_in("/tmp").unwrap();
    let cwd = directory.path();
    for command in ["pause", "resume"] {
        let help = seedbox(&["help", command], cwd);
        assert!(help.status.success());
        assert!(String::from_utf8_lossy(&help.stdout).contains("<RUN>"));
        let missing = seedbox(&[command], cwd);
        assert_eq!(missing.status.code(), Some(2));
        assert!(stderr(&missing).contains("<RUN>"));
        let inactive = seedbox(&[command, "old-run"], cwd);
        assert_eq!(inactive.status.code(), Some(2));
        assert_eq!(stderr(&inactive), "error: host control old-run: not available; use an executing run/replay with a host-control engine and the same user (or root)\n");
    }
    fs::create_dir_all(cwd.join("run/output")).unwrap();
    let socket = cwd.join("socket");
    fs::write(
        cwd.join("run/output/host-control"),
        socket.as_os_str().as_encoded_bytes(),
    )
    .unwrap();
    let server = std::os::unix::net::UnixDatagram::bind(&socket).unwrap();
    std::thread::scope(|scope| {
        let worker = scope.spawn(|| {
            for (request, response) in [
                (b"pause".as_slice(), b"paused".as_slice()),
                (b"resume", b"running"),
            ] {
                let mut bytes = [0; 32];
                let (n, peer) = server.recv_from(&mut bytes).unwrap();
                assert_eq!(&bytes[..n], request);
                server
                    .send_to(response, peer.as_pathname().unwrap())
                    .unwrap();
            }
        });
        for (command, status) in [("pause", "PAUSED"), ("resume", "RESUMED")] {
            let output = seedbox(&[command, "run"], cwd);
            assert!(output.status.success(), "{}", stderr(&output));
            assert!(output.stderr.is_empty());
            assert_eq!(
                String::from_utf8_lossy(&output.stdout),
                format!("{status}: run\n")
            );
        }
        worker.join().unwrap();
    });
    drop(server);
    fs::remove_file(socket).unwrap();
    let stale = seedbox(&["pause", "run"], cwd);
    assert_eq!(stale.status.code(), Some(2));
    assert!(stderr(&stale).contains("not available"));
}

#[test]
fn host_pause_rejects_invalid_acknowledgements_and_bounds_missing_replies() {
    let directory = tempfile::Builder::new().tempdir_in("/tmp").unwrap();
    let cwd = directory.path();
    fs::create_dir_all(cwd.join("run/output")).unwrap();
    let socket = cwd.join("socket");
    fs::write(
        cwd.join("run/output/host-control"),
        socket.as_os_str().as_encoded_bytes(),
    )
    .unwrap();
    let server = std::os::unix::net::UnixDatagram::bind(socket).unwrap();
    std::thread::scope(|scope| {
        let worker = scope.spawn(|| {
            let mut bytes = [0; 32];
            let (_, peer) = server.recv_from(&mut bytes).unwrap();
            server
                .send_to(b"invalid", peer.as_pathname().unwrap())
                .unwrap();
            // Receive the second request but deliberately never acknowledge it.
            server.recv_from(&mut bytes).unwrap();
        });
        let invalid = seedbox(&["resume", "run"], cwd);
        assert_eq!(invalid.status.code(), Some(2));
        assert!(invalid.stdout.is_empty());
        assert_eq!(stderr(&invalid), "error: host control run: invalid boundary acknowledgement; use an executing run/replay with a host-control engine and the same user (or root)\n");
        let unanswered = seedbox(&["pause", "run"], cwd);
        assert_eq!(unanswered.status.code(), Some(2));
        assert!(unanswered.stdout.is_empty());
        assert_eq!(stderr(&unanswered), "error: host control run: no boundary acknowledgement within 5s (request may still take effect); use an executing run/replay with a host-control engine and the same user (or root)\n");
        worker.join().unwrap();
    });
}

#[test]
fn validation_cli_reports_structured_output_and_nonzero_errors() {
    let directory = tempfile::tempdir().unwrap();
    let source = directory.path().join("compose.yaml");
    let yaml = "name: cli-test\nservices:\n  workload: {image: example/workload:v1}\nx-seedbox:\n  version: 1\n  seed: 0\n  limits: {simulated_time: 1s, wall_time: 1m}\n  check: {service: workload}\n";
    fs::write(&source, yaml).unwrap();
    let success = seedbox(&["validate", "compose.yaml", "--json"], directory.path());
    assert!(success.status.success(), "{}", stderr(&success));
    let resolved: serde_json::Value = serde_json::from_slice(&success.stdout).unwrap();
    assert_eq!(resolved["seed"], 0);
    fs::write(
        &source,
        yaml.replace(
            "image: example/workload:v1",
            "image: example/workload:v1, volumes: [./host:/data]",
        ),
    )
    .unwrap();
    let failure = seedbox(&["validate", "compose.yaml"], directory.path());
    assert_eq!(failure.status.code(), Some(2));
    assert!(stderr(&failure).contains("$.services.workload.volumes"));
    assert!(!directory.path().join("runs").exists());
}

#[test]
fn compose_argument_defaults_to_the_project_directory() {
    let directory = tempfile::tempdir().unwrap();
    let project = directory.path().join("demo");
    let created = seedbox(&["init", "demo"], directory.path());
    assert!(created.status.success());
    assert_eq!(String::from_utf8_lossy(&created.stdout), "CREATED: demo\n");
    for args in [
        &["validate"][..],
        &["validate", "."],
        &["validate", "compose.yaml"],
    ] {
        let output = seedbox(args, &project);
        assert!(output.status.success(), "{args:?}");
        assert!(String::from_utf8_lossy(&output.stdout).starts_with("VALID: etcd-demo "));
    }
    let output = seedbox(&["validate", "demo"], directory.path());
    assert!(output.status.success());
}

#[test]
fn setup_errors_name_the_missing_input_and_the_fix() {
    let directory = tempfile::tempdir().unwrap();
    let cwd = directory.path();
    let cases: [(&[&str], &str); 5] = [
        (
            &["init", "."],
            "error: .: already exists; choose a new directory\n",
        ),
        (
            &["validate", "nope.yaml"],
            "error: Compose file nope.yaml: not found\n",
        ),
        (
            &["validate"],
            "error: no Compose file found; searched:\n  ./compose.yaml\n  ./compose.yml\n  \
             ./docker-compose.yaml\n  ./docker-compose.yml\npass the file path directly\n",
        ),
        (
            &["inspect", "no-such-run"],
            "error: run directory no-such-run: not found\n",
        ),
        (
            &["inspect", "."],
            "error: .: not a run directory (no manifest.json)\n",
        ),
    ];
    for (args, expected) in cases {
        let output = seedbox(args, cwd);
        assert_eq!(output.status.code(), Some(2), "{args:?}");
        assert_eq!(stderr(&output), expected, "{args:?}");
    }

    fs::write(cwd.join("out.json"), "").unwrap();
    let output = seedbox(&["prepare", "-o", "out.json"], cwd);
    assert_eq!(
        stderr(&output),
        "error: --output out.json: already exists\n"
    );

    assert!(seedbox(&["init", "demo"], cwd).status.success());
    fs::write(cwd.join("blocker"), "").unwrap();
    let output = seedbox(
        &[
            "prepare",
            "demo",
            "-o",
            "new.json",
            "--state",
            "blocker/state",
        ],
        cwd,
    );
    assert!(
        stderr(&output).starts_with("error: --state blocker/state: "),
        "{}",
        stderr(&output)
    );
}

#[test]
fn bad_values_are_usage_errors() {
    let directory = tempfile::tempdir().unwrap();
    for (args, expected) in [
        (&["run", "--margin", "0"][..], "positive branch count"),
        (&["run", "--undo", "999999999"], "not in 0..=262144"),
        (&["run", "--memory-mb", "1"], "not in 64..=65536"),
        (&["run", "--cpu", "5000"], "not in 0..1024"),
        (
            &["init", "x", "--template", "bogus"],
            "possible values: etcd, redis",
        ),
    ] {
        let output = seedbox(args, directory.path());
        assert_eq!(output.status.code(), Some(2), "{args:?}");
        assert!(
            stderr(&output).contains(expected),
            "{args:?}: {}",
            stderr(&output)
        );
    }
}

#[test]
fn build_epoch_is_bounded_and_only_available_for_preparation_commands() {
    let directory = tempfile::tempdir().unwrap();
    for value in ["-1", "4294967296", "not-a-number"] {
        let output = seedbox(
            &["prepare", "--output", "out.json", "--build-epoch", value],
            directory.path(),
        );
        assert_eq!(output.status.code(), Some(2));
        assert!(
            stderr(&output).contains("--build-epoch"),
            "{}",
            stderr(&output)
        );
        assert!(!directory.path().join("out.json").exists());
    }
    for command in ["prepare", "run", "explore"] {
        let output = seedbox(&[command, "--help"], directory.path());
        assert!(output.status.success());
        assert!(String::from_utf8_lossy(&output.stdout).contains("--build-epoch"));
    }
    for command in ["replay", "validate", "inspect", "init"] {
        let output = seedbox(&[command, "--help"], directory.path());
        assert!(output.status.success());
        assert!(!String::from_utf8_lossy(&output.stdout).contains("--build-epoch"));
    }
    for value in ["0", "4294967295"] {
        let output = seedbox(
            &["prepare", "--output", "out.json", "--build-epoch", value],
            directory.path(),
        );
        assert_eq!(output.status.code(), Some(2));
        assert!(stderr(&output).starts_with("error: no Compose file found"));
    }
}

/// A project plus empty stand-ins for every runtime input but BusyBox.
#[cfg(target_os = "linux")]
fn runtime_project() -> (tempfile::TempDir, Vec<String>) {
    let directory = tempfile::tempdir().unwrap();
    assert!(
        seedbox(&["init", "demo", "--template", "redis"], directory.path())
            .status
            .success()
    );
    let mut args = vec!["run".to_owned(), "demo".to_owned()];
    for flag in ["kernel", "vmm", "guest-init"] {
        let path = directory.path().join(flag);
        fs::write(&path, b"").unwrap();
        args.extend([format!("--{flag}"), path.to_str().unwrap().to_owned()]);
    }
    (directory, args)
}

#[cfg(target_os = "linux")]
#[test]
fn run_without_a_kernel_lists_where_it_looked() {
    let (directory, mut args) = runtime_project();
    let kernel = args.iter().position(|arg| arg == "--kernel").unwrap();
    args.drain(kernel..kernel + 2);
    let args: Vec<_> = args.iter().map(String::as_str).collect();
    let output = seedbox(&args, directory.path());
    assert_eq!(output.status.code(), Some(2));
    let home = directory.path().display();
    assert_eq!(
        stderr(&output),
        format!(
            "error: no guest kernel found; searched:\n  artifacts/bzImage\n  \
             {home}/seedbox-sim/artifacts/bzImage\nbuild one with `bash kernel/build.sh`, \
             or pass --kernel or set SEEDBOX_KERNEL\n"
        )
    );
    assert!(!directory.path().join(".seedbox/runs").exists());
}

#[cfg(target_os = "linux")]
#[test]
fn explicit_inputs_must_exist() {
    let (directory, mut args) = runtime_project();
    let vmm = args.iter().position(|arg| arg == "--vmm").unwrap();
    args[vmm + 1] = "/nonexistent/seedbox-vmm".into();
    let args: Vec<_> = args.iter().map(String::as_str).collect();
    let output = seedbox(&args, directory.path());
    assert_eq!(
        stderr(&output),
        "error: --vmm /nonexistent/seedbox-vmm: not found\n"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn unqualified_cpu_permission_is_checked_after_the_inputs() {
    // Whatever this host's CPU, input errors come first, with or without the flag.
    let (directory, mut args) = runtime_project();
    let bin = directory.path().join("bin");
    fs::create_dir(&bin).unwrap();
    for allow in [false, true] {
        if allow {
            args.push("--allow-unqualified-cpu".into());
        }
        let args: Vec<_> = args.iter().map(String::as_str).collect();
        let output = seedbox_with(&args, directory.path(), Some(&bin));
        assert_eq!(output.status.code(), Some(2));
        assert!(stderr(&output).starts_with("error: no static BusyBox found;"));
    }
}

#[cfg(target_os = "linux")]
#[test]
fn busybox_default_requires_a_static_busybox_on_path() {
    let (directory, args) = runtime_project();
    let args: Vec<_> = args.iter().map(String::as_str).collect();
    let bin = directory.path().join("bin");
    fs::create_dir(&bin).unwrap();

    let output = seedbox_with(&args, directory.path(), Some(&bin));
    assert_eq!(
        stderr(&output),
        "error: no static BusyBox found; searched:\n  $PATH/busybox\nenter `nix develop`, \
         or pass --busybox or set SEEDBOX_BUSYBOX\n"
    );

    // The test binary is dynamically linked on glibc hosts.
    if cfg!(target_env = "gnu") {
        let busybox = bin.join("busybox");
        fs::copy(std::env::current_exe().unwrap(), &busybox).unwrap();
        let output = seedbox_with(&args, directory.path(), Some(&bin));
        assert_eq!(
            stderr(&output),
            format!(
                "error: {} is dynamically linked; install a static BusyBox, or pass \
                 --busybox or set SEEDBOX_BUSYBOX\n",
                busybox.display()
            )
        );
    }
}

#[cfg(target_os = "linux")]
#[test]
fn an_explicit_mke2fs_must_exist() {
    let (directory, mut args) = runtime_project();
    let busybox = directory.path().join("busybox");
    fs::write(&busybox, b"").unwrap();
    args.extend(["--busybox".into(), busybox.to_str().unwrap().to_owned()]);
    args.extend(["--mke2fs".into(), "/nonexistent/mke2fs".into()]);
    let args: Vec<_> = args.iter().map(String::as_str).collect();
    let output = seedbox(&args, directory.path());
    assert_eq!(output.status.code(), Some(2));
    assert_eq!(
        stderr(&output),
        "error: --mke2fs /nonexistent/mke2fs: not found\n"
    );
}

#[test]
fn explore_bounds_and_help_are_public_before_runtime_inputs_or_state() {
    let directory = tempfile::tempdir().unwrap();
    assert!(seedbox(&["init", "demo"], directory.path())
        .status
        .success());
    let project = directory.path().join("demo");
    for count in ["0", "nope"] {
        let output = seedbox(&["explore", "--count", count], &project);
        assert_eq!(output.status.code(), Some(2));
        assert!(stderr(&output).contains("--count"));
        assert!(!project.join(".seedbox").exists());
    }
    let overflow = seedbox(
        &["explore", "--seed", "18446744073709551615", "--count", "2"],
        &project,
    );
    assert_eq!(stderr(&overflow),"error: --seed/--count: range is empty or overflows u64; use a positive --count and reduce the range\n");
    assert!(!project.join(".seedbox").exists());
    for workers in ["0", "33", "nope"] {
        let output = seedbox(&["explore", "--workers", workers], &project);
        assert_eq!(output.status.code(), Some(2));
        assert!(stderr(&output).contains("--workers"));
        assert!(!project.join(".seedbox").exists());
    }
    let pinned = seedbox(&["explore", "--workers", "2", "--cpu", "4"], &project);
    assert_eq!(pinned.status.code(), Some(2));
    assert_eq!(stderr(&pinned), "error: --workers: parallel exploration requires automatic core selection; omit --cpu and SEEDBOX_CPU\n");
    assert!(!project.join(".seedbox").exists());
    let env_pinned = Command::new(env!("CARGO_BIN_EXE_seedbox"))
        .args(["explore", "--workers", "2"])
        .current_dir(&project)
        .env_clear()
        .env("HOME", &project)
        .env("PATH", "")
        .env("SEEDBOX_CPU", "4")
        .output()
        .unwrap();
    assert_eq!(env_pinned.status.code(), Some(2));
    assert_eq!(stderr(&env_pinned), stderr(&pinned));
    assert!(!project.join(".seedbox").exists());
    let boundary = seedbox(
        &["explore", "--seed", "18446744073709551615", "--count", "1"],
        &project,
    );
    assert_eq!(boundary.status.code(), Some(2));
    assert!(!stderr(&boundary).contains("overflows"));
    let help = seedbox(&["help", "explore"], &project);
    assert!(help.status.success());
    let text = String::from_utf8_lossy(&help.stdout);
    assert!(
        text.contains("--count")
            && text.contains("[default: 1]")
            && text.contains("--json")
            && text.contains("Exit status:")
    );
    assert!(text.contains("--workers"));
    fs::write(project.join("compose.yaml"),"services:\n  a: {image: example/a}\nx-seedbox:\n  version: 1\n  limits: {simulated_time: 1s, wall_time: 1m}\n  check: {service: a}\n").unwrap();
    let missing = seedbox(&["explore"], &project);
    assert_eq!(
        stderr(&missing),
        "error: --seed: no Compose seed; provide --seed FIRST\n"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn explore_resolves_tools_before_host_or_preparation() {
    let (directory, mut args) = runtime_project();
    args[0] = "explore".into();
    let busybox = directory.path().join("busybox");
    let mut elf = [0u8; 64];
    elf[..6].copy_from_slice(b"\x7fELF\x02\x01");
    fs::write(&busybox, elf).unwrap();
    args.extend([
        "--busybox".into(),
        busybox.to_str().unwrap().into(),
        "--json".into(),
    ]);
    let refs: Vec<_> = args.iter().map(String::as_str).collect();
    let missing = seedbox(&refs, directory.path());
    assert_eq!(
        stderr(&missing),
        "error: skopeo not found; install it or enter `nix develop`\n"
    );
    assert!(missing.stdout.is_empty());
    assert!(!directory.path().join(".seedbox/runs").exists());
    let bin = directory.path().join("bin");
    fs::create_dir(&bin).unwrap();
    fs::write(bin.join("skopeo"), b"must not execute").unwrap();
    let missing = seedbox_with(&refs, directory.path(), Some(&bin));
    assert_eq!(
        stderr(&missing),
        "error: umoci not found; install it or enter `nix develop`\n"
    );
    fs::write(bin.join("umoci"), b"must not execute").unwrap();
    let missing = seedbox_with(&refs, directory.path(), Some(&bin));
    assert_eq!(
        stderr(&missing),
        "error: podman not found; install it or enter `nix develop`\n"
    );
}

#[test]
fn bundle_cli_validates_paths_limits_and_positional_inputs() {
    let directory = tempfile::tempdir().unwrap();
    for command in ["export", "import"] {
        let output = seedbox(&[command], directory.path());
        assert_eq!(output.status.code(), Some(2));
        assert!(stderr(&output).contains("required arguments"));
        let output = seedbox(&[command, "missing", "--max-bytes", "0"], directory.path());
        assert_eq!(output.status.code(), Some(2));
        assert!(stderr(&output).contains("--max-bytes"));
        let output = seedbox(
            &[command, "missing", "--max-bytes", "not-a-number"],
            directory.path(),
        );
        assert_eq!(output.status.code(), Some(2));
        assert!(stderr(&output).contains("--max-bytes"));
    }
    let output = seedbox(&["export", "missing"], directory.path());
    assert_eq!(stderr(&output), "error: run directory missing: not found\n");
    let output = seedbox(&["import", "missing"], directory.path());
    assert_eq!(stderr(&output), "error: bundle missing: not found\n");
}

#[test]
fn build_epoch_custom_builder_fails_before_runtime_resolution_or_state_creation() {
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("Dockerfile"), "FROM scratch\n").unwrap();
    fs::write(root.path().join("compose.yaml"), "name: epoch\nservices:\n  app: {build: .}\nx-seedbox:\n  version: 1\n  seed: 1\n  limits: {simulated_time: 1s, wall_time: 1m}\n  check: {service: app}\n").unwrap();
    fs::write(
        root.path().join("builder"),
        "custom builder must not execute",
    )
    .unwrap();
    for command in ["run", "explore", "prepare"] {
        let mut args = vec![command, "--build-epoch", "0", "--builder", "./builder"];
        if command == "prepare" {
            args.extend(["--output", "prepared.json"]);
        }
        let output = seedbox(&args, root.path());
        assert_eq!(output.status.code(), Some(2));
        assert_eq!(stderr(&output), "error: --build-epoch: builder ./builder is unsupported; use the qualified project-Nix local Podman or omit --build-epoch\n");
        assert!(output.stdout.is_empty());
        assert!(!root.path().join(".seedbox").exists());
        assert!(!root.path().join("prepared.json").exists());
    }
}

#[test]
fn old_host_proof_replay_is_not_comparable_while_inspection_and_transfer_stay_available() {
    use seedbox::{
        config,
        execution::*,
        image::Launch,
        orchestration::*,
        store::{ContentDigest, ContentStore},
    };
    use std::collections::BTreeMap;
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    fs::write(root.join("compose.yaml"), "name: old\nservices:\n  workload: {image: fixture:v1}\nx-seedbox:\n  version: 1\n  seed: 0\n  limits: {simulated_time: 1s, wall_time: 1m}\n  check: {service: workload}\n").unwrap();
    let simulation = config::load(&root.join("compose.yaml"), &BTreeMap::new()).unwrap();
    let store = ContentStore::new(root.join("state/objects"));
    let object = store.put(b"opaque retained fixture").unwrap();
    let inputs = PreparedInputs {
        kernel: object.clone(),
        profile: ExecutionProfile {
            backend: "seedbox-kvm".into(),
            engine: object.clone(),
            witness_mode: WitnessMode::Exits,
            semantic_parameters: BTreeMap::from([("memory_mb".into(), "512".into())]),
        },
        placement: BTreeMap::from([("workload".into(), "workload".into())]),
        guests: BTreeMap::from([("workload".into(), object.clone())]),
        images: BTreeMap::from([(
            "workload".into(),
            PreparedImage {
                manifest: object.clone(),
                config: object.clone(),
                layers: vec![],
                launch: Launch {
                    argv: vec!["/app".into()],
                    environment: BTreeMap::new(),
                    user: "".into(),
                    working_dir: "/".into(),
                    volumes: vec![],
                    disk_volumes: vec![],
                },
            },
        )]),
        auxiliary: BTreeMap::new(),
        provenance: BTreeMap::new(),
    };
    let identity = RunIdentity {
        version: 2,
        seed: 0,
        configuration: simulation,
        inputs,
    };
    let run_id = store.put(&serde_json::to_vec(&identity).unwrap()).unwrap();
    let record = RunRecord {
        run_id,
        attempt: "old".into(),
        identity,
        performance_parameters: BTreeMap::new(),
        allow_engine_change: false,
        log_policy: LogPolicy::default(),
        omitted_logs: BTreeMap::new(),
        outcome: Outcome::Pass,
        report: Some(ExecutionReport {
            reason: StopReason::Completed,
            workload_exit: Some(WorkloadExit {
                service: "workload".into(),
                code: 0,
            }),
            witness: Some(ContentDigest::of(b"fixture witness")),
            full_state: None,
            faults: vec![],
            detail: None,
            engine_qualification: None,
        }),
        error: None,
        logs: BTreeMap::new(),
        replay_of: None,
        replay: None,
    };
    fs::create_dir_all(root.join("state/runs/old")).unwrap();
    let manifest = serde_json::to_vec(&serde_json::json!({"sha256": ContentDigest::of(&serde_json::to_vec(&record).unwrap()), "record": record})).unwrap();
    fs::write(root.join("state/runs/old/manifest.json"), &manifest).unwrap();
    for override_engine in [false, true] {
        let mut args = vec!["replay", "state/runs/old", "--state", "state"];
        if override_engine {
            args.push("--allow-engine-change");
        }
        let output = seedbox(&args, root);
        assert_eq!(output.status.code(), Some(2), "{}", stderr(&output));
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(stdout.starts_with("ERROR: "), "{stdout}");
        assert!(stdout.contains("replay: NOT_COMPARABLE"), "{stdout}");
        assert_eq!(stderr(&output), "input integrity: host admission proof missing: host_policy; use the retained previous CLI only for historical evidence\n");
        assert_eq!(
            fs::read(root.join("state/runs/old/manifest.json")).unwrap(),
            manifest
        );
    }
    assert!(seedbox(&["inspect", "state/runs/old", "--json"], root)
        .status
        .success());
    let exported = seedbox(
        &[
            "export",
            "state/runs/old",
            "--state",
            "state",
            "--output",
            "old.tar",
        ],
        root,
    );
    assert!(exported.status.success(), "{}", stderr(&exported));
    let imported = seedbox(&["import", "old.tar", "--state", "imported"], root);
    assert!(imported.status.success(), "{}", stderr(&imported));
    assert_eq!(
        fs::read(root.join("state/runs/old/manifest.json")).unwrap(),
        manifest
    );
}

#[test]
fn doctor_help_ranges_and_missing_inputs_follow_cli_contract() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let help = seedbox(&["help", "doctor"], root);
    assert!(help.status.success());
    let text = String::from_utf8(help.stdout).unwrap();
    assert!(text.contains("Linux KVM host, root"));
    for flag in ["--cpu", "--kernel", "--vmm", "--json"] {
        assert!(text.contains(flag), "{text}");
    }
    for flag in [
        "--state",
        "--builder",
        "--undo",
        "--memory-mb",
        "--guest-init",
    ] {
        assert!(!text.contains(flag), "{text}");
    }
    assert!(text.contains("Doctor never builds"));
    let invalid = seedbox(&["doctor", "--cpu", "1024"], root);
    assert_eq!(invalid.status.code(), Some(2));
    assert!(stderr(&invalid).contains("--cpu"));
    for json in [false, true] {
        let mut args = vec!["doctor", "--vmm", "missing"];
        if json {
            args.push("--json");
        }
        let output = seedbox(&args, root);
        assert_eq!(output.status.code(), Some(2));
        assert_eq!(stderr(&output), "error: --vmm missing: not found\n");
        assert!(output.stdout.is_empty());
    }
    fs::write(root.join("engine"), b"unsupported engine").unwrap();
    let missing_kernel = seedbox(&["doctor", "--vmm", "engine", "--kernel", "missing"], root);
    assert_eq!(
        stderr(&missing_kernel),
        "error: --kernel missing: not found\n"
    );
    assert!(!root.join(".seedbox").exists());
}

#[test]
fn doctor_bad_inputs_execute_no_tools_and_create_no_state_even_with_cargo_environment() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let tools = root.join("tools");
    fs::create_dir(&tools).unwrap();
    let marker = root.join("tool-called");
    for name in ["cargo", "podman", "skopeo", "umoci"] {
        let tool = tools.join(name);
        fs::write(
            &tool,
            format!(
                "#!/bin/sh\necho invoked >> '{}'\nexit 0\n",
                marker.display()
            ),
        )
        .unwrap();
        fs::set_permissions(&tool, fs::Permissions::from_mode(0o755)).unwrap();
    }
    fs::write(root.join("kernel"), b"unqualified fixture kernel").unwrap();
    for json in [false, true] {
        let mut command = Command::new(env!("CARGO_BIN_EXE_seedbox"));
        command.args(["doctor", "--kernel", "kernel"]);
        if json {
            command.arg("--json");
        }
        let output = command
            .current_dir(root)
            .env_clear()
            .env("HOME", root)
            .env("PATH", &tools)
            .env("CARGO_MANIFEST_DIR", env!("CARGO_MANIFEST_DIR"))
            .env("CARGO", tools.join("cargo"))
            .env("SEEDBOX_STATE", root.join("unwanted-state"))
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(2), "{}", stderr(&output));
        assert!(output.stdout.is_empty());
        assert!(
            !marker.exists(),
            "doctor invoked a runtime/build/preparation program"
        );
        assert!(!root.join("unwanted-state").exists());
        assert!(!root.join(".seedbox").exists());
        assert!(!root.join("output").exists());
    }
    fs::write(root.join("engine"), b"unsupported engine").unwrap();
    let output = seedbox(&["doctor", "--vmm", "engine", "--kernel", "kernel"], root);
    assert_eq!(stderr(&output),"error: input integrity: engine is outside the admitted host policy; no engine-change override bypass is available\n");
    assert!(!marker.exists());
    assert!(output.stdout.is_empty());
}

#[test]
fn dev_engine_opt_in_is_available_only_on_execution_commands() {
    let dir = tempfile::tempdir().unwrap();
    for command in ["run", "explore", "replay", "doctor"] {
        let help = seedbox(&["help", command], dir.path());
        assert!(help.status.success());
        assert!(String::from_utf8_lossy(&help.stdout).contains("--allow-dev-engine"));
        let output = seedbox(
            &[command, "--allow-dev-engine", "--cpu", "1024"],
            dir.path(),
        );
        assert_eq!(output.status.code(), Some(2));
        assert!(!stderr(&output).contains("unexpected argument '--allow-dev-engine'"));
    }
    for command in ["prepare", "inspect", "export", "import"] {
        let help = seedbox(&["help", command], dir.path());
        assert!(!String::from_utf8_lossy(&help.stdout).contains("--allow-dev-engine"));
    }
    let output = seedbox(
        &["doctor", "--allow-dev-engine", "--vmm", "missing"],
        dir.path(),
    );
    assert_eq!(stderr(&output), "error: --vmm missing: not found\n");
    assert!(output.stdout.is_empty());
    assert!(!dir.path().join(".seedbox").exists());
}

#[cfg(not(target_os = "linux"))]
#[test]
fn doctor_dev_opt_in_keeps_host_refusal_and_creates_no_state() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("engine"), b"dev engine").unwrap();
    fs::write(dir.path().join("kernel"), b"dev kernel").unwrap();
    for json in [false, true] {
        let mut args = vec![
            "doctor",
            "--allow-dev-engine",
            "--vmm",
            "engine",
            "--kernel",
            "kernel",
        ];
        if json {
            args.push("--json");
        }
        let output = seedbox(&args, dir.path());
        assert_eq!(output.status.code(), Some(2));
        assert_eq!(
            stderr(&output),
            "error: run and replay require the qualified Linux KVM host\n"
        );
        assert!(output.stdout.is_empty());
        assert!(!dir.path().join(".seedbox").exists());
    }
}

mod diff_tests {
    use super::*;
    use seedbox as sb;
    include!("support/diff_fixture.rs");

    #[test]
    fn diff_plain_and_json_status_and_trace_details() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write_fixture(
            &root.join("a"),
            &mut fixture(),
            Some(&raw_report()),
            Some(b"I 1 2 3 4\nH 5 6 7 8\n"),
        );
        write_fixture(
            &root.join("b"),
            &mut fixture(),
            Some(&raw_report()),
            Some(b"I 1 2 3 4\nP 5 9 7 8\n"),
        );
        let output = seedbox(&["diff", "a", "a"], root);
        assert_eq!(output.status.code(), Some(0));
        assert!(stderr(&output).is_empty());
        let plain = String::from_utf8(output.stdout).unwrap();
        assert!(plain.starts_with("MATCH: a <> a\nworld witness: "));
        assert_eq!(
            plain
                .lines()
                .filter(|line| line.chars().next().unwrap().is_ascii_uppercase())
                .count(),
            1
        );
        let output = seedbox(&["diff", "a", "b", "--json"], root);
        assert_eq!(output.status.code(), Some(1));
        assert!(stderr(&output).is_empty());
        let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(value["status"], "DIFFERENT");
        assert_eq!(value["first_record"]["machine"], "workload");
        assert_eq!(value["first_record"]["record"], 2);
        assert_eq!(value["first_record"]["a"]["kind"], "H");
        assert_eq!(value["first_record"]["b"]["time_ns"], 9);
        assert!(!root.join(".seedbox").exists());
        let output = seedbox(&["diff", "a", "b"], root);
        assert_eq!(output.status.code(), Some(1));
        assert!(output.stdout.starts_with(b"DIFFERENT: a <> b\n"));
    }

    #[test]
    fn diff_arguments_and_help() {
        let dir = tempfile::tempdir().unwrap();
        let output = seedbox(&["diff", "a"], dir.path());
        assert_eq!(output.status.code(), Some(2));
        assert!(stderr(&output).contains("<RUN_B>"));
        let output = seedbox(&["help", "diff"], dir.path());
        assert!(output.status.success());
        let help = String::from_utf8(output.stdout).unwrap();
        assert!(help.contains("<RUN_A> <RUN_B>"));
        assert!(help.contains("--json"));
        assert!(help.contains("diff DIFFERENT"));
    }

    #[test]
    fn diff_rejects_corrupt_and_missing_declared_evidence_with_context() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write_fixture(&root.join("a"), &mut fixture(), Some(&raw_report()), None);
        fs::write(root.join("a/output/vmm.json"), "corrupt").unwrap();
        let output = seedbox(&["diff", "a", "a", "--json"], root);
        assert_eq!(output.status.code(), Some(2));
        assert!(output.stdout.is_empty());
        assert_eq!(stderr(&output), "error: diff a/output/vmm.json: evidence checksum mismatch; use an intact retained run directory (import bundles first)\n");
        fs::remove_file(root.join("a/output/vmm.json")).unwrap();
        let cause = fs::symlink_metadata(root.join("a/output/vmm.json")).unwrap_err();
        let output = seedbox(&["diff", "a", "a"], root);
        assert_eq!(output.status.code(), Some(2));
        assert_eq!(stderr(&output), format!("error: diff a/output/vmm.json: {cause}; use an intact retained run directory (import bundles first)\n"));
    }

    #[test]
    fn diff_rejects_missing_or_corrupt_manifest() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let cause = fs::read(root.join("missing/manifest.json")).unwrap_err();
        let output = seedbox(&["diff", "missing", "missing"], root);
        assert_eq!(output.status.code(), Some(2));
        assert_eq!(stderr(&output), format!("error: diff missing/manifest.json: {cause}; use an intact retained run directory (import bundles first)\n"));
        write_fixture(&root.join("a"), &mut fixture(), None, None);
        let path = root.join("a/manifest.json");
        let mut manifest: serde_json::Value =
            serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        manifest["record"]["identity"]["seed"] = 99.into();
        fs::write(path, serde_json::to_vec(&manifest).unwrap()).unwrap();
        let output = seedbox(&["diff", "a", "a"], root);
        assert_eq!(output.status.code(), Some(2));
        assert_eq!(stderr(&output), "error: diff a/manifest.json: input integrity: manifest checksum mismatch; use an intact retained run directory (import bundles first)\n");
    }

    #[test]
    fn diff_retained_metadata_changes_and_missing_trace_are_visible() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let mut b = fixture();
        b.identity.seed = 77;
        write_fixture(&root.join("a"), &mut fixture(), None, Some(b"I 1 2 3 4\n"));
        write_fixture(&root.join("b"), &mut b, None, None);
        let output = seedbox(&["diff", "a", "b", "--json"], root);
        assert_eq!(output.status.code(), Some(1));
        let result: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(result["identity_differences"][0]["field"], "/seed");
        assert_eq!(
            result["evidence_differences"][0]["b"],
            serde_json::Value::Null
        );
        assert!(result["first_record"].is_null());
    }
    #[test]
    fn diff_rejects_invalid_retained_reports_and_traces() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let mut wrong_version = raw_report();
        wrong_version["report_version"] = 2.into();
        let mut duplicate = raw_report();
        let machine = duplicate["machines"][0].clone();
        duplicate["machines"].as_array_mut().unwrap().push(machine);
        let mut wrong_world = raw_report();
        wrong_world["world_witness"] =
            serde_json::to_value(sb::store::ContentDigest::of(b"wrong")).unwrap();
        for (raw, message) in [
            (wrong_version, "unsupported VMM report version"),
            (duplicate, "duplicate machine name"),
            (wrong_world, "world witness disagrees with manifest"),
        ] {
            write_fixture(&root.join("a"), &mut fixture(), Some(&raw), None);
            let output = seedbox(&["diff", "a", "a"], root);
            assert_eq!(output.status.code(), Some(2));
            assert_eq!(stderr(&output), format!("error: diff a/output/vmm.json: {message}; use an intact retained run directory (import bundles first)\n"));
        }
        let mut record = fixture();
        let path = root.join("a/output/vmm.json");
        fs::write(&path, b"bad").unwrap();
        record
            .logs
            .insert("vmm.json".into(), sb::store::ContentDigest::of(b"bad"));
        write_fixture(&root.join("a"), &mut record, None, None);
        let output = seedbox(&["diff", "a", "a"], root);
        assert_eq!(output.status.code(), Some(2));
        assert_eq!(stderr(&output), "error: diff a/output/vmm.json: expected value at line 1 column 1; use an intact retained run directory (import bundles first)\n");
        write_fixture(&root.join("a"), &mut fixture(), None, Some(b"bad\n"));
        let output = seedbox(&["diff", "a", "a"], root);
        assert_eq!(output.status.code(), Some(2));
        assert_eq!(stderr(&output), "error: diff a: machines/workload.trace compared with a: input integrity: invalid trace record: expected kind branches time_ns extra(hex) rip(hex); use an intact retained run directory (import bundles first)\n");
    }

    #[cfg(unix)]
    #[test]
    fn diff_rejects_symlinked_evidence_and_unsafe_names() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write_fixture(&root.join("a"), &mut fixture(), Some(&raw_report()), None);
        let path = root.join("a/output/vmm.json");
        fs::rename(&path, root.join("raw.json")).unwrap();
        std::os::unix::fs::symlink(root.join("raw.json"), &path).unwrap();
        let output = seedbox(&["diff", "a", "a"], root);
        assert_eq!(output.status.code(), Some(2));
        assert_eq!(stderr(&output), "error: diff a/output/vmm.json: evidence must be a regular file under real directories; use an intact retained run directory (import bundles first)\n");
        let mut record = fixture();
        record.identity.inputs.guests.insert(
            "../../escape".into(),
            sb::store::ContentDigest::of(b"guest"),
        );
        record.logs.insert(
            "machines/../../escape.trace".into(),
            sb::store::ContentDigest::of(b"trace"),
        );
        write_fixture(&root.join("b"), &mut record, None, None);
        let output = seedbox(&["diff", "b", "b"], root);
        assert_eq!(output.status.code(), Some(2));
        assert_eq!(stderr(&output), "error: diff b: unsafe evidence path; use an intact retained run directory (import bundles first)\n");
    }

    #[test]
    fn diff_compares_imported_bundle_without_original_store() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let store = sb::store::ContentStore::new(root.join("objects"));
        store.put(b"fixture").unwrap();
        let mut record = fixture();
        write_fixture(
            &root.join("original"),
            &mut record,
            Some(&raw_report()),
            Some(b"I 1 2 3 4\n"),
        );
        store
            .put(&serde_json::to_vec(&record.identity).unwrap())
            .unwrap();
        for name in record.logs.keys() {
            store
                .put_file(&root.join("original/output").join(name))
                .unwrap();
        }
        let options = sb::bundle::Options::default();
        sb::bundle::export(
            &root.join("original"),
            &store,
            &root.join("run.tar"),
            &options,
        )
        .unwrap();
        let imported =
            sb::bundle::import(&root.join("run.tar"), &root.join("imported"), &options).unwrap();
        fs::remove_dir_all(root.join("objects")).unwrap();
        let output = seedbox(
            &[
                "diff",
                "original",
                imported.directory.to_str().unwrap(),
                "--json",
            ],
            root,
        );
        assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
        let result: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(result["status"], "MATCH");
        assert_eq!(result["machines"][0]["witness"]["matches"], true);
    }
}

#[test]
fn torn_writes_is_opt_in_presence_only_and_replay_uses_the_recorded_policy() {
    let directory = tempfile::tempdir().unwrap();
    for command in ["run", "prepare"] {
        let help = seedbox(&["help", command], directory.path());
        assert!(help.status.success());
        let help = String::from_utf8_lossy(&help.stdout);
        assert!(help.contains("--torn-writes"));
        assert!(help.contains("off by default"));
        assert!(!help.contains("SEEDBOX_TORN_WRITES"));
        let mut args = vec![command, "--torn-writes"];
        if command == "prepare" {
            args.extend(["--output", "out.json"]);
        }
        let output = seedbox(&args, directory.path());
        assert_eq!(output.status.code(), Some(2));
        assert_eq!(stderr(&output), "error: no Compose file found; searched:\n  ./compose.yaml\n  ./compose.yml\n  ./docker-compose.yaml\n  ./docker-compose.yml\npass the file path directly\n");
        let output = seedbox(&[command, "--torn-writes=false"], directory.path());
        assert_eq!(output.status.code(), Some(2));
        assert!(stderr(&output).contains("--torn-writes"));
    }
    let output = seedbox(&["replay", "run", "--torn-writes"], directory.path());
    assert_eq!(output.status.code(), Some(2));
    assert!(stderr(&output).contains("unexpected argument '--torn-writes'"));
}
