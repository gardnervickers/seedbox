use std::{
    fs,
    path::Path,
    process::{Command, Output},
};

/// Run the CLI with a scrubbed environment: no `SEEDBOX_*`, `CARGO*` or
/// `SUDO_USER` variables, `HOME` and scratch storage at `cwd`, and `PATH` set to
/// `path` (empty if `None`), so no default can come from the developer's machine.
fn seedbox_with(args: &[&str], cwd: &Path, path: Option<&Path>) -> Output {
    let _server_cleanup = ServerCleanup(cwd.join(".local/state/seedbox/server.sock"));
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

/// A command may automatically start a daemon in this test's private HOME.
struct ServerCleanup(std::path::PathBuf);
impl Drop for ServerCleanup {
    fn drop(&mut self) {
        if !self.0.exists() {
            return;
        }
        let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        else {
            return;
        };
        let Ok(client) = seedbox_client::Client::connect_to(&self.0) else {
            return;
        };
        let Ok(Ok(info)) = runtime.block_on(async {
            tokio::time::timeout(std::time::Duration::from_secs(2), client.info()).await
        }) else {
            return;
        };
        // SAFETY: this server answered on the socket inside this test's private HOME.
        unsafe {
            libc::kill(info.process_id as i32, libc::SIGTERM);
        }
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while std::time::Instant::now() < deadline
            && std::os::unix::net::UnixStream::connect(&self.0).is_ok()
        {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }
}

fn seedbox(args: &[&str], cwd: &Path) -> Output {
    seedbox_with(args, cwd, None)
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[cfg(unix)]
#[test]
fn long_server_state_path_fails_before_creating_state() {
    let directory = tempfile::tempdir().unwrap();
    let state = directory.path().join("s".repeat(200));
    let output = seedbox(
        &["serve", "--state", state.to_str().unwrap()],
        directory.path(),
    );
    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    assert_eq!(stderr(&output), format!(
        "error: server socket {}: invalid local socket path; set SEEDBOX_STATE to a shorter directory\n",
        state.join("server.sock").display()
    ));
    assert!(!state.exists());
}

#[test]
fn server_performance_settings_are_bounded_usage_errors_before_state() {
    let directory = tempfile::tempdir().unwrap();
    let help = seedbox(&["help", "serve"], directory.path());
    assert!(help.status.success());
    let help = String::from_utf8_lossy(&help.stdout);
    let performance = help
        .split_once("Performance (never changes results):")
        .map(|(_, section)| section)
        .unwrap_or_else(|| panic!("{help}"));
    assert!(
        performance.contains("--machine-workers <N>") && performance.contains("[default: 1]"),
        "{help}"
    );
    let state = directory.path().join("state");
    let mut bounded = vec![("--machine-workers", ["0", "65", "all"])];
    if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        // The ARM engine has no PV watchdog to tune.
        assert!(!help.contains("--watchdog-ms"), "{help}");
        let output = seedbox(
            &[
                "serve",
                "--state",
                state.to_str().unwrap(),
                "--watchdog-ms",
                "20",
            ],
            directory.path(),
        );
        assert_eq!(output.status.code(), Some(2));
        assert!(stderr(&output).contains("unexpected argument '--watchdog-ms'"));
    } else {
        assert!(performance.contains("--watchdog-ms <MS>"), "{help}");
        assert!(performance.contains("[default: 20]"), "{help}");
        bounded.push(("--watchdog-ms", ["0", "60001", "fast"]));
    }
    for (flag, values) in bounded {
        for value in values {
            let output = seedbox(
                &["serve", "--state", state.to_str().unwrap(), flag, value],
                directory.path(),
            );
            assert_eq!(output.status.code(), Some(2), "{flag} {value}");
            assert!(
                stderr(&output).contains(&format!("invalid value '{value}' for '{flag}")),
                "{}",
                stderr(&output)
            );
        }
    }
    assert!(!state.exists());
}

#[test]
fn explicit_asset_directory_overrides_bundled_kernel_without_build_tools() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let binary = root.join("seedbox");
    fs::copy(env!("CARGO_BIN_EXE_seedbox"), &binary).unwrap();
    let output = Command::new(binary)
        .arg("doctor")
        .current_dir(&root)
        .env_clear()
        .env("HOME", &root)
        .env("SEEDBOX_ASSET_DIR", &root)
        .env("PATH", "")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert_eq!(stderr(&output), format!("error: --kernel {}: not found; from the Seedbox checkout run `cargo xtask build-assets`, or pass --kernel PATH\n", root.join(std::env::consts::ARCH).join("kernel").display()));
    assert!(!root.join(".seedbox").exists());
}

#[test]
fn version_and_help_describe_the_builtin_engine_without_exposing_worker_dispatch() {
    let directory = tempfile::tempdir().unwrap();

    let version = seedbox(&["--version"], directory.path());
    assert!(version.status.success());
    assert_eq!(
        String::from_utf8_lossy(&version.stdout),
        format!(
            "seedbox {}\n",
            seedbox_application::engine::SIMULATOR_VERSION
        )
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
    assert!(help.contains("--detach"), "{help}");
    assert!(help.contains("--paused"), "{help}");
    assert!(help.contains("--seed"), "{help}");
    assert!(!help.contains("--vmm"), "{help}");
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
fn internal_worker_requires_a_request_without_state_or_external_tools() {
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
        stderr(&output).contains("internal world worker requires --request PATH"),
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
fn project_argument_defaults_to_the_project_directory() {
    let directory = tempfile::tempdir().unwrap();
    let project = directory.path().join("demo");
    let created = seedbox(&["init", "demo"], directory.path());
    assert!(created.status.success());
    assert_eq!(String::from_utf8_lossy(&created.stdout), "CREATED: demo\n");
    for args in [
        &["validate"][..],
        &["validate", "."],
        &["validate", "seedbox.yaml"],
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
    let cases: [(&[&str], &str); 3] = [
        (
            &["init", "."],
            "error: .: already exists; choose a new directory\n",
        ),
        (
            &["validate", "nope.yaml"],
            "error: project file nope.yaml: not found\n",
        ),
        (
            &["validate"],
            "error: no project file found; searched:\n  ./seedbox.yaml\n  ./seedbox.yml\n  ./compose.yaml\n  ./compose.yml\n  \
             ./docker-compose.yaml\n  ./docker-compose.yml\npass the file path directly\n",
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
    let output = seedbox(&["init", "x", "--template", "bogus"], directory.path());
    assert_eq!(output.status.code(), Some(2));
    assert!(stderr(&output).contains("possible values: etcd, redis"));
}

#[test]
fn execution_commands_keep_performance_settings_on_the_server() {
    let directory = tempfile::tempdir().unwrap();
    for command in ["run", "rerun", "replay", "explore"] {
        let output = seedbox(&["help", command], directory.path());
        assert!(output.status.success());
        let help = String::from_utf8(output.stdout).unwrap();
        for flag in [
            "--watchdog-ms",
            "--pv-watchdog-ms",
            "--machine-workers",
            "--margin",
            "--undo",
            "--timebase",
        ] {
            assert!(!help.contains(flag), "{command}: {help}");
            let output = seedbox(&[command, "abcd2345", flag, "20"], directory.path());
            assert_eq!(output.status.code(), Some(2));
            assert!(
                stderr(&output).contains(&format!("unexpected argument '{flag}'")),
                "{}",
                stderr(&output)
            );
        }
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
    let mut args = vec![
        "prepare".to_owned(),
        "demo".to_owned(),
        "--output".to_owned(),
        "prepared.json".to_owned(),
    ];
    for flag in ["kernel", "guest-init"] {
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
    assert_eq!(stderr(&output), format!("error: --kernel {}: not found; from the Seedbox checkout run `cargo xtask build-assets`, or pass --kernel PATH\n", kernel.display()));
    assert!(!directory.path().join(".seedbox/runs").exists());
}

#[cfg(target_os = "linux")]
#[test]
fn explicit_inputs_must_exist() {
    let (directory, mut args) = runtime_project();
    let init = args.iter().position(|arg| arg == "--guest-init").unwrap();
    args[init + 1] = "/nonexistent/compose-init".into();
    let args: Vec<_> = args.iter().map(String::as_str).collect();
    let output = seedbox(&args, directory.path());
    assert_eq!(
        stderr(&output),
        "error: --guest-init /nonexistent/compose-init: not found\n"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn explicit_missing_helpers_do_not_fall_back_to_embedded_assets() {
    let (directory, mut args) = runtime_project();
    let archive = directory
        .path()
        .join("assets")
        .join(std::env::consts::ARCH)
        .join("runtime.tar.gz");
    // Exercise a missing archive even when this binary embeds built assets.
    args.extend([
        "--runtime-assets".into(),
        archive.to_string_lossy().into_owned(),
    ]);
    let args: Vec<_> = args.iter().map(String::as_str).collect();
    let output = seedbox(&args, directory.path());
    assert_eq!(
        stderr(&output),
        format!("error: --runtime-assets {}: not found\n", archive.display())
    );
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
    for count in ["0", "4097", "nope"] {
        let output = seedbox(&["explore", "--count", count], &project);
        assert_eq!(output.status.code(), Some(2));
        assert!(stderr(&output).contains("--count"));
    }
    let overflow = seedbox(
        &["explore", "--seed", "18446744073709551615", "--count", "2"],
        &project,
    );
    assert_eq!(stderr(&overflow), "error: seed range overflows u64\n");
    for flag in ["--workers", "--cpu", "--runtime-assets", "--watchdog-ms"] {
        let output = seedbox(&["explore", flag, "1"], &project);
        assert_eq!(output.status.code(), Some(2));
        assert!(stderr(&output).contains("unexpected argument"));
    }
    let help = seedbox(&["help", "explore"], &project);
    assert!(help.status.success());
    let text = String::from_utf8_lossy(&help.stdout);
    assert!(text.contains("--count") && text.contains("--detach") && text.contains("--json"));
    assert!(text.contains("--paused"));
    assert!(!text.contains("--workers"));
    assert!(!project.join(".seedbox").exists());
}

#[test]
fn bundle_cli_validates_paths_limits_and_positional_inputs() {
    let directory = tempfile::tempdir().unwrap();
    for command in ["export", "import"] {
        let output = seedbox(&[command], directory.path());
        assert_eq!(output.status.code(), Some(2));
        assert!(stderr(&output).contains("required arguments"));
        let output = seedbox(&[command, "k7m2p9qx", "--max-bytes", "0"], directory.path());
        assert_eq!(output.status.code(), Some(2));
        assert!(stderr(&output).contains("--max-bytes"));
        let output = seedbox(
            &[command, "k7m2p9qx", "--max-bytes", "not-a-number"],
            directory.path(),
        );
        assert_eq!(output.status.code(), Some(2));
        assert!(stderr(&output).contains("--max-bytes"));
    }
    let output = seedbox(&["import", "missing"], directory.path());
    assert_eq!(
        stderr(&output),
        "error: bundle missing: not found; choose a readable regular bundle file\n"
    );
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

#[test]
fn doctor_help_ranges_and_missing_inputs_follow_cli_contract() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let help = seedbox(&["help", "doctor"], root);
    assert!(help.status.success());
    let text = String::from_utf8(help.stdout).unwrap();
    assert!(text.contains("Linux KVM host"));
    for flag in ["--kernel", "--json"] {
        assert!(text.contains(flag), "{text}");
    }
    for flag in [
        "--vmm",
        "--cpu",
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
    for json in [false, true] {
        let mut args = vec!["doctor", "--kernel", "missing"];
        if json {
            args.push("--json");
        }
        let output = seedbox(&args, root);
        assert_eq!(output.status.code(), Some(2));
        assert_eq!(stderr(&output), "error: --kernel missing: not found\n");
        assert!(output.stdout.is_empty());
    }
    let vmm = seedbox(&["doctor", "--vmm", "engine"], root);
    assert_eq!(vmm.status.code(), Some(2));
    assert!(stderr(&vmm).contains("--vmm"));
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
    assert_eq!(stderr(&output), format!("error: --kernel {}: not found; from the Seedbox checkout run `cargo xtask build-assets`, or pass --kernel PATH\n", kernel.display()));
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
        assert_eq!(stderr(&output), "error: --kernel kernel: not found\n");
        assert!(output.stdout.is_empty());
        assert!(
            !marker.exists(),
            "doctor invoked a runtime/build/preparation program"
        );
        assert!(!root.join("unwanted-state").exists());
        assert!(!root.join(".seedbox").exists());
        assert!(!root.join("output").exists());
    }
}

#[test]
fn logs_help_is_portable_and_has_no_runtime_options() {
    let tmp = tempfile::tempdir().unwrap();
    let out = seedbox(&["help", "logs"], tmp.path());
    assert!(out.status.success());
    let help = String::from_utf8_lossy(&out.stdout);
    assert!(help.contains("--since") && help.contains("--stream") && help.contains("--follow"));
    assert!(!help.contains("--kernel") && !help.contains("--state"));
}

#[test]
fn simulation_parameters_are_world_options_not_flags() {
    let directory = tempfile::tempdir().unwrap();
    for command in ["run", "prepare", "explore", "replay"] {
        let help = seedbox(&["help", command], directory.path());
        assert!(help.status.success());
        let help = String::from_utf8_lossy(&help.stdout);
        for flag in [
            &["--memory-mb", "2048"][..],
            &["--page-timer"],
            &["--deferred-tx"],
            &["--torn-writes"],
        ] {
            assert!(!help.contains(flag[0]), "{command} help lists {}", flag[0]);
            let mut args = vec![command, if command == "replay" { "abcd2345" } else { "." }];
            args.extend_from_slice(flag);
            let output = seedbox(&args, directory.path());
            assert_eq!(output.status.code(), Some(2));
            assert!(
                stderr(&output).contains(&format!("unexpected argument '{}'", flag[0])),
                "{}",
                stderr(&output)
            );
        }
    }
}

#[test]
fn run_keeps_server_and_execution_controls_out_of_submission_flags() {
    let directory = tempfile::tempdir().unwrap();
    let help = seedbox(&["help", "run"], directory.path());
    assert!(help.status.success());
    let help = String::from_utf8_lossy(&help.stdout);
    for flag in [
        "--checkpoint-at-ns",
        "--branch-seed",
        "--kernel",
        "--cpu",
        "--watchdog-ms",
    ] {
        assert!(!help.contains(flag));
        let result = seedbox(&["run", flag, "1"], directory.path());
        assert_eq!(result.status.code(), Some(2));
        assert!(stderr(&result).contains(&format!("unexpected argument '{flag}'")));
    }
}

#[test]
fn timebase_is_no_longer_a_public_mode() {
    let directory = tempfile::tempdir().unwrap();
    for command in ["run", "prepare", "explore", "doctor", "replay"] {
        let help = seedbox(&["help", command], directory.path());
        assert!(help.status.success());
        assert!(!String::from_utf8_lossy(&help.stdout).contains("--timebase"));
        for value in ["counter", "pv"] {
            let output = seedbox(&[command, "--timebase", value], directory.path());
            assert_eq!(output.status.code(), Some(2));
            assert!(stderr(&output).contains("unexpected argument '--timebase'"));
        }
    }
}

#[test]
fn internal_runtime_modes_are_not_authoring_options() {
    let directory = tempfile::tempdir().unwrap();
    for (group, option, value) in [
        ("machine", "page_timer", "true"),
        ("machine", "deferred_tx", "true"),
        ("schedule", "stop_tolerance", "1ms"),
    ] {
        fs::write(directory.path().join("compose.yaml"), format!("name: defaults\nservices:\n  app:\n    image: busybox:1.37.0\nx-seedbox:\n  version: 1\n  {group}: {{{option}: {value}}}\n  check: {{service: app}}\n")).unwrap();
        let output = seedbox(&["validate", "."], directory.path());
        assert_eq!(output.status.code(), Some(2));
        assert_eq!(
            stderr(&output),
            format!("error: $.x-seedbox.{group}.{option}: unsupported field\n")
        );
        assert!(!directory.path().join(".seedbox").exists());
    }
}

#[test]
fn runtime_assets_override_is_documented_and_missing_paths_fail_before_preparation() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    assert!(seedbox(&["init", "demo"], root).status.success());
    let project = root.join("demo");
    {
        let command = "prepare";
        let help = seedbox(&["help", command], root);
        assert!(help.status.success());
        let help = String::from_utf8_lossy(&help.stdout);
        assert!(help.contains("--runtime-assets"), "{help}");
        assert!(help.contains("SEEDBOX_RUNTIME_ASSETS"), "{help}");
        assert!(
            help.contains("embedded kernel, otherwise beside the executable"),
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
        .args(["prepare", "--output", "prepared.json"])
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
            "prepare",
            "--output",
            "prepared.json",
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
fn removed_preparation_flags_fail_before_preparation() {
    let directory = tempfile::tempdir().unwrap();
    fs::write(directory.path().join("compose.yaml"), "name: source-build\nservices:\n  client: {build: ./missing}\nx-seedbox:\n  version: 1\n  limits: {simulated_time: 1s, wall_time: 1m}\n  check: {service: client}\n").unwrap();
    for flag in ["--builder", "--build-epoch", "--wall-clock-epoch"] {
        let output = seedbox(&["run", flag, "value"], directory.path());
        assert!(stderr(&output).contains("unexpected argument"));
    }
}

#[test]
fn omitted_run_subcommand_accepts_the_same_compose_argument() {
    let directory = tempfile::tempdir().unwrap();
    let explicit = seedbox(&["run", "missing.yaml"], directory.path());
    let shorthand = seedbox(&["missing.yaml"], directory.path());
    assert_eq!(explicit.status.code(), Some(2));
    assert_eq!(explicit.status.code(), shorthand.status.code());
    assert_eq!(stderr(&explicit), stderr(&shorthand));
    assert_eq!(
        stderr(&shorthand),
        "error: project file missing.yaml: not found\n"
    );
}

#[test]
fn playback_validates_its_positional_id_before_starting_a_server() {
    let directory = tempfile::tempdir().unwrap();
    let output = seedbox(&["playback", "invalid", "--json"], directory.path());
    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    assert!(stderr(&output).contains("simulation ID must contain eight characters"));
}

#[test]
fn playback_mutations_reject_invalid_rates_and_request_ids_before_connecting() {
    let directory = tempfile::tempdir().unwrap();
    for (args, phrase) in [
        (vec!["rate", "k7m2p9qx", "1/0"], "rate must be"),
        (vec!["rate", "k7m2p9qx", "4294967296"], "rate must be"),
        (
            vec!["pause", "k7m2p9qx", "--request-id", "bad/id"],
            "request ID must be",
        ),
        (vec!["playback", "k7m2p9qx", "bad/id"], "request ID must be"),
    ] {
        let output = seedbox(&args, directory.path());
        assert_eq!(output.status.code(), Some(2));
        assert!(output.stdout.is_empty());
        assert!(stderr(&output).contains(phrase), "{}", stderr(&output));
    }
}

#[test]
fn observation_flags_are_validated_before_connecting() {
    let directory = tempfile::tempdir().unwrap();
    for (args, phrase) in [
        (
            vec!["wait", "k7m2p9qx", "--for", "bogus"],
            "unknown condition",
        ),
        (
            vec!["wait", "k7m2p9qx", "--for", "machine/n3=up"],
            "unknown machine condition",
        ),
        (
            vec!["wait", "k7m2p9qx", "--for", "fault/p1=done"],
            "unknown fault status",
        ),
        (vec!["wait", "k7m2p9qx"], "--for <CONDITION>"),
        (
            vec!["wait", "k7m2p9qx", "--for", "paused", "--timeout", "soon"],
            "--timeout",
        ),
        (vec!["logs", "k7m2p9qx", "--since", "soon"], "--since"),
        (vec!["logs", "k7m2p9qx", "--until", "later"], "--until"),
        (vec!["logs", "k7m2p9qx", "--tail", "many"], "--tail"),
        (
            vec!["logs", "k7m2p9qx", "--incarnation", "first"],
            "--incarnation",
        ),
        (
            vec!["get", "world", "invalid"],
            "simulation ID must contain",
        ),
        (
            vec!["get", "timeline", "invalid"],
            "simulation ID must contain",
        ),
        (
            vec!["get", "properties", "invalid"],
            "simulation ID must contain",
        ),
        (
            vec!["events", "invalid", "--follow"],
            "simulation ID must contain",
        ),
    ] {
        let output = seedbox(&args, directory.path());
        assert_eq!(output.status.code(), Some(2), "{args:?}");
        assert!(output.stdout.is_empty(), "{args:?}");
        assert!(
            stderr(&output).contains(phrase),
            "{args:?}: {}",
            stderr(&output)
        );
    }
}

#[test]
fn observation_commands_document_following_and_conditions() {
    let directory = tempfile::tempdir().unwrap();
    let logs = seedbox(&["help", "logs"], directory.path());
    let text = String::from_utf8_lossy(&logs.stdout);
    for flag in ["--follow", "--tail", "--since", "--until", "--incarnation"] {
        assert!(text.contains(flag), "logs help lacks {flag}:\n{text}");
    }
    let wait = seedbox(&["help", "wait"], directory.path());
    let text = String::from_utf8_lossy(&wait.stdout);
    for phrase in ["machine/NAME=CONDITION", "fault/ID-OR-HANDLE=STATUS", "124"] {
        assert!(text.contains(phrase), "wait help lacks {phrase}:\n{text}");
    }
}

#[test]
fn fault_input_errors_are_reported_before_connecting() {
    let directory = tempfile::tempdir().unwrap();
    let cwd = directory.path();
    let missing = seedbox(&["inject", "k7m2p9qx", "missing.yaml"], cwd);
    assert_eq!(missing.status.code(), Some(2));
    assert!(missing.stdout.is_empty());
    assert_eq!(
        stderr(&missing),
        "error: fault missing.yaml: No such file or directory (os error 2)\n"
    );
    fs::write(cwd.join("invalid.yaml"), "action: nonexistent\n").unwrap();
    let invalid = seedbox(&["inject", "k7m2p9qx", "invalid.yaml"], cwd);
    assert_eq!(invalid.status.code(), Some(2));
    assert!(invalid.stdout.is_empty());
    assert!(stderr(&invalid)
        .contains("error: fault invalid.yaml: action: unknown variant `nonexistent`"));
    fs::write(cwd.join("empty.yaml"), "").unwrap();
    let empty = seedbox(&["inject", "k7m2p9qx", "empty.yaml"], cwd);
    assert_eq!(empty.status.code(), Some(2));
    assert!(empty.stdout.is_empty());
    assert!(
        stderr(&empty).starts_with("error: fault empty.yaml: "),
        "{}",
        stderr(&empty)
    );
    fs::write(cwd.join("large.yaml"), vec![b' '; 65537]).unwrap();
    let large = seedbox(&["inject", "k7m2p9qx", "large.yaml"], cwd);
    assert_eq!(large.status.code(), Some(2));
    assert_eq!(
        stderr(&large),
        "error: fault large.yaml: exceeds 65536 bytes\n"
    );
}

#[test]
fn offline_artifact_and_server_state_use_distinct_environment_variables() {
    let directory = tempfile::tempdir().unwrap();
    let output = seedbox(&["help", "prepare"], directory.path());
    assert!(output.status.success());
    let help = String::from_utf8_lossy(&output.stdout);
    assert!(help.contains("SEEDBOX_ARTIFACT_STATE"), "{help}");
    assert!(!help.contains("SEEDBOX_STATE="), "{help}");
    let output = seedbox(&["help", "serve"], directory.path());
    assert!(String::from_utf8_lossy(&output.stdout).contains("SEEDBOX_STATE"));
}

#[test]
fn completion_is_silent_without_a_server_and_never_starts_one() {
    let root = tempfile::tempdir().unwrap();
    let result = seedbox(&["__complete", "--", "logs", ""], root.path());
    assert!(result.status.success());
    assert!(result.stdout.is_empty());
    assert!(result.stderr.is_empty());
    assert!(!root
        .path()
        .join(".local/state/seedbox/server.sock")
        .exists());
    let result = seedbox(&["__complete", "--", "lo"], root.path());
    assert!(result.status.success());
    assert!(String::from_utf8_lossy(&result.stdout)
        .lines()
        .any(|line| line == "logs"));
    for shell in ["bash", "zsh", "fish"] {
        let result = seedbox(&["completions", shell], root.path());
        assert!(result.status.success());
        assert!(String::from_utf8_lossy(&result.stdout).contains("__complete"));
    }
}

#[cfg(unix)]
#[test]
fn tui_requires_an_interactive_terminal_before_starting_a_server() {
    let directory = tempfile::tempdir().unwrap();
    let output = seedbox(&["tui"], directory.path());
    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    assert_eq!(
        stderr(&output),
        "error: seedbox tui needs an interactive terminal; use seedbox events or seedbox get from scripts\n"
    );
    assert!(!directory.path().join(".local/state/seedbox").exists());
}

#[test]
fn native_project_discovery_prefers_seedbox_yaml_and_validates_oracle() {
    let directory = tempfile::tempdir().unwrap();
    fs::write(
        directory.path().join("compose.yaml"),
        "invalid obsolete input",
    )
    .unwrap();
    fs::write(directory.path().join("seedbox.yaml"), "name: native\nmachines:\n  app: {image: app}\noracle: {image: checker}\nnetwork: {latency: 2ms}\n").unwrap();
    let output = seedbox(&["validate", ".", "--json"], directory.path());
    assert!(output.status.success(), "{}", stderr(&output));
    let output = seedbox(&["run", ".", "--wall-time", "0s"], directory.path());
    assert_eq!(output.status.code(), Some(2));
    assert!(stderr(&output).contains("--wall-time"));
    assert!(stderr(&output).contains("duration must be positive"));
}

#[test]
fn volume_sizes_validate_with_errors_that_name_the_field() {
    let directory = tempfile::tempdir().unwrap();
    let project = |volumes: &str| {
        fs::write(
            directory.path().join("seedbox.yaml"),
            format!("name: sized\nmachines:\n  db:\n    image: app\n    volumes:\n{volumes}oracle: {{image: checker}}\n"),
        )
        .unwrap();
        seedbox(&["validate"], directory.path())
    };
    let output = project("      data: {target: /data, size: 4GiB}\n      wal: {target: /wal}\n");
    assert!(output.status.success(), "{}", stderr(&output));
    for (size, column) in [("4GB", 35), ("4096", 35), ("1.5GiB", 35), ("128GiB", 35)] {
        let output = project(&format!("      data: {{target: /data, size: {size}}}\n"));
        assert_eq!(output.status.code(), Some(2), "{size}");
        assert_eq!(
            stderr(&output),
            format!("error: machines.db.volumes.data.size: size \"{size}\": expected a whole number of MiB or GiB such as 4GiB or 512MiB, from 16MiB to 64GiB at line 6 column {column}\n"),
        );
    }
    let output = project(
        "      data: {target: /data, size: 40GiB}\n      wal: {target: /wal, size: 30GiB}\n",
    );
    assert_eq!(output.status.code(), Some(2));
    assert_eq!(
        stderr(&output),
        "error: $.machines.db.volumes: volume sizes total 70GiB, above the 64GiB limit for one machine\n"
    );
}

#[test]
fn progress_client_is_optional_and_validated() {
    let directory = tempfile::tempdir().unwrap();
    let project = |engine: &str| {
        fs::write(
            directory.path().join("seedbox.yaml"),
            format!(
                "name: engine
oracle: {{image: checker}}
{engine}"
            ),
        )
        .unwrap();
        seedbox(&["validate"], directory.path())
    };
    for engine in [
        "",
        "engine: {}\n",
        "engine: {progressClient: standard}\n",
        "engine: {progressClient: lean}\n",
    ] {
        let output = project(engine);
        assert!(output.status.success(), "{engine}: {}", stderr(&output));
    }
    let output = project("engine: {progressClient: fast}\n");
    assert_eq!(output.status.code(), Some(2));
    assert_eq!(
        stderr(&output),
        "error: engine.progressClient: unknown variant `fast`, expected `standard` or `lean` at line 3 column 26\n"
    );
    let output = project("engine: {progress: lean}\n");
    assert_eq!(output.status.code(), Some(2));
    assert!(
        stderr(&output).contains("unknown field `progress`, expected `progressClient`"),
        "{}",
        stderr(&output)
    );
}

#[test]
fn capacity_help_explains_resident_slots_and_accepts_json() {
    let directory = tempfile::tempdir().unwrap();
    let output = seedbox(&["get", "capacity", "--help"], directory.path());
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("resident execution slots"));
    assert!(stdout.contains("--json"));
    let output = seedbox(&["get", "capacity", "--unexpected"], directory.path());
    assert_eq!(output.status.code(), Some(2));
    assert!(stderr(&output).contains("--unexpected"));
}

#[test]
fn bundle_and_diagnostics_help_errors_and_removed_commands() {
    let root = tempfile::tempdir().unwrap();
    for command in ["export", "import", "diagnostics"] {
        let help = seedbox(&["help", command], root.path());
        assert!(help.status.success());
        let text = String::from_utf8_lossy(&help.stdout);
        assert!(text.contains("--max-bytes") && text.contains("--json"));
        assert!(!text.contains("--state") && !text.contains("--kernel"));
        let over = seedbox(
            &[command, "k7m2p9qx", "--max-bytes", "68719476737"],
            root.path(),
        );
        assert_eq!(over.status.code(), Some(2));
        assert!(stderr(&over).contains("--max-bytes"));
    }
    let help = seedbox(&["help", "export"], root.path());
    assert!(String::from_utf8_lossy(&help.stdout).contains("SIMULATION-ID.seedbox.tar"));
    for command in [
        "inspect",
        "diff",
        "retained-pause",
        "retained-resume",
        "retained-logs",
    ] {
        let output = seedbox(&["help", command], root.path());
        assert_eq!(output.status.code(), Some(2));
        assert!(stderr(&output).contains("unrecognized subcommand"));
    }
    fs::write(root.path().join("k7m2p9qx.seedbox.tar"), b"existing").unwrap();
    let existing = seedbox(&["export", "k7m2p9qx"], root.path());
    assert_eq!(stderr(&existing), "error: --output k7m2p9qx.seedbox.tar: already exists; choose a new file under real directories\n");
    let parent = seedbox(&["export", "k7m2p9qx", "-o", "missing/out"], root.path());
    assert_eq!(stderr(&parent), "error: --output missing/out: No such file or directory (os error 2); choose a new file under real directories\n");
    fs::write(root.path().join("large"), b"12345").unwrap();
    let large = seedbox(&["import", "large", "--max-bytes", "4"], root.path());
    assert_eq!(
        stderr(&large),
        "error: bundle large: exceeds --max-bytes 4\n"
    );
    let directory = seedbox(&["import", "."], root.path());
    assert_eq!(stderr(&directory), "error: bundle .: not a regular file\n");
}

#[test]
fn transfer_symlinks_are_rejected_and_compare_documents_exit_codes() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    fs::write(root.join("source"), b"bundle").unwrap();
    std::os::unix::fs::symlink("source", root.join("link")).unwrap();
    let import = seedbox(&["import", "link"], root);
    assert_eq!(
        stderr(&import),
        format!(
            "error: bundle link: {}; choose a readable regular bundle file\n",
            std::io::Error::from_raw_os_error(libc::ELOOP)
        )
    );
    std::os::unix::fs::symlink("missing", root.join("destination")).unwrap();
    let export = seedbox(&["export", "k7m2p9qx", "-o", "destination"], root);
    assert_eq!(
        stderr(&export),
        "error: --output destination: already exists; choose a new file under real directories\n"
    );
    std::os::unix::fs::symlink(".", root.join("parent")).unwrap();
    let parent = seedbox(&["export", "k7m2p9qx", "-o", "parent/output"], root);
    assert_eq!(parent.status.code(), Some(2));
    assert!(stderr(&parent).starts_with("error: --output parent/output:"));
    assert!(!root.join("output").exists());
    let help = seedbox(&["help", "compare"], root);
    assert!(String::from_utf8_lossy(&help.stdout).contains("Exit status:"));
    assert!(String::from_utf8_lossy(&help.stdout).contains("NOT_COMPARABLE"));
}

/// Answer one GET whose request line starts with `path` on the default server
/// socket, as a stand-in for the server.
fn stand_in_server(state: &Path, path: &'static str, body: String) {
    use std::io::{Read, Write};
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(state)
        .unwrap();
    let listener = std::os::unix::net::UnixListener::bind(state.join("server.sock")).unwrap();
    std::thread::spawn(move || {
        for mut stream in listener.incoming().flatten() {
            let mut request = Vec::new();
            let mut buffer = [0; 1024];
            while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                match stream.read(&mut buffer) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => request.extend_from_slice(&buffer[..n]),
                }
            }
            if !request.starts_with(format!("GET {path} ").as_bytes()) {
                continue;
            }
            let _ = write!(
                stream,
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            return;
        }
    });
}

fn out_of_memory_kill(incarnation: u32) -> seedbox_api::OutOfMemoryKill {
    seedbox_api::OutOfMemoryKill {
        incarnation,
        simulated_time_ns: 122_954_950_231,
        process: Some(seedbox_api::OutOfMemoryProcess {
            pid: 412,
            command: "nats-server".into(),
            total_vm_kib: 1_203_328,
            anon_rss_kib: 446_228,
            file_rss_kib: 1024,
            shmem_rss_kib: 0,
        }),
    }
}

#[test]
fn get_world_prints_machine_and_link_tables() {
    use seedbox_api::{
        LinkCounters, LinkPolicy, LinkState, MachineCondition, MachineState, WorldState,
    };
    let directory = tempfile::tempdir().unwrap();
    let state = directory.path().join(".local/state/seedbox");
    let machine = |name: &str, oracle| MachineState {
        name: name.into(),
        incarnation: 0,
        condition: MachineCondition::Ready,
        exit_code: None,
        cpu_speed_hz: None,
        throttle_speed_hz: None,
        oracle,
        last_transition: None,
        last_transition_at_ns: None,
        condition_since_ns: None,
        application: Default::default(),
        out_of_memory_kills: Vec::new(),
    };
    // n2 exited after the guest kernel killed its application for memory;
    // n3's kill was in an earlier incarnation, so its exit is unexplained.
    let exited = |name: &str, incarnation, killed| MachineState {
        incarnation,
        condition: MachineCondition::Exited,
        exit_code: Some(137),
        out_of_memory_kills: vec![out_of_memory_kill(killed)],
        ..machine(name, false)
    };
    let link = |from: &str, to: &str, cut: Option<&str>| LinkState {
        from: from.into(),
        to: to.into(),
        policy: LinkPolicy {
            latency_ns: 2_500_000,
            jitter_ns: 500_000,
            loss_ppm: 100,
            bandwidth_bps: None,
        },
        partitioned_by: cut.map(Into::into),
        shaped_by: None,
        counters: LinkCounters {
            scheduled: 12,
            partition_dropped: 3,
            loss_dropped: 1,
            bytes: 960,
            ..Default::default()
        },
    };
    let world = WorldState {
        choice_inputs: Default::default(),
        triggers: Default::default(),
        override_rejections: Default::default(),
        simulation: "k7m2p9qx".parse().unwrap(),
        revision: 3,
        simulated_time_ns: 1_000_000_000,
        machines: vec![
            machine("n1", false),
            exited("n2", 0, 0),
            exited("n3", 1, 0),
            machine("oracle", true),
        ],
        links: vec![link("n1", "oracle", None), link("oracle", "n1", Some("p1"))],
        faults: Vec::new(),
    };
    stand_in_server(
        &state,
        "/v1/simulations/k7m2p9qx/world",
        serde_json::to_string(&world).unwrap(),
    );
    let output = seedbox(&["get", "world", "k7m2p9qx"], directory.path());
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        "\
MACHINE  CONDITION  INCARNATION  EXIT                 ORACLE
n1       ready      0            -                    -
n2       exited     0            137 (out of memory)  -
n3       exited     1            137                  -
oracle   ready      0            -                    yes

FROM    TO      LATENCY  JITTER  LOSS    FRAMES  BYTES  DROPPED  FAULT
n1      oracle  2.5ms    500µs   100ppm  12      960    4        -
oracle  n1      2.5ms    500µs   100ppm  12      960    4        cut p1

simulated time: 1s
"
    );
}

#[test]
fn fault_profiles_are_checked_before_connecting() {
    let directory = tempfile::tempdir().unwrap();
    let missing = directory.path().join("missing.yaml");
    let output = seedbox(
        &["run", "--fault-profile", missing.to_str().unwrap()],
        directory.path(),
    );
    assert_eq!(output.status.code(), Some(2));
    assert_eq!(
        stderr(&output),
        format!("error: --fault-profile {}: not found\n", missing.display())
    );
    let invalid = directory.path().join("profile.yaml");
    std::fs::write(&invalid, "end: 1s\ninterval: 1s\nfaults: []\n").unwrap();
    for command in ["run", "explore"] {
        let output = seedbox(
            &[command, "--fault-profile", invalid.to_str().unwrap()],
            directory.path(),
        );
        assert_eq!(output.status.code(), Some(2), "{command}");
        assert_eq!(
            stderr(&output),
            format!(
                "error: --fault-profile {}: fault profile needs at least one fault kind with positive weight\n",
                invalid.display()
            ),
            "{command}"
        );
    }
}

#[test]
fn campaign_shrink_and_findings_bounds_are_usage_errors_before_any_server() {
    let directory = tempfile::tempdir().unwrap();
    let project = directory.path();
    for (args, phrase) in [
        (vec!["campaign", "--budget", "0"], "--budget"),
        (vec!["campaign", "--batch", "65"], "--batch"),
        (vec!["campaign", "--fresh-share", "1001"], "--fresh-share"),
        (vec!["campaign", "--corpus", "0"], "--corpus"),
        (vec!["campaign", "--anchor", "soon"], "--anchor"),
        (
            vec!["shrink", "k7m2p9qx", "--max-probes", "0"],
            "--max-probes",
        ),
        (vec!["shrink", "not-an-id"], "ID"),
        (vec!["shrink", "k7m2p9qx", "--seeds", "0"], "--seeds"),
        (vec!["shrink", "k7m2p9qx", "--seeds", "9"], "--seeds"),
        (
            vec!["shrink", "k7m2p9qx", "--from-checkpoint=yes"],
            "--from-checkpoint",
        ),
        (
            vec!["get", "findings", "campaign-1", "--limit", "101"],
            "--limit",
        ),
        (
            vec!["get", "findings", "campaign-1", "--within", "x"],
            "--within",
        ),
        (vec!["stop", "nonsense!"], "ID"),
        (vec!["explain", "k7m2p9qx", "--kind", "gossip"], "--kind"),
        (vec!["explain", "not-an-id"], "ID"),
    ] {
        let output = seedbox(&args, project);
        assert_eq!(output.status.code(), Some(2), "{args:?}");
        assert!(
            stderr(&output).contains(phrase),
            "{args:?}: {}",
            stderr(&output)
        );
    }
    let help = seedbox(&["help", "campaign"], project);
    let help = String::from_utf8_lossy(&help.stdout).into_owned();
    assert!(
        help.contains("--budget") && help.contains("Exit status"),
        "{help}"
    );
    let help = seedbox(&["help", "serve"], project);
    assert!(String::from_utf8_lossy(&help.stdout).contains("Performance (never changes results)"));
}

#[test]
fn diagnostics_lists_out_of_memory_kills_after_retained_files() {
    let directory = tempfile::tempdir().unwrap();
    let state = directory.path().join(".local/state/seedbox");
    let lost = seedbox_api::OutOfMemoryKill {
        simulated_time_ns: 130_000_000_000,
        process: None,
        ..out_of_memory_kill(1)
    };
    let report = seedbox_api::DiagnosticFiles {
        files: vec![seedbox_api::DiagnosticFile {
            name: "vmm.json".into(),
            sha256: "ab".repeat(32),
            size_bytes: 29414,
        }],
        omissions: Default::default(),
        out_of_memory_kills: vec![
            seedbox_api::MachineOutOfMemoryKill {
                machine: "n3".into(),
                memory_mib: Some(512),
                kill: out_of_memory_kill(0),
            },
            seedbox_api::MachineOutOfMemoryKill {
                machine: "n3".into(),
                memory_mib: Some(512),
                kill: lost,
            },
        ],
    };
    stand_in_server(
        &state,
        "/v1/simulations/k7m2p9qx/diagnostics",
        serde_json::to_string(&report).unwrap(),
    );
    let output = seedbox(&["diagnostics", "k7m2p9qx"], directory.path());
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        format!(
            "\
NAME      BYTES  SHA256
vmm.json  29414  {}
out-of-memory: machine n3 incarnation 0 at 122.955s: the guest kernel's out-of-memory killer killed nats-server (pid 412, anon-rss 446228 kB); raise memoryMiB (now 512)
out-of-memory: machine n3 incarnation 1 at 130s: the guest kernel's out-of-memory killer killed a process whose kernel log record was lost; raise memoryMiB (now 512)
",
            "ab".repeat(32)
        )
    );
}
