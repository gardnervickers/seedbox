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
        .env("SEEDBOX_ASSET_DIR", cwd.join("assets"))
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

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
#[test]
fn mac_rejects_linux_cpu_options_before_preparation_or_state_creation() {
    let directory = tempfile::tempdir().unwrap();
    fs::write(directory.path().join("compose.yaml"), "name: mac-options\nservices:\n  app: {image: example/app}\nx-seedbox:\n  version: 1\n  seed: 1\n  limits: {simulated_time: 1s, wall_time: 1m}\n  check: {service: app}\n").unwrap();
    for command in ["run", "explore", "doctor"] {
        let args = [command, "--cpu", "1"];
        let output = seedbox(&args, directory.path());
        assert_eq!(output.status.code(), Some(2), "{args:?}");
        assert!(output.stdout.is_empty(), "{args:?}");
        assert_eq!(
            stderr(&output),
            "error: --cpu requires Linux KVM; omit it on Mac\n",
            "{args:?}"
        );
        assert!(!directory.path().join(".seedbox").exists(), "{args:?}");
    }
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
#[test]
fn mac_explore_rejects_multiple_workers_before_state_or_runtime_setup() {
    let directory = tempfile::tempdir().unwrap();
    assert!(seedbox(&["init", "demo"], directory.path())
        .status
        .success());
    let project = directory.path().join("demo");

    let help = seedbox(&["help", "explore"], &project);
    assert!(help.status.success());
    assert!(String::from_utf8_lossy(&help.stdout).contains(
        "Maximum active attempts; Mac supports only 1. Omit --cpu for Linux parallel execution"
    ));
    assert!(String::from_utf8_lossy(&help.stdout).contains("[default: 1]"));

    let output = seedbox_with(&["explore", "--workers", "2"], &project, None);
    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    assert_eq!(
        stderr(&output),
        "error: --workers: Mac exploration supports one worker; use --workers 1 or omit it\n"
    );
    assert!(!project.join(".seedbox").exists());
}

#[test]
fn version_and_help_describe_the_builtin_engine_without_exposing_worker_dispatch() {
    let directory = tempfile::tempdir().unwrap();

    let version = seedbox(&["--version"], directory.path());
    assert!(version.status.success());
    assert_eq!(
        String::from_utf8_lossy(&version.stdout),
        format!("seedbox {}\n", seedbox::engine::SIMULATOR_VERSION)
    );
    assert!(version.stderr.is_empty());

    let root_help = seedbox(&["--help"], directory.path());
    assert!(root_help.status.success());
    let help = String::from_utf8_lossy(&root_help.stdout);
    assert!(!help.contains("__engine"), "{help}");
    assert!(!help.contains("seedbox-internal-worker-v1"), "{help}");
    assert!(!help.contains("arm64-fixture-v1"), "{help}");

    let run_help = seedbox(&["help", "run"], directory.path());
    assert!(run_help.status.success());
    let help = String::from_utf8_lossy(&run_help.stdout);
    assert!(help.contains("--vmm"), "{help}");
    assert!(help.contains("legacy standalone VMM"), "{help}");
    assert!(help.contains("engine built into seedbox"), "{help}");
}

#[test]
fn internal_dispatch_rejects_missing_and_unknown_contracts_before_cli_state_setup() {
    let directory = tempfile::tempdir().unwrap();
    for args in [&["__engine"][..], &["__engine", "unknown-contract"]] {
        let output = seedbox(args, directory.path());
        assert_eq!(
            output.status.code(),
            Some(2),
            "{args:?}: {}",
            stderr(&output)
        );
        assert!(output.stdout.is_empty(), "{args:?}");
        assert!(
            stderr(&output).contains("internal engine contract"),
            "{args:?}: {}",
            stderr(&output)
        );
        assert!(!directory.path().join(".seedbox").exists(), "{args:?}");
    }
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
#[test]
fn internal_worker_requires_config_without_state_or_external_tools() {
    use std::os::unix::fs::PermissionsExt;

    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    let tools = root.join("tools");
    fs::create_dir(&tools).unwrap();
    let marker = root.join("tool-called");
    let cargo = tools.join("cargo");
    fs::write(
        &cargo,
        format!(
            "#!/bin/sh\necho invoked >> '{}'\nexit 0\n",
            marker.display()
        ),
    )
    .unwrap();
    fs::set_permissions(&cargo, fs::Permissions::from_mode(0o755)).unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_seedbox"))
        .args(["__engine", "seedbox-internal-worker-v1"])
        .current_dir(root)
        .env_clear()
        .env("HOME", root)
        .env("SEEDBOX_ASSET_DIR", root)
        .env("PATH", &tools)
        .env("CARGO", &cargo)
        .env("CARGO_MANIFEST_DIR", env!("CARGO_MANIFEST_DIR"))
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    assert!(output.stdout.is_empty());
    assert!(
        stderr(&output).contains("--config required"),
        "{}",
        stderr(&output)
    );
    assert!(!marker.exists(), "worker invoked external cargo");
    assert!(!root.join(".seedbox").exists());
}

#[cfg(all(target_arch = "aarch64", any(target_os = "linux", target_os = "macos")))]
#[test]
fn arm_fixture_contract_uses_the_shared_fixture_argument_parser() {
    let directory = tempfile::tempdir().unwrap();
    let output = seedbox(
        &[
            "__engine",
            "arm64-fixture-v1",
            "run",
            "--kernel",
            "kernel",
            "--initrd",
            "initrd",
            "--seed",
            "not-a-seed",
        ],
        directory.path(),
    );
    assert_eq!(output.status.code(), Some(2), "{}", stderr(&output));
    assert!(output.stdout.is_empty());
    assert!(stderr(&output).contains("--seed"), "{}", stderr(&output));
    assert!(!stderr(&output).contains("requires Linux arm64"));
    assert!(!directory.path().join(".seedbox").exists());
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
    let kernel = directory
        .path()
        .join("assets")
        .join(std::env::consts::ARCH)
        .join("kernel");
    assert_eq!(stderr(&output), format!("error: --kernel {}: not found; from the Seedbox checkout run `bash tools/build-assets.sh`, or pass --kernel PATH\n", kernel.display()));
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
fn missing_helpers_explain_the_asset_build_without_searching_path() {
    let (directory, args) = runtime_project();
    let args: Vec<_> = args.iter().map(String::as_str).collect();
    let output = seedbox(&args, directory.path());
    let archive = directory
        .path()
        .join("assets")
        .join(std::env::consts::ARCH)
        .join("runtime.tar.gz");
    assert_eq!(stderr(&output), format!("error: --runtime-assets {}: not found; from the Seedbox checkout run `bash tools/build-assets.sh`, or pass --runtime-assets PATH\n", archive.display()));
}

#[cfg(target_os = "linux")]
#[test]
fn an_explicit_mke2fs_must_exist() {
    let (directory, mut args) = runtime_project();
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
fn explore_resolves_assets_before_host_or_preparation() {
    let (directory, mut args) = runtime_project();
    args[0] = "explore".into();
    args.push("--json".into());
    let args: Vec<_> = args.iter().map(String::as_str).collect();
    let output = seedbox(&args, directory.path());
    let archive = directory
        .path()
        .join("assets")
        .join(std::env::consts::ARCH)
        .join("runtime.tar.gz");
    assert_eq!(stderr(&output), format!("error: --runtime-assets {}: not found; from the Seedbox checkout run `bash tools/build-assets.sh`, or pass --runtime-assets PATH\n", archive.display()));
    assert!(output.stdout.is_empty());
    assert!(!directory.path().join(".seedbox/runs").exists());
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
fn validate_cli_resolves_directed_baselines_and_reports_each_policy_error() {
    let directory = tempfile::tempdir().unwrap();
    let source = directory.path().join("compose.yaml");
    let yaml = "name: cli-links\nservices:\n  server: {image: example/server:v1}\n  client: {image: example/client:v1}\nx-seedbox:\n  version: 1\n  limits: {simulated_time: 1s, wall_time: 1m}\n  network: {latency: 1ms, links: [LINKS]}\n  check: {service: client}\n";
    let good = "{from: server, to: client, latency: 1ms}";
    fs::write(&source, yaml.replace("LINKS", good)).unwrap();
    let output = seedbox(&["validate", "--json"], directory.path());
    assert!(output.status.success(), "{}", stderr(&output));
    let resolved: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        resolved["network"]["links"][0],
        serde_json::json!({"from":"server","to":"client","latency_ns":1_000_000,"jitter_ns":0,"loss_ppm":0})
    );
    assert!(resolved["network"].get("bandwidth_bps").is_none());
    let finite = yaml
        .replace(
            "LINKS",
            "{from: server, to: client, latency: 1ms, bandwidth_bps: 1000000, queue_bound: 12ms}",
        )
        .replace(
            "latency: 1ms, links:",
            "latency: 1ms, bandwidth_bps: 10000000, queue_bound: 0ns, links:",
        );
    fs::write(&source, finite).unwrap();
    let output = seedbox(&["validate", "--json"], directory.path());
    assert!(output.status.success(), "{}", stderr(&output));
    let finite: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(finite["network"]["bandwidth_bps"], 10_000_000);
    assert_eq!(finite["network"]["queue_bound_ns"], 0);
    assert_eq!(finite["network"]["links"][0]["bandwidth_bps"], 1_000_000);
    assert_eq!(finite["network"]["links"][0]["queue_bound_ns"], 12_000_000);
    for (entry, path) in [
        (format!("{good},{good}"), "links[1]"),
        (
            good.replace("from: server", "from: unknown"),
            "links[0].from",
        ),
        (good.replace("1ms", "999us"), "links[0].latency"),
        (good.replace("1ms}", "1ms, jitter: 1ms}"), "links[0].jitter"),
        (
            good.replace("1ms}", "1ms, loss_ppm: 1000001}"),
            "links[0].loss_ppm",
        ),
        (
            good.replace("1ms}", "1ms, bandwidth_bps: 0}"),
            "links[0].bandwidth_bps",
        ),
        (
            good.replace("1ms}", "1ms, bandwidth_bps: 1.5}"),
            "links[0].bandwidth_bps",
        ),
        (
            good.replace("1ms}", "1ms, queue_bound: 12ms}"),
            "links[0].queue_bound",
        ),
        (
            good.replace("1ms}", "1ms, bandwidth_bps: 1000000, queue_bound: -1ms}"),
            "links[0].queue_bound",
        ),
    ] {
        fs::write(&source, yaml.replace("LINKS", &entry)).unwrap();
        let output = seedbox(&["validate"], directory.path());
        assert_eq!(output.status.code(), Some(2));
        assert!(output.stdout.is_empty());
        assert!(
            stderr(&output).contains(&format!("$.x-seedbox.network.{path}")),
            "{}",
            stderr(&output)
        );
    }
    for (options, path) in [
        ("bandwidth_bps: 0", "bandwidth_bps"),
        ("bandwidth_bps: true", "bandwidth_bps"),
        ("queue_bound: 1ms", "queue_bound"),
    ] {
        fs::write(
            &source,
            yaml.replace("LINKS", good).replace(
                "latency: 1ms, links:",
                &format!("latency: 1ms, {options}, links:"),
            ),
        )
        .unwrap();
        let output = seedbox(&["validate"], directory.path());
        assert_eq!(output.status.code(), Some(2));
        assert!(
            stderr(&output).contains(&format!("$.x-seedbox.network.{path}")),
            "{}",
            stderr(&output)
        );
    }
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
#[test]
fn old_records_stay_inspectable_and_transferable() {
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
        wall_clock_epoch: None,
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
            timebase: None,
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
    assert!(text.contains("Linux KVM host"));
    for flag in ["--cpu", "--kernel", "--vmm", "--timebase", "--json"] {
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
    assert!(text.contains("Doctor never builds or downloads"));
    let invalid_timebase = seedbox(&["doctor", "--timebase", "invalid"], root);
    assert_eq!(invalid_timebase.status.code(), Some(2));
    assert!(stderr(&invalid_timebase).contains("--timebase"));
    let pv_missing = seedbox(&["doctor", "--timebase", "pv", "--vmm", "missing"], root);
    assert_eq!(stderr(&pv_missing), "error: --vmm missing: not found\n");
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    {
        let counter = seedbox(&["doctor", "--timebase", "counter"], root);
        assert_eq!(
            stderr(&counter),
            "error: --timebase counter requires Linux KVM; use --timebase pv or omit it on Mac\n"
        );
    }
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

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
#[test]
fn doctor_reports_missing_temporary_assets_without_downloads() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    let output = Command::new(env!("CARGO_BIN_EXE_seedbox"))
        .args(["doctor", "--json"])
        .current_dir(root)
        .env_clear()
        .env("HOME", root)
        .env("SEEDBOX_ASSET_DIR", root)
        .env("PATH", "")
        .env("GH_TOKEN", "invalid\ncredential")
        .output()
        .unwrap();
    let kernel = root.join("aarch64/kernel");
    assert_eq!(output.status.code(), Some(2));
    assert_eq!(stderr(&output), format!("error: --kernel {}: not found; from the Seedbox checkout run `bash tools/build-assets.sh`, or pass --kernel PATH\n", kernel.display()));
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
    // Any engine is admitted; doctor still requires a supported, ready host.
    assert_eq!(output.status.code(), Some(2));
    assert!(!stderr(&output).contains("engine is outside"));
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    assert_eq!(
        stderr(&output),
        "error: --vmm requires Linux KVM; use the built-in Mac engine\n"
    );
    assert!(!marker.exists());
    assert!(output.stdout.is_empty());
}

#[cfg(not(target_os = "linux"))]
#[test]
fn doctor_dev_opt_in_keeps_host_refusal_and_creates_no_state() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("engine"), b"dev engine").unwrap();
    fs::write(dir.path().join("kernel"), b"dev kernel").unwrap();
    for json in [false, true] {
        let mut args = vec!["doctor", "--vmm", "engine", "--kernel", "kernel"];
        if json {
            args.push("--json");
        }
        let output = seedbox(&args, dir.path());
        assert_eq!(output.status.code(), Some(2));
        #[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
        assert_eq!(
            stderr(&output),
            "error: branch counter detection requires an x86-64 host\n"
        );
        #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
        assert_eq!(
            stderr(&output),
            "error: --vmm requires Linux KVM; use the built-in Mac engine\n"
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
    fn ram_only_page_diagnostics_preserve_stdout_json_and_exit_contracts() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let (a, b) = ram_pair();
        for (name, raw, pfns) in [("a", &a, vec![]), ("b", &b, vec![2, 5, 131071])] {
            let path = root.join(name);
            let mut record = fixture();
            write_fixture(&path, &mut record, Some(raw), None);
            write_pages(&path, &mut record, &pfns);
            write_fixture(&path, &mut record, Some(raw), None);
        }
        let plain = seedbox(&["diff", "a", "b"], root);
        assert_eq!(plain.status.code(), Some(1));
        assert_eq!(
            stderr(&plain),
            "ram pages: server: differing pfns: 3; first pfns: [2, 5, 131071]\n"
        );
        let stdout = String::from_utf8(plain.stdout).unwrap();
        assert!(stdout.starts_with("DIFFERENT: a <> b\n"));
        assert_eq!(
            stdout
                .lines()
                .filter(|l| l.starts_with("DIFFERENT: "))
                .count(),
            1
        );
        let json = seedbox(&["diff", "a", "b", "--json"], root);
        assert_eq!(json.status.code(), Some(1));
        assert!(json.stderr.is_empty());
        let value: serde_json::Value = serde_json::from_slice(&json.stdout).unwrap();
        assert_eq!(value["machines"][0]["ram_pages"]["differing_pfns"], 3);
        assert_eq!(
            value["machines"][0]["ram_pages"]["first_pfns"],
            serde_json::json!([2, 5, 131071])
        );
        for field in [
            "status",
            "world_witness",
            "machines",
            "first_record",
            "identity_differences",
            "result_differences",
            "evidence_differences",
            "limitations",
        ] {
            assert!(value.get(field).is_some(), "lost {field}");
        }
        for field in ["machine", "witness", "full_state", "differences"] {
            assert!(value["machines"][0].get(field).is_some(), "lost {field}");
        }
        let matched = seedbox(&["diff", "a", "a"], root);
        assert_eq!(matched.status.code(), Some(0));
        assert!(matched.stderr.is_empty());
    }

    #[test]
    fn selected_page_file_errors_are_setup_errors_with_no_json_stdout() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let (a, b) = ram_pair();
        let mut left = fixture();
        let mut right = fixture();
        for (name, record, raw) in [("a", &mut left, &a), ("b", &mut right, &b)] {
            let path = root.join(name);
            write_fixture(&path, record, Some(raw), None);
            write_pages(&path, record, &[]);
            write_fixture(&path, record, Some(raw), None);
        }
        fs::write(root.join("b/output/machines/server.pages"), b"broken").unwrap();
        for (error, update_digest, remove_memory) in [
            ("diff b/output/machines/server.pages: evidence checksum mismatch", false, false),
            ("diff b/output/machines/server.pages: page hash length must be 1048576 bytes for recorded RAM", true, false),
            ("diff b: page hashes require recorded memory_mb", true, true),
        ] {
            if update_digest {
                right.logs.insert("machines/server.pages".into(), sb::store::ContentDigest::of(b"broken"));
            }
            if remove_memory { right.identity.inputs.profile.semantic_parameters.remove("memory_mb"); }
            write_fixture(&root.join("b"), &mut right, Some(&b), None);
            let output = seedbox(&["diff", "a", "b", "--json"], root);
            assert_eq!(output.status.code(), Some(2));
            assert!(output.stdout.is_empty());
            assert_eq!(stderr(&output), format!("error: {error}; use an intact retained run directory (import bundles first)\n"));
        }
    }

    #[test]
    fn diff_current_multi_machine_parts_and_cli_contract() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let a = current_report(true);
        let mut b = a.clone();
        b["machines"][1]["state_digest_parts"]["disk"] =
            serde_json::to_value(sb::store::ContentDigest::of(b"changed disk")).unwrap();
        write_fixture(&root.join("a"), &mut fixture(), Some(&a), None);
        write_fixture(&root.join("b"), &mut fixture(), Some(&b), None);
        for (right, code, status) in [("a", 0, "MATCH"), ("b", 1, "DIFFERENT")] {
            for json in [false, true] {
                let mut args = vec!["diff", "a", right];
                if json {
                    args.push("--json");
                }
                let output = seedbox(&args, root);
                assert_eq!(output.status.code(), Some(code), "{}", stderr(&output));
                assert!(output.stderr.is_empty());
                let value: serde_json::Value = if json {
                    serde_json::from_slice(&output.stdout).unwrap()
                } else {
                    let plain = String::from_utf8(output.stdout).unwrap();
                    assert!(plain.starts_with(&format!("{status}: a <> {right}\n")));
                    assert_eq!(
                        plain
                            .lines()
                            .filter(|l| l.starts_with("MATCH: ") || l.starts_with("DIFFERENT: "))
                            .count(),
                        1
                    );
                    let first = plain
                        .lines()
                        .find_map(|l| l.strip_prefix("first record: "))
                        .unwrap();
                    serde_json::json!({"first_record": serde_json::from_str::<serde_json::Value>(first).unwrap()})
                };
                if json {
                    assert_eq!(value["status"], status);
                    assert_eq!(value["machines"].as_array().unwrap().len(), 2);
                }
                if code == 1 {
                    let first = &value["first_record"];
                    assert_eq!(first["machine"], "server");
                    assert_eq!(first["source"], "terminal");
                    assert!(first["record"].is_null());
                    assert_eq!(first["a"]["time_ns"], 100);
                    assert_eq!(
                        first["a"]["value"]["state_digest_parts"]["disk"],
                        a["machines"][1]["state_digest_parts"]["disk"]
                    );
                    assert_eq!(
                        first["b"]["value"]["state_digest_parts"]["disk"],
                        b["machines"][1]["state_digest_parts"]["disk"]
                    );
                    if json {
                        assert_eq!(
                            value["machines"][0]["differences"][0]["field"],
                            "/state_digest_parts/disk"
                        );
                        assert_eq!(
                            value["machines"][0]["differences"]
                                .as_array()
                                .unwrap()
                                .len(),
                            1
                        );
                        assert!(value["machines"][1]["differences"]
                            .as_array()
                            .unwrap()
                            .is_empty());
                    }
                } else {
                    assert!(value["first_record"].is_null());
                }
            }
        }
    }

    #[test]
    fn diff_current_digest_fields_remain_strict_and_manifest_bound() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        for pointer in [
            "/world_witness",
            "/machines/1/witness",
            "/machines/1/state_digest",
            "/machines/1/state_digest_parts/regs",
            "/machines/1/state_digest_parts/xsave",
            "/machines/1/state_digest_parts/xcrs",
            "/machines/1/state_digest_parts/msrs",
            "/machines/1/state_digest_parts/time_page",
            "/machines/1/state_digest_parts/pending_io",
            "/machines/1/state_digest_parts/ram",
            "/machines/1/state_digest_parts/disk",
            "/machines/1/state_digest_parts/devices",
        ] {
            for invalid in ["ABCDEF".repeat(11), "not a digest".into(), "A".repeat(64)] {
                let mut raw = current_report(true);
                raw["machines"][1]["state_digest_parts"]["devices"] =
                    serde_json::to_value(sb::store::ContentDigest::of(b"devices")).unwrap();
                *raw.pointer_mut(pointer).unwrap() = invalid.into();
                write_fixture(&root.join("a"), &mut fixture(), Some(&raw), None);
                let output = seedbox(&["diff", "a", "a", "--json"], root);
                assert_eq!(output.status.code(), Some(2), "{pointer}");
                assert!(output.stdout.is_empty());
                assert!(stderr(&output).starts_with("error: diff a/output/vmm.json: "));
                assert!(stderr(&output).contains("expected a lowercase SHA-256 digest"));
            }
        }
        let mut record = fixture();
        record.report.as_mut().unwrap().full_state = Some(std::collections::BTreeMap::from([(
            "server".into(),
            sb::store::ContentDigest::of(b"wrong"),
        )]));
        write_fixture(
            &root.join("a"),
            &mut record,
            Some(&current_report(true)),
            None,
        );
        let output = seedbox(&["diff", "a", "a"], root);
        assert_eq!(output.status.code(), Some(2));
        assert!(output.stdout.is_empty());
        assert_eq!(stderr(&output), "error: diff a: machine state digest disagrees with manifest; use an intact retained run directory (import bundles first)\n");
    }

    #[test]
    fn inspect_vmm_failure_shows_bounded_diagnostics_only_on_stderr() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let run = root.join("failed");
        let mut record = fixture();
        record.outcome = sb::orchestration::Outcome::Error;
        record.report = None;
        let error = sb::Error::Execution(sb::vmm::VMM_FAILURE_MESSAGE.into()).to_string();
        record.error = Some(error.clone());
        write_fixture(&run, &mut record, None, None);
        let manifest = fs::read(run.join("manifest.json")).unwrap();
        let diagnostic = run.join("output/vmm.stderr");
        let overshoot = "overshoot on client: 17 branches past stop target 100";
        // The old prefix exceeds the read cap; blanks do not consume line slots.
        fs::write(
            &diagnostic,
            format!(
                "{}\nold\nfirst\n\nsecond\n \t\r\nthird\nERROR: misleading status\n{overshoot}\n\n",
                "x".repeat(32 * 1024)
            ),
        )
        .unwrap();
        let expected_stdout =
            format!("ERROR: fixture (seed 0)\ntimebase: counter\nerror: {error}\n");
        let output = seedbox(&["inspect", "failed"], root);
        assert!(output.status.success());
        assert_eq!(String::from_utf8_lossy(&output.stdout), expected_stdout);
        assert_eq!(
            stderr(&output),
            format!("vmm stderr: first\nvmm stderr: second\nvmm stderr: third\nvmm stderr: ERROR: misleading status\nvmm stderr: {overshoot}\n")
        );
        let statuses = output
            .stdout
            .split(|byte| *byte == b'\n')
            .chain(output.stderr.split(|byte| *byte == b'\n'))
            .filter(|line| line.starts_with(b"ERROR: "))
            .count();
        assert_eq!(statuses, 1);
        let json = seedbox(&["inspect", "failed", "--json"], root);
        assert!(json.status.success());
        assert!(json.stderr.is_empty());
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&json.stdout).unwrap(),
            serde_json::to_value(&record).unwrap()
        );
        assert_eq!(fs::read(run.join("manifest.json")).unwrap(), manifest);

        for bytes in [b"".as_slice(), b"\n \t\r\n"] {
            fs::write(&diagnostic, bytes).unwrap();
            let output = seedbox(&["inspect", "failed"], root);
            assert!(output.status.success());
            assert_eq!(String::from_utf8_lossy(&output.stdout), expected_stdout);
            assert!(output.stderr.is_empty());
        }
        fs::remove_file(&diagnostic).unwrap();
        let output = seedbox(&["inspect", "failed"], root);
        assert!(output.status.success());
        assert_eq!(String::from_utf8_lossy(&output.stdout), expected_stdout);
        assert!(output.stderr.is_empty());
        fs::create_dir(&diagnostic).unwrap();
        let output = seedbox(&["inspect", "failed"], root);
        assert!(output.status.success());
        assert_eq!(String::from_utf8_lossy(&output.stdout), expected_stdout);
        assert!(output.stderr.is_empty());
    }

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

#[allow(dead_code)]
mod timed_logs {
    use super::*;
    use seedbox as sb;
    include!("support/diff_fixture.rs");
    include!("support/logs_fixture.rs");

    #[test]
    fn logs_defaults_filters_and_binary_json() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        timed_fixture(&root.join("run"));
        let text = seedbox(&["logs", "run"], root);
        assert!(text.status.success(), "{}", stderr(&text));
        assert_eq!(stderr(&text), "logs: run\n");
        assert_eq!(text.stdout, b"\0\xff\xfeA\nB");
        let text = seedbox(
            &[
                "logs",
                "run",
                "--stream",
                "console",
                "--since-ns",
                "10",
                "--until-ns",
                "20",
            ],
            root,
        );
        assert!(text.status.success());
        assert_eq!(text.stdout, b"\0\xffA\n");
        let text = seedbox(&["logs", "run", "--since-ns", "20"], root);
        assert!(text.status.success());
        assert_eq!(text.stdout, b"B"); // No added newline or chunk decoration.
        let text = seedbox(&["logs", "run", "--until-ns", "10"], root);
        assert!(text.status.success());
        assert!(text.stdout.is_empty());
        for (args, orders) in [
            (vec!["logs", "run", "--json"], vec![0, 1, 2, 3]),
            (
                vec![
                    "logs",
                    "run",
                    "--machine",
                    "workload",
                    "--stream",
                    "console",
                    "--since-ns",
                    "10",
                    "--until-ns",
                    "20",
                    "--json",
                ],
                vec![0, 2],
            ),
            (vec!["logs", "run", "--stream", "mgmt", "--json"], vec![1]),
            (vec!["logs", "run", "--since-ns", "20", "--json"], vec![3]),
            (vec!["logs", "run", "--until-ns", "10", "--json"], vec![]),
        ] {
            let output = seedbox(&args, root);
            assert!(output.status.success(), "{}", stderr(&output));
            assert!(output.stderr.is_empty());
            let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
            assert_eq!(
                json["chunks"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|v| v["order"].as_u64().unwrap())
                    .collect::<Vec<_>>(),
                orders
            );
            assert_eq!(json["status"], "LOGS");
            assert_eq!(json["omissions"], serde_json::json!({}));
        }
        let out = seedbox(&["logs", "run", "--json"], root);
        let json: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(json["chunks"][0]["bytes_hex"], "00ff");
    }

    #[test]
    fn logs_errors_and_omissions_are_explicit() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let path = root.join("run");
        let mut record = timed_fixture(&path);
        for (args, expected) in [
            (
                vec!["logs", "missing"],
                "error: run directory missing: not found\n",
            ),
            (
                vec!["logs", "run", "--machine", "unknown"],
                "error: --machine unknown: unknown machine; use a machine from inspect --json\n",
            ),
            (
                vec!["logs", "run", "--since-ns", "20", "--until-ns", "10"],
                "error: --until-ns must be at least --since-ns\n",
            ),
        ] {
            let out = seedbox(&args, root);
            assert_eq!(out.status.code(), Some(2));
            assert_eq!(stderr(&out), expected);
            assert!(out.stdout.is_empty());
        }
        for (flag, value) in [
            ("--stream", "stdout"),
            ("--since-ns", "1s"),
            ("--until-ns", "18446744073709551616"),
        ] {
            let out = seedbox(&["logs", "run", flag, value], root);
            assert_eq!(out.status.code(), Some(2));
            assert!(stderr(&out).contains(flag));
        }
        record.logs.remove("machines/workload.console.chunks");
        write_fixture(&path, &mut record, None, None);
        let out = seedbox(&["logs", "run", "--json"], root);
        assert!(out.status.success());
        let json: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
        assert!(json["omissions"]["machines/workload.console.chunks"]
            .as_str()
            .unwrap()
            .contains("unavailable"));
        let out = seedbox(&["logs", "run"], root);
        assert!(out.status.success());
        assert_eq!(out.stdout, b"\xfe");
        assert_eq!(stderr(&out), "logs: run\nomission: [\"machines/workload.console.chunks\",\"committed-time index unavailable; raw bytes cannot be timed\"]\n");
        add_log(
            &path,
            &mut record,
            "machines/workload.console.chunks",
            b"SBLOG001x",
        );
        write_fixture(&path, &mut record, None, None);
        let out = seedbox(&["logs", "run"], root);
        assert_eq!(out.status.code(), Some(2));
        assert!(out.stdout.is_empty());
        assert_eq!(stderr(&out),"error: logs run: machines/workload.console.chunks: truncated or oversized chunk index; use an intact retained run directory (import bundles first)\n");
    }
}

#[allow(dead_code)]
#[path = "../../seedbox-vmm/src/output_layout.rs"]
mod checkpoint_layout;
#[allow(dead_code)]
#[path = "../../seedbox-vmm/src/log_index.rs"]
mod checkpoint_log_producer;

#[allow(dead_code)]
mod checkpoint_evidence {
    use super::*;
    use super::{checkpoint_layout as checkpoint, checkpoint_log_producer as producer};
    use seedbox as sb;
    use std::io::Write;
    include!("support/diff_fixture.rs");

    fn branch_record(seed: u64) -> sb::orchestration::RunRecord {
        let mut record = fixture();
        let parameters = &mut record.identity.inputs.profile.semantic_parameters;
        parameters.insert("checkpoint_at_ns".into(), "20".into());
        parameters.insert("checkpoint_branch_seed".into(), seed.to_string());
        record
    }

    fn branch_output(run: &Path, seed: u64) -> std::path::PathBuf {
        let out = run.join("output/machines");
        let branch = checkpoint::checkpoint_directory(out.to_str().unwrap(), 1, 1, 20, seed);
        fs::create_dir_all(&branch).unwrap();
        branch.into()
    }

    // Use the same recursive retention layout as a run, including batch subdirectories.
    fn retain_output(run: &Path, record: &mut sb::orchestration::RunRecord, directory: &Path) {
        for entry in fs::read_dir(directory).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                retain_output(run, record, &path);
            } else {
                let name = path.strip_prefix(run.join("output")).unwrap();
                record.logs.insert(
                    name.to_str().unwrap().into(),
                    sb::store::ContentDigest::of(&fs::read(&path).unwrap()),
                );
            }
        }
    }

    #[test]
    fn logs_on_checkpoint_branch_include_post_checkpoint_chunks() {
        for stream in [false, true] {
            let tmp = tempfile::tempdir().unwrap();
            let root = tmp.path();
            let run = root.join("branch");
            let branch = branch_output(&run, 11);
            let mut record = branch_record(11);
            let mut order = 0;
            let mut snapshots = Vec::new();
            for ext in ["console", "mgmt"] {
                let raw = branch.join(format!("workload.{ext}"));
                let raw = raw.to_str().unwrap();
                let mut parent = producer::Index::default();
                parent.open(&producer::index_name(raw));
                parent.capture(6, 10, 0, &mut order);
                parent.finish();
                snapshots.push((ext, parent.snapshot()));
                // Stale output from the prefix sink must be replaced on restore.
                fs::write(raw, b"prefixdiscarded").unwrap();
                let mut discarded_order = 100;
                parent.capture(15, 30, 0, &mut discarded_order);
                parent.finish();
                drop(parent);
            }
            for (ext, mut restored) in snapshots {
                let raw = branch.join(format!("workload.{ext}"));
                let raw = raw.to_str().unwrap();
                let mut sink = restored.reopen_stream(raw, b"prefix", stream).unwrap();
                let suffix = if ext == "console" {
                    b"console"
                } else {
                    b"manage!"
                };
                if let Some(file) = sink.as_mut() {
                    file.write_all(suffix).unwrap();
                } else {
                    fs::write(raw, [b"prefix".as_slice(), suffix].concat()).unwrap();
                }
                restored.capture(13, 21, 0, &mut order);
                restored.finish();
            }
            retain_output(&run, &mut record, &run.join("output"));
            write_fixture(&run, &mut record, None, None);
            for (ext, expected) in [("console", b"console"), ("mgmt", b"manage!")] {
                let out = seedbox(
                    &["logs", "branch", "--stream", ext, "--since-ns", "20"],
                    root,
                );
                assert!(out.status.success(), "{}", stderr(&out));
                assert_eq!(out.stdout, expected);
                assert_eq!(stderr(&out), "logs: branch\n");
                let out = seedbox(
                    &["logs", "branch", "--stream", ext, "--until-ns", "20"],
                    root,
                );
                assert!(out.status.success(), "{}", stderr(&out));
                assert_eq!(out.stdout, b"prefix");
            }
        }
    }

    #[test]
    fn diff_between_full_state_checkpoint_branches_finds_page_hashes() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let (a, b) = ram_pair();
        for (name, seed, raw, pfn) in [("a", 11, &a, None), ("b", 12, &b, Some(37))] {
            let run = root.join(name);
            let branch = branch_output(&run, seed);
            let mut record = branch_record(seed);
            record.identity.inputs.profile.witness_mode = sb::execution::WitnessMode::FullState;
            let mut pages = vec![0u8; 1024 * 1024];
            if let Some(pfn) = pfn {
                pages[pfn * 8] = 1;
            }
            fs::write(branch.join("server.pages"), pages).unwrap();
            retain_output(&run, &mut record, &run.join("output"));
            write_fixture(&run, &mut record, Some(raw), None);
        }
        let out = seedbox(&["diff", "a", "b", "--json"], root);
        assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
        assert!(out.stderr.is_empty());
        let report: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(report["machines"][0]["ram_pages"]["differing_pfns"], 1);
        assert_eq!(
            report["machines"][0]["ram_pages"]["first_pfns"],
            serde_json::json!([37])
        );
        assert!(!report["limitations"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v.as_str().unwrap().contains("page hashes unavailable")));
    }
}

#[test]
fn logs_help_is_portable_and_has_no_runtime_options() {
    let tmp = tempfile::tempdir().unwrap();
    let out = seedbox(&["help", "logs"], tmp.path());
    assert!(out.status.success());
    let help = String::from_utf8(out.stdout).unwrap();
    for text in [
        "<RUN>",
        "--machine",
        "--stream",
        "--since-ns",
        "--until-ns",
        "--json",
    ] {
        assert!(help.contains(text));
    }
    for text in ["--kernel", "--seed", "--vmm", "--cpu", "--state"] {
        assert!(!help.contains(text));
    }
    let out = seedbox(&["logs"], tmp.path());
    assert_eq!(out.status.code(), Some(2));
    assert!(stderr(&out).contains("<RUN>"));
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

#[test]
fn checkpoint_flags_are_paired_typed_and_documented() {
    let dir = tempfile::tempdir().unwrap();
    for (args, phrase) in [
        (vec!["run", "--checkpoint-at-ns", "1"], "--branch-seed"),
        (vec!["run", "--branch-seed", "2"], "--checkpoint-at-ns"),
        (
            vec!["run", "--checkpoint-at-ns", "bad", "--branch-seed", "2"],
            "--checkpoint-at-ns",
        ),
        (
            vec![
                "run",
                "--checkpoint-at-ns",
                "1",
                "--branch-seed",
                "18446744073709551616",
            ],
            "--branch-seed",
        ),
    ] {
        let result = seedbox_with(&args, dir.path(), None);
        assert_eq!(result.status.code(), Some(2));
        assert!(stderr(&result).contains(phrase), "{}", stderr(&result));
    }
    let help = seedbox_with(&["help", "run"], dir.path(), None);
    assert!(help.status.success());
    let help = String::from_utf8(help.stdout).unwrap();
    assert!(help.contains("--checkpoint-at-ns"));
    assert!(help.contains("the parent seed continues unchanged"));
    assert!(help.contains("--branch-seed"));
    assert!(!help.contains("SEEDBOX_CHECKPOINT"));
    fs::write(dir.path().join("compose.yaml"), "name: checkpoint-test\nservices:\n  workload: {image: example/workload:v1}\nx-seedbox:\n  version: 1\n  seed: 0\n  limits: {simulated_time: 1s, wall_time: 1m}\n  check: {service: workload}\n").unwrap();
    let result = seedbox_with(
        &[
            "run",
            "--checkpoint-at-ns",
            "1000000000",
            "--branch-seed",
            "2",
        ],
        dir.path(),
        None,
    );
    assert_eq!(result.status.code(), Some(2));
    let expected = if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        "error: --checkpoint-at-ns and --branch-seed require Linux KVM; omit them on Mac\n"
    } else {
        "error: --checkpoint-at-ns must be before the simulated time limit; choose an earlier checkpoint\n"
    };
    assert_eq!(stderr(&result), expected);
    assert!(!dir.path().join(".seedbox").exists());
    let accepted = seedbox_with(
        &[
            "run",
            "--checkpoint-at-ns",
            "0",
            "--branch-seed",
            "0",
            "--vmm",
            "/checkpoint-missing-vmm",
        ],
        dir.path(),
        None,
    );
    assert_eq!(
        stderr(&accepted),
        if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
            "error: --checkpoint-at-ns and --branch-seed require Linux KVM; omit them on Mac\n"
        } else {
            "error: --vmm /checkpoint-missing-vmm: not found\n"
        }
    );
}

#[test]
fn timebase_help_values_and_replay_uses_only_the_recorded_choice() {
    let directory = tempfile::tempdir().unwrap();
    for command in ["run", "prepare", "explore"] {
        let help = seedbox(&["help", command], directory.path());
        assert!(help.status.success());
        let help = String::from_utf8_lossy(&help.stdout);
        assert!(help.contains("--timebase <TIMEBASE>"));
        assert!(help.contains("[default: pv on Mac; counter on Linux]"));
        assert!(help.contains("Count retired guest branches with the host PMU"));
        assert!(help.contains("Advance time at guest exits, syscalls and clock reads"));
        let bad = seedbox(&[command, "--timebase", "future"], directory.path());
        assert_eq!(bad.status.code(), Some(2));
        assert!(stderr(&bad).contains("--timebase <TIMEBASE>"));
        assert!(stderr(&bad).contains("possible values: counter, pv"));
    }
    let help = seedbox(&["help", "replay"], directory.path());
    assert!(help.status.success());
    assert!(!String::from_utf8_lossy(&help.stdout).contains("--timebase"));
    let override_attempt = seedbox(&["replay", ".", "--timebase", "pv"], directory.path());
    assert_eq!(override_attempt.status.code(), Some(2));
    assert!(stderr(&override_attempt).contains("unexpected argument '--timebase'"));
    assert!(seedbox(&["init", "demo"], directory.path())
        .status
        .success());
    for command in ["run", "explore"] {
        for value in ["counter", "pv"] {
            let output = seedbox(
                &[command, "no-such.yaml", "--timebase", value],
                directory.path(),
            );
            assert_eq!(output.status.code(), Some(2));
            let expected = if cfg!(all(target_os = "macos", target_arch = "aarch64"))
                && value == "counter"
            {
                "error: --timebase counter requires Linux KVM; use --timebase pv or omit it on Mac\n"
            } else {
                "error: Compose file no-such.yaml: not found\n"
            };
            assert_eq!(stderr(&output), expected);
        }
    }
}

fn timebase_record(directory: &Path, timebase: Option<&str>) {
    use seedbox::{orchestration::RunRecord, store::ContentDigest};
    let fixture: serde_json::Value =
        serde_json::from_slice(include_bytes!("fixtures/formats/synthetic-c84-v2.json")).unwrap();
    let mut record: RunRecord = serde_json::from_value(fixture["record"].clone()).unwrap();
    record.identity.inputs.profile.backend = "seedbox-kvm".into();
    if let Some(value) = timebase {
        record
            .identity
            .inputs
            .profile
            .semantic_parameters
            .insert("timebase".into(), value.into());
    }
    if let Some(report) = &mut record.report {
        report.timebase = match timebase {
            Some("pv") => Some(seedbox::execution::Timebase::Pv),
            None => Some(seedbox::execution::Timebase::Counter),
            _ => None,
        };
    }
    record.run_id = ContentDigest::of(&serde_json::to_vec(&record.identity).unwrap());
    let checksum = ContentDigest::of(&serde_json::to_vec(&record).unwrap());
    fs::write(
        directory.join("manifest.json"),
        serde_json::to_vec(&serde_json::json!({"sha256": checksum, "record": record})).unwrap(),
    )
    .unwrap();
}

#[test]
fn inspect_shows_timebase_and_json_preserves_library_fields() {
    let directory = tempfile::tempdir().unwrap();
    for (parameter, value) in [(None, "counter"), (Some("pv"), "pv")] {
        timebase_record(directory.path(), parameter);
        let output = seedbox(&["inspect", "."], directory.path());
        assert!(output.status.success(), "{}", stderr(&output));
        assert!(String::from_utf8_lossy(&output.stdout).contains(&format!("\ntimebase: {value}\n")));
        let text = String::from_utf8_lossy(&output.stdout);
        assert_eq!(
            text.lines()
                .filter(|line| line
                    .split_once(": ")
                    .is_some_and(|(word, _)| !word.is_empty()
                        && word.bytes().all(|byte| byte.is_ascii_uppercase())))
                .count(),
            1
        );
        let output = seedbox(&["inspect", ".", "--json"], directory.path());
        assert!(output.status.success(), "{}", stderr(&output));
        assert!(output.stderr.is_empty());
        let record: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(record["report"]["timebase"], value);
        assert_eq!(
            record["identity"]["inputs"]["profile"]["semantic_parameters"]["timebase"].as_str(),
            parameter
        );
    }
}

#[test]
fn replay_rejects_unknown_recorded_timebase_before_host_checks() {
    let directory = tempfile::tempdir().unwrap();
    timebase_record(directory.path(), Some("future"));
    let output = seedbox(&["replay", "."], directory.path());
    assert_eq!(output.status.code(), Some(2));
    assert_eq!(stderr(&output), "error: input integrity: recorded --timebase future: expected counter or pv; use a run recorded with a supported timebase\n");
    assert!(output.stdout.is_empty());
    assert!(!directory.path().join(".seedbox").exists());
    // Inspection remains available for diagnosing unsupported recorded values.
    let inspect = seedbox(&["inspect", "."], directory.path());
    assert!(inspect.status.success(), "{}", stderr(&inspect));
    assert!(String::from_utf8_lossy(&inspect.stdout).contains("timebase: future"));
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
#[test]
fn mac_counter_timebase_is_rejected_before_preparation() {
    let directory = tempfile::tempdir().unwrap();
    fs::write(directory.path().join("compose.yaml"), "name: mac-timebase\nservices:\n  app:\n    image: busybox:1.37.0\nx-seedbox:\n  version: 1\n  seed: 1\n  limits: {simulated_time: 1s, wall_time: 1m}\n  check: {service: app, exit_code: 0}\n").unwrap();
    for command in ["run", "explore"] {
        let output = seedbox(&[command, ".", "--timebase", "counter"], directory.path());
        assert_eq!(output.status.code(), Some(2));
        assert_eq!(
            stderr(&output),
            "error: --timebase counter requires Linux KVM; use --timebase pv or omit it on Mac\n"
        );
    }
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
#[test]
fn mac_linux_only_runtime_modes_fail_before_state_or_preparation() {
    let directory = tempfile::tempdir().unwrap();
    fs::write(directory.path().join("compose.yaml"), "name: mac-modes\nservices:\n  app:\n    image: busybox:1.37.0\nx-seedbox:\n  version: 1\n  seed: 1\n  limits: {simulated_time: 1s, wall_time: 1m}\n  check: {service: app, exit_code: 0}\n").unwrap();
    for (args, expected) in [
        (
            vec!["run", ".", "--page-timer"],
            "error: --page-timer and --deferred-tx require Linux KVM; omit them on Mac\n",
        ),
        (
            vec!["run", ".", "--deferred-tx"],
            "error: --page-timer and --deferred-tx require Linux KVM; omit them on Mac\n",
        ),
        (
            vec!["run", ".", "--checkpoint-at-ns", "1", "--branch-seed", "2"],
            "error: --checkpoint-at-ns and --branch-seed require Linux KVM; omit them on Mac\n",
        ),
    ] {
        let output = seedbox(&args, directory.path());
        assert_eq!(output.status.code(), Some(2));
        assert_eq!(stderr(&output), expected);
        assert!(output.stdout.is_empty());
        assert!(!directory.path().join(".seedbox").exists());
    }
}

#[test]
fn page_timer_is_an_opt_in_runtime_flag_and_replay_accepts_no_override() {
    let directory = tempfile::tempdir().unwrap();
    for command in ["prepare", "run", "explore"] {
        let output = seedbox(&[command, "--help"], directory.path());
        assert!(output.status.success());
        let help = String::from_utf8_lossy(&output.stdout);
        assert!(help.contains("--page-timer"));
        let output = seedbox(&[command, "no-such.yaml", "--page-timer"], directory.path());
        assert_eq!(output.status.code(), Some(2));
        assert!(!stderr(&output).contains("unexpected argument '--page-timer'"));
    }
    let output = seedbox(&["replay", ".", "--page-timer"], directory.path());
    assert_eq!(output.status.code(), Some(2));
    assert!(stderr(&output).contains("unexpected argument '--page-timer'"));
}

#[test]
fn deferred_tx_help_is_presence_only_and_replay_uses_the_recorded_mode() {
    let directory = tempfile::tempdir().unwrap();
    for command in ["run", "prepare", "explore"] {
        let help = seedbox(&["help", command], directory.path());
        assert!(help.status.success());
        assert!(String::from_utf8_lossy(&help.stdout).contains("--deferred-tx"));
        let mut args = vec![command, "no-such.yaml", "--deferred-tx"];
        if command == "prepare" {
            args.extend(["--output", "prepared"]);
        }
        let output = seedbox(&args, directory.path());
        assert_eq!(output.status.code(), Some(2));
        let expected = if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
            "error: --page-timer and --deferred-tx require Linux KVM; omit them on Mac\n"
        } else {
            "error: Compose file no-such.yaml: not found\n"
        };
        assert_eq!(stderr(&output), expected);
    }
    let help = seedbox(&["help", "replay"], directory.path());
    assert!(!String::from_utf8_lossy(&help.stdout).contains("--deferred-tx"));
    let output = seedbox(&["replay", ".", "--deferred-tx"], directory.path());
    assert_eq!(output.status.code(), Some(2));
    assert!(stderr(&output).contains("unexpected argument '--deferred-tx'"));
    timebase_record(directory.path(), None);
    let mut record = seedbox::orchestration::inspect(directory.path()).unwrap();
    record
        .identity
        .inputs
        .profile
        .semantic_parameters
        .insert("deferred_tx".into(), "future".into());
    record.run_id =
        seedbox::store::ContentDigest::of(&serde_json::to_vec(&record.identity).unwrap());
    let checksum = seedbox::store::ContentDigest::of(&serde_json::to_vec(&record).unwrap());
    fs::write(
        directory.path().join("manifest.json"),
        serde_json::to_vec(&serde_json::json!({"sha256": checksum, "record": record})).unwrap(),
    )
    .unwrap();
    let output = seedbox(&["replay", "."], directory.path());
    assert_eq!(output.status.code(), Some(2));
    assert!(stderr(&output).contains("recorded --deferred-tx future"));
}

#[test]
fn runtime_assets_override_is_documented_and_missing_paths_fail_before_preparation() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    assert!(seedbox(&["init", "demo"], root).status.success());
    let project = root.join("demo");
    for command in ["run", "explore", "prepare"] {
        let help = seedbox(&["help", command], root);
        assert!(help.status.success());
        let help = String::from_utf8_lossy(&help.stdout);
        assert!(help.contains("--runtime-assets"), "{help}");
        assert!(help.contains("SEEDBOX_RUNTIME_ASSETS"), "{help}");
        assert!(
            help.contains("local assets in the OS temporary directory"),
            "{help}"
        );
        let mut args = vec![command, "--runtime-assets", "missing.tar.gz"];
        if command == "prepare" {
            args.extend(["--output", "prepared.json"]);
        }
        let output = seedbox(&args, &project);
        assert_eq!(output.status.code(), Some(2));
        assert!(output.stdout.is_empty());
        assert_eq!(
            stderr(&output),
            "error: --runtime-assets missing.tar.gz: not found\n"
        );
        assert!(!project.join(".seedbox/runs").exists());
    }
    let output = Command::new(env!("CARGO_BIN_EXE_seedbox"))
        .arg("run")
        .current_dir(&project)
        .env_clear()
        .env("HOME", root)
        .env("SEEDBOX_ASSET_DIR", root)
        .env("PATH", "")
        .env("SEEDBOX_RUNTIME_ASSETS", "env-missing.tar.gz")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert_eq!(
        stderr(&output),
        "error: --runtime-assets env-missing.tar.gz: not found\n"
    );
    for command in ["doctor", "replay"] {
        let help = seedbox(&["help", command], root);
        assert!(help.status.success());
        assert!(!String::from_utf8_lossy(&help.stdout).contains("--runtime-assets"));
    }
}

#[test]
fn invalid_local_runtime_archive_has_path_context_without_downloads_or_tools() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    assert!(seedbox(&["init", "demo"], root).status.success());
    let project = root.join("demo");
    fs::write(project.join("invalid.tar.gz"), b"not an archive").unwrap();
    fs::write(project.join("kernel"), b"kernel").unwrap();
    let output = seedbox(
        &[
            "run",
            "--kernel",
            "kernel",
            "--runtime-assets",
            "invalid.tar.gz",
        ],
        &project,
    );
    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    let error = stderr(&output);
    assert!(
        error.starts_with("error: runtime assets invalid.tar.gz:"),
        "{error}"
    );
    assert!(!error.contains("downloading"), "{error}");
    assert!(!project.join(".seedbox/runs").exists());
}

#[test]
fn removed_builder_flags_fail_before_preparation() {
    let directory = tempfile::tempdir().unwrap();
    fs::write(directory.path().join("compose.yaml"), "name: source-build\nservices:\n  client: {build: ./missing}\nx-seedbox:\n  version: 1\n  limits: {simulated_time: 1s, wall_time: 1m}\n  check: {service: client}\n").unwrap();
    for flag in ["--builder", "--build-epoch"] {
        let output = seedbox(&["run", flag, "value"], directory.path());
        assert!(stderr(&output).contains("unexpected argument"));
    }
}

#[test]
fn omitted_run_subcommand_accepts_the_same_kernel_and_compose_arguments() {
    let directory = tempfile::tempdir().unwrap();
    assert!(seedbox(&["init", "demo"], directory.path())
        .status
        .success());
    let explicit = seedbox(&["run", "--kernel", "missing", "demo"], directory.path());
    let shorthand = seedbox(&["--kernel", "missing", "demo"], directory.path());
    assert_eq!(explicit.status.code(), shorthand.status.code());
    assert_eq!(stderr(&explicit), stderr(&shorthand));
    assert_eq!(stderr(&shorthand), "error: --kernel missing: not found\n");
}
