use seedbox_test::process::Guarded;
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
        .guarded_output()
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
        let Ok(info) = runtime.block_on(client.info()) else {
            return;
        };
        // SAFETY: this server answered on the socket inside this test's private HOME.
        unsafe {
            libc::kill(info.process_id as i32, libc::SIGTERM);
        }
        seedbox_test_time::HangGuard::new(
            "private CLI server closes its listener after SIGTERM",
            std::time::Duration::from_secs(30),
        )
        .wait_for(|| {
            std::os::unix::net::UnixStream::connect(&self.0)
                .is_err()
                .then_some(())
        })
        .unwrap();
    }
}

fn seedbox(args: &[&str], cwd: &Path) -> Output {
    seedbox_with(args, cwd, None)
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[path = "support/backend_authority.rs"]
mod backend_authority;

#[cfg(unix)]
#[test]
fn long_server_state_path_fails_before_creating_state() {
    let directory = tempfile::tempdir().unwrap();
    let state = directory.path().join("s".repeat(200));
    let output = seedbox(
        &[
            "server",
            "start",
            "--foreground",
            "--state",
            state.to_str().unwrap(),
        ],
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
fn explicit_asset_directory_overrides_bundled_driver_without_build_tools() {
    // Hard-link instead of copying: a copy holds a writable descriptor that a
    // concurrently forked test child can inherit, making exec fail with ETXTBSY.
    // CARGO_TARGET_TMPDIR shares the binary's filesystem, so linking works.
    let directory = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let root = directory.path().canonicalize().unwrap();
    fs::write(
        root.join("seedbox.yaml"),
        "name: test\noracle: {image: busybox}\n",
    )
    .unwrap();
    let binary = root.join("seedbox");
    fs::hard_link(env!("CARGO_BIN_EXE_seedbox"), &binary).unwrap();
    let output = Command::new(&binary)
        .arg("check")
        .current_dir(&root)
        .env_clear()
        .env("HOME", &root)
        .env("SEEDBOX_ASSET_DIR", &root)
        .env("PATH", "")
        .guarded_output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    let archive = root
        .join(std::env::consts::ARCH)
        .join("driver-runtime.tar.gz");
    assert_eq!(stderr(&output), format!("error: driver runtime {}: {}; run cargo xtask build-assets or supply a complete runtime archive\n", archive.display(), std::io::Error::from_raw_os_error(2)));
    assert!(output.stdout.is_empty());
    // Relocation still wins; retired runtime environment variables are ignored.
    let expected = stderr(&output);
    for variable in [
        "SEEDBOX_KERNEL",
        "SEEDBOX_VMM",
        "SEEDBOX_RUNTIME_ASSETS",
        "SEEDBOX_GUEST_INIT",
        "SEEDBOX_MKE2FS",
    ] {
        let output = Command::new(&binary)
            .arg("check")
            .current_dir(&root)
            .env_clear()
            .env("HOME", &root)
            .env("SEEDBOX_ASSET_DIR", &root)
            .env("PATH", "")
            .env(variable, "missing-retired-input")
            .guarded_output()
            .unwrap();
        assert_eq!(output.status.code(), Some(2));
        assert_eq!(
            stderr(&output),
            expected,
            "{variable} must not affect runtime lookup"
        );
        assert!(output.stdout.is_empty());
    }
    assert!(!root.join(".seedbox").exists());
    assert!(!root.join(".local/state/seedbox").exists());
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
    assert!(
        help.contains("seedbox [RUN OPTIONS] [PROJECT | CHECKPOINT]"),
        "{help}"
    );
    assert!(!help.contains("__engine"), "{help}");

    let run_help = seedbox(&["help", "run"], directory.path());
    assert!(run_help.status.success());
    let help = String::from_utf8_lossy(&run_help.stdout);
    assert!(help.contains("--detach"), "{help}");
    assert!(help.contains("--paused"), "{help}");
    assert!(help.contains("--seed"), "{help}");
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
fn driver_worker_requires_a_request_without_state_or_external_tools() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    let tools = root.join("tools");
    fs::create_dir(&tools).unwrap();
    let marker = root.join("tool-called");
    let cargo = tools.join("cargo");
    seedbox_test::process::install_executable(
        &cargo,
        format!(
            "#!/bin/sh\necho invoked >> '{}'\nexit 0\n",
            marker.display()
        )
        .as_bytes(),
        0o755,
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_seedbox"))
        .args(["__engine", "driver-world-v1"])
        .current_dir(root)
        .env_clear()
        .env("HOME", root)
        .env("SEEDBOX_ASSET_DIR", root)
        .env("PATH", &tools)
        .env("CARGO", &cargo)
        .env("CARGO_MANIFEST_DIR", env!("CARGO_MANIFEST_DIR"))
        .guarded_output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2), "{}", stderr(&output));
    assert!(output.stdout.is_empty());
    assert!(
        stderr(&output).contains("driver-world-v1 needs exactly --request PATH"),
        "{}",
        stderr(&output)
    );
    assert!(!marker.exists(), "worker invoked external cargo");
    assert!(!root.join(".seedbox").exists());
}

#[test]
fn check_validates_project_before_host_readiness() {
    let directory = tempfile::tempdir().unwrap();
    let source = directory.path().join("seedbox.yaml");
    let yaml = "name: cli-test\noracle: {image: example/workload:v1}\n";
    fs::write(&source, yaml).unwrap();
    let success = seedbox(&["check", "seedbox.yaml", "--json"], directory.path());
    assert_eq!(success.status.code(), Some(2));
    assert!(
        stderr(&success).contains("driver runtime"),
        "{}",
        stderr(&success)
    );
    assert!(success.stdout.is_empty());
    fs::write(
        &source,
        yaml.replace(
            "image: example/workload:v1",
            "image: example/workload:v1, volumes: [./host:/data]",
        ),
    )
    .unwrap();
    let failure = seedbox(&["check", "seedbox.yaml"], directory.path());
    assert_eq!(failure.status.code(), Some(2));
    assert!(stderr(&failure).contains("invalid type: sequence"));
    assert!(!directory.path().join("runs").exists());
}

#[test]
fn project_argument_defaults_to_the_project_directory() {
    let directory = tempfile::tempdir().unwrap();
    let project = directory.path().join("demo");
    let created = seedbox(&["init", "demo"], directory.path());
    assert!(created.status.success());
    assert_eq!(String::from_utf8_lossy(&created.stdout), "CREATED: demo\n");
    let redis = seedbox(&["init", "redis", "--template", "redis"], directory.path());
    assert!(redis.status.success());
    for name in ["demo", "redis"] {
        let document: serde_yaml_ng::Value = serde_yaml_ng::from_str(
            &fs::read_to_string(directory.path().join(name).join("seedbox.yaml")).unwrap(),
        )
        .unwrap();
        assert_eq!(document["simulator"]["quantum"].as_str(), Some("1ms"));
        assert_eq!(document["simulator"]["machineWorkers"].as_u64(), Some(1));
    }
    for args in [&["check"][..], &["check", "."], &["check", "seedbox.yaml"]] {
        let output = seedbox(args, &project);
        assert_eq!(output.status.code(), Some(2), "{args:?}");
        assert!(
            stderr(&output).contains("driver runtime"),
            "{args:?}: {}",
            stderr(&output)
        );
    }
    let output = seedbox(&["check", "demo"], directory.path());
    assert!(stderr(&output).contains("driver runtime"));
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
            &["check", "nope.yaml"],
            "error: project file nope.yaml: not found\n",
        ),
        (
            &["check"],
            "error: no project file found; searched:\n  ./seedbox.yaml\n  ./seedbox.yml\npass the file path directly\n",
        ),
    ];
    for (args, expected) in cases {
        let output = seedbox(args, cwd);
        assert_eq!(output.status.code(), Some(2), "{args:?}");
        assert_eq!(stderr(&output), expected, "{args:?}");
    }
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
    for command in ["run", "replay"] {
        let output = seedbox(&["help", command], directory.path());
        assert!(output.status.success());
        let help = String::from_utf8(output.stdout).unwrap();
        for flag in [
            "--watchdog-ms",
            "--pv-watchdog-ms",
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

#[test]
fn legacy_runtime_flags_are_usage_errors_before_any_state_or_preparation() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    fs::write(root.join("existing-input"), b"not a runtime override").unwrap();
    for command in ["check", "run", "replay"] {
        let help = seedbox(&["help", command], root);
        assert!(help.status.success());
        let help = String::from_utf8_lossy(&help.stdout);
        for (flag, variable) in [
            ("--vmm", "SEEDBOX_VMM"),
            ("--kernel", "SEEDBOX_KERNEL"),
            ("--runtime-assets", "SEEDBOX_RUNTIME_ASSETS"),
            ("--guest-init", "SEEDBOX_GUEST_INIT"),
            ("--mke2fs", "SEEDBOX_MKE2FS"),
        ] {
            assert!(!help.contains(flag), "{command}: {help}");
            assert!(!help.contains(variable), "{command}: {help}");
            for path in ["existing-input", "missing-input"] {
                let mut args = vec![command];
                args.extend([flag, path]);
                let output = seedbox(&args, root);
                assert_eq!(output.status.code(), Some(2), "{command} {flag}");
                assert!(output.stdout.is_empty());
                assert!(
                    stderr(&output).contains(&format!("unexpected argument '{flag}'")),
                    "{}",
                    stderr(&output)
                );
            }
        }
    }
    assert!(!root.join("prepared.json").exists());
    assert!(!root.join(".local/state/seedbox").exists());
    assert!(!root.join(".seedbox").exists());
}

#[test]
fn seed_range_bounds_and_help_are_public_before_runtime_inputs_or_state() {
    let directory = tempfile::tempdir().unwrap();
    assert!(seedbox(&["init", "demo"], directory.path())
        .status
        .success());
    let project = directory.path().join("demo");
    for count in ["0", "4097", "nope"] {
        let output = seedbox(&["run", "--count", count], &project);
        assert_eq!(output.status.code(), Some(2));
        assert!(stderr(&output).contains("--count"));
    }
    let overflow = seedbox(
        &["run", "--seed", "18446744073709551615", "--count", "2"],
        &project,
    );
    assert_eq!(stderr(&overflow), "error: seed range overflows u64\n");
    for flag in ["--workers", "--cpu", "--runtime-assets", "--watchdog-ms"] {
        let output = seedbox(&["run", flag, "1"], &project);
        assert_eq!(output.status.code(), Some(2));
        assert!(stderr(&output).contains("unexpected argument"));
    }
    let help = seedbox(&["help", "run"], &project);
    assert!(help.status.success());
    let text = String::from_utf8_lossy(&help.stdout);
    assert!(text.contains("--count") && text.contains("--detach") && text.contains("--json"));
    assert!(text.contains("--paused"));
    assert!(!text.contains("--workers"));
    assert!(!project.join(".seedbox").exists());
}

#[test]
fn validation_rejects_obsolete_compose_without_creating_state() {
    let directory = tempfile::tempdir().unwrap();
    fs::write(
        directory.path().join("compose.yaml"),
        "services: {app: {image: example/app}}\n",
    )
    .unwrap();
    let undiscovered = seedbox(&["check"], directory.path());
    assert_eq!(undiscovered.status.code(), Some(2));
    assert!(stderr(&undiscovered).contains("no project file found"));
    let explicit = seedbox(&["check", "compose.yaml"], directory.path());
    assert_eq!(explicit.status.code(), Some(2));
    assert!(stderr(&explicit).contains("unknown field `services`"));
    assert!(!directory.path().join(".seedbox").exists());
}

#[test]
fn check_help_ranges_and_missing_inputs_follow_cli_contract() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let help = seedbox(&["help", "check"], root);
    assert!(help.status.success());
    let text = String::from_utf8(help.stdout).unwrap();
    assert!(text.contains("host"));
    assert!(text.contains("--json"), "{text}");
    for flag in [
        "--kernel",
        "--vmm",
        "--runtime-assets",
        "--mke2fs",
        "--cpu",
        "--builder",
        "--undo",
        "--memory-mb",
        "--guest-init",
    ] {
        assert!(!text.contains(flag), "{text}");
    }
    assert!(text.contains("--json"));
    let invalid_timebase = seedbox(&["check", "--timebase", "invalid"], root);
    assert_eq!(invalid_timebase.status.code(), Some(2));
    assert!(stderr(&invalid_timebase).contains("--timebase"));
    assert!(!root.join(".seedbox").exists());
}

#[test]
fn check_reports_missing_driver_inputs_without_downloads() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    fs::write(
        root.join("seedbox.yaml"),
        "name: test\noracle: {image: busybox}\n",
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_seedbox"))
        .args(["check", "--json"])
        .current_dir(root)
        .env_clear()
        .env("HOME", root)
        .env("SEEDBOX_ASSET_DIR", root)
        .env("PATH", "")
        .env("GH_TOKEN", "invalid\ncredential")
        .guarded_output()
        .unwrap();
    let archive = root
        .join(std::env::consts::ARCH)
        .join("driver-runtime.tar.gz");
    assert_eq!(output.status.code(), Some(2));
    assert_eq!(stderr(&output), format!("error: driver runtime {}: {}; run cargo xtask build-assets or supply a complete runtime archive\n", archive.display(), std::io::Error::from_raw_os_error(2)));
    assert!(output.stdout.is_empty());
    assert!(!root.join(".seedbox").exists());
    assert!(!root.join(".local/state/seedbox").exists());
}

#[test]
fn check_ignored_legacy_environment_executes_no_tools_and_creates_no_state() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::write(
        root.join("seedbox.yaml"),
        "name: test\noracle: {image: busybox}\n",
    )
    .unwrap();
    let tools = root.join("tools");
    fs::create_dir(&tools).unwrap();
    let marker = root.join("tool-called");
    for name in ["cargo", "podman", "skopeo", "umoci"] {
        let tool = tools.join(name);
        seedbox_test::process::install_executable(
            &tool,
            format!(
                "#!/bin/sh\necho invoked >> '{}'\nexit 0\n",
                marker.display()
            )
            .as_bytes(),
            0o755,
        )
        .unwrap();
    }
    for json in [false, true] {
        let mut command = Command::new(env!("CARGO_BIN_EXE_seedbox"));
        command.arg("check");
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
            .env("SEEDBOX_ASSET_DIR", root)
            .envs([
                ("SEEDBOX_VMM", "missing-vmm"),
                ("SEEDBOX_KERNEL", "missing-kernel"),
                ("SEEDBOX_RUNTIME_ASSETS", "missing-archive"),
                ("SEEDBOX_GUEST_INIT", "missing-init"),
                ("SEEDBOX_MKE2FS", "missing-formatter"),
            ])
            .guarded_output()
            .unwrap();
        assert_eq!(output.status.code(), Some(2), "{}", stderr(&output));
        let archive = root
            .join(std::env::consts::ARCH)
            .join("driver-runtime.tar.gz");
        assert_eq!(stderr(&output), format!("error: driver runtime {}: {}; run cargo xtask build-assets or supply a complete runtime archive\n", archive.display(), std::io::Error::from_raw_os_error(2)));
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

#[cfg(not(target_os = "linux"))]
#[test]
fn check_removed_override_is_rejected_before_host_admission() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("engine"), b"dev engine").unwrap();
    fs::write(dir.path().join("kernel"), b"dev kernel").unwrap();
    for json in [false, true] {
        let mut args = vec!["check", "--vmm", "engine", "--kernel", "kernel"];
        if json {
            args.push("--json");
        }
        let output = seedbox(&args, dir.path());
        assert_eq!(output.status.code(), Some(2));
        assert!(stderr(&output).contains("unexpected argument '--vmm'"));
        assert!(output.stdout.is_empty());
        assert!(!dir.path().join(".seedbox").exists());
    }
}

#[test]
fn logs_help_is_portable_and_has_shared_server_options() {
    let tmp = tempfile::tempdir().unwrap();
    let out = seedbox(&["help", "logs"], tmp.path());
    assert!(out.status.success());
    let help = String::from_utf8_lossy(&out.stdout);
    assert!(help.contains("--since") && help.contains("--stream") && help.contains("--follow"));
    assert!(!help.contains("--kernel"));
    assert!(help.contains("--state"));
}

#[test]
fn simulation_parameters_are_world_options_not_flags() {
    let directory = tempfile::tempdir().unwrap();
    for command in ["run", "replay"] {
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
fn run_rejects_removed_execution_controls() {
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
    for command in ["run", "check", "replay"] {
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
    for option in ["page_timer", "deferred_tx", "stop_tolerance"] {
        fs::write(directory.path().join("seedbox.yaml"), format!("name: defaults\noracle: {{image: busybox:1.37.0}}\nsimulator: {{{option}: true}}\n")).unwrap();
        let output = seedbox(&["check", "."], directory.path());
        assert_eq!(output.status.code(), Some(2));
        assert!(stderr(&output).contains(&format!("unknown field `{option}`")));
        assert!(!directory.path().join(".seedbox").exists());
    }
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
#[test]
fn retired_environment_does_not_override_the_driver_package_during_preparation() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    assert!(seedbox(&["init", "demo"], root).status.success());
    let project = root.join("demo");
    let output = Command::new(env!("CARGO_BIN_EXE_seedbox"))
        .args(["check"])
        .current_dir(&project)
        .env_clear()
        .env("HOME", root)
        .env("SEEDBOX_ASSET_DIR", root)
        .env("PATH", "")
        .envs([
            ("SEEDBOX_VMM", "missing-vmm"),
            ("SEEDBOX_KERNEL", "missing-kernel"),
            ("SEEDBOX_RUNTIME_ASSETS", "missing-archive"),
            ("SEEDBOX_GUEST_INIT", "missing-init"),
            ("SEEDBOX_MKE2FS", "missing-formatter"),
        ])
        .guarded_output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    let archive = root
        .join(std::env::consts::ARCH)
        .join("driver-runtime.tar.gz");
    assert_eq!(stderr(&output), format!("error: driver runtime {}: {}; run cargo xtask build-assets or supply a complete runtime archive\n", archive.display(), std::io::Error::from_raw_os_error(2)));
    assert!(!project.join("prepared.json").exists());
    assert!(!project.join(".seedbox/runs").exists());
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
#[test]
fn invalid_relocated_driver_package_is_rejected_without_downloads_or_tools() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    assert!(seedbox(&["init", "demo"], root).status.success());
    let project = root.join("demo");
    let assets = project.join("assets").join(std::env::consts::ARCH);
    fs::create_dir_all(&assets).unwrap();
    let archive = assets.join("driver-runtime.tar.gz");
    fs::write(&archive, b"not an archive").unwrap();
    let output = seedbox(&["check"], &project);
    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    assert_eq!(stderr(&output), format!("error: driver runtime {}: invalid gzip header; run cargo xtask build-assets or supply a complete runtime archive\n", archive.display()));
    assert!(!stderr(&output).contains("downloading"));
    assert!(!project.join("prepared.json").exists());
    assert!(!project.join(".seedbox/runs").exists());
}

#[test]
fn removed_preparation_flags_fail_before_preparation() {
    let directory = tempfile::tempdir().unwrap();
    fs::write(
        directory.path().join("seedbox.yaml"),
        "name: source-build\noracle: {build: {context: ./missing}}\n",
    )
    .unwrap();
    for flag in ["--builder", "--build-epoch", "--wall-clock-epoch"] {
        let output = seedbox(&["run", flag, "value"], directory.path());
        assert!(stderr(&output).contains("unexpected argument"));
    }
}

#[test]
fn omitted_run_subcommand_accepts_the_same_project_argument() {
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
    let output = seedbox(
        &["show", "--view", "playback", "invalid", "--json"],
        directory.path(),
    );
    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    assert!(stderr(&output).contains("simulation ID must contain eight characters"));
}

#[test]
fn playback_mutations_reject_invalid_rates_and_request_ids_before_connecting() {
    let directory = tempfile::tempdir().unwrap();
    for (args, phrase) in [
        (vec!["control", "rate", "k7m2p9qx", "1/0"], "rate must be"),
        (
            vec!["control", "rate", "k7m2p9qx", "4294967296"],
            "rate must be",
        ),
        (
            vec!["control", "pause", "k7m2p9qx", "--request-id", "bad/id"],
            "request ID must be",
        ),
        (
            vec!["control", "receipt", "k7m2p9qx", "bad/id"],
            "request ID must be",
        ),
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
            vec!["show", "--view", "world", "invalid"],
            "simulation ID must contain",
        ),
        (
            vec!["show", "--view", "timeline", "invalid"],
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
    let missing = seedbox(&["control", "inject", "k7m2p9qx", "missing.yaml"], cwd);
    assert_eq!(missing.status.code(), Some(2));
    assert!(missing.stdout.is_empty());
    assert_eq!(
        stderr(&missing),
        "error: fault missing.yaml: No such file or directory (os error 2)\n"
    );
    fs::write(cwd.join("invalid.yaml"), "action: nonexistent\n").unwrap();
    let invalid = seedbox(&["control", "inject", "k7m2p9qx", "invalid.yaml"], cwd);
    assert_eq!(invalid.status.code(), Some(2));
    assert!(invalid.stdout.is_empty());
    assert!(stderr(&invalid)
        .contains("error: fault invalid.yaml: action: unknown variant `nonexistent`"));
    fs::write(cwd.join("empty.yaml"), "").unwrap();
    let empty = seedbox(&["control", "inject", "k7m2p9qx", "empty.yaml"], cwd);
    assert_eq!(empty.status.code(), Some(2));
    assert!(empty.stdout.is_empty());
    assert!(
        stderr(&empty).starts_with("error: fault empty.yaml: "),
        "{}",
        stderr(&empty)
    );
    fs::write(cwd.join("large.yaml"), vec![b' '; 65537]).unwrap();
    let large = seedbox(&["control", "inject", "k7m2p9qx", "large.yaml"], cwd);
    assert_eq!(large.status.code(), Some(2));
    assert_eq!(
        stderr(&large),
        "error: fault large.yaml: exceeds 65536 bytes\n"
    );
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
        "error: seedbox tui needs an interactive terminal; use seedbox events or seedbox show from scripts\n"
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
    let output = seedbox(&["check", ".", "--json"], directory.path());
    assert!(
        stderr(&output).contains("driver runtime"),
        "{}",
        stderr(&output)
    );
    let output = seedbox(&["run", ".", "--wall-time", "0s"], directory.path());
    assert_eq!(output.status.code(), Some(2));
    assert!(stderr(&output).contains("--wall-time"));
    assert!(stderr(&output).contains("duration must be positive"));
}

#[test]
fn capacity_help_explains_resident_slots_and_accepts_json() {
    let directory = tempfile::tempdir().unwrap();
    let output = seedbox(&["server", "status", "--help"], directory.path());
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("capacity"));
    assert!(stdout.contains("--json"));
    let output = seedbox(&["server", "status", "--unexpected"], directory.path());
    assert_eq!(output.status.code(), Some(2));
    assert!(stderr(&output).contains("--unexpected"));
}

#[test]
fn machine_workers_is_only_configurable_in_the_project() {
    let root = tempfile::tempdir().unwrap();
    for command in ["run", "replay"] {
        let help = seedbox(&[command, "--help"], root.path());
        assert!(help.status.success(), "{command}: {}", stderr(&help));
        let text = String::from_utf8_lossy(&help.stdout);
        assert!(!text.contains("--machine-workers"), "{command}: {text}");

        let output = seedbox(&[command, "--machine-workers", "4"], root.path());
        assert_eq!(output.status.code(), Some(2), "{command}");
        assert!(output.stdout.is_empty());
        assert!(
            stderr(&output).contains("unexpected argument '--machine-workers'"),
            "{command}: {}",
            stderr(&output)
        );
    }
}

#[test]
fn backend_flag_is_typed_and_only_on_execution_commands() {
    let root = tempfile::tempdir().unwrap();
    for command in ["run", "replay"] {
        let help = seedbox(&[command, "--help"], root.path());
        assert!(help.status.success(), "{command}: {}", stderr(&help));
        let text = String::from_utf8_lossy(&help.stdout);
        if command == "run" {
            assert!(
                text.split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" ")
                    .contains("without KVM"),
                "{text}"
            );
        }
        assert!(
            text.contains("--backend") && text.contains("pv") && text.contains("tcg"),
            "{command}: {text}"
        );
        let invalid = seedbox(&[command, "--backend", "not-a-backend"], root.path());
        assert_eq!(invalid.status.code(), Some(2));
        assert!(
            stderr(&invalid).contains("not-a-backend"),
            "{}",
            stderr(&invalid)
        );
    }
    for command in ["init", "show", "checkpoint"] {
        let help = seedbox(&[command, "--help"], root.path());
        assert!(help.status.success());
        assert!(!String::from_utf8_lossy(&help.stdout).contains("--backend"));
    }
}

#[cfg(unix)]
#[path = "cli/checkpoint_history.rs"]
mod checkpoint_history;
#[cfg(unix)]
#[path = "cli/checkpoint_version.rs"]
mod checkpoint_version;

#[test]
fn show_world_prints_machine_and_link_tables() {
    use seedbox_api::{
        LinkCounters, LinkPolicy, LinkState, MachineCondition, MachineState, WorldState,
    };
    use std::io::{Read, Write};
    use std::os::unix::fs::DirBuilderExt;
    let directory = tempfile::tempdir().unwrap();
    let state = directory.path().join(".local/state/seedbox");
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&state)
        .unwrap();
    let machine = |name: &str, oracle| MachineState {
        name: name.into(),
        incarnation: 0,
        condition: MachineCondition::Ready,
        exit_code: None,
        throttle_permille: None,
        oracle,
        last_transition: None,
        last_transition_at_ns: None,
        condition_since_ns: None,
        application: Default::default(),
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
        override_rejections: Default::default(),
        simulation: "k7m2p9qx".parse().unwrap(),
        revision: 3,
        simulated_time_ns: 1_000_000_000,
        machines: vec![machine("n1", false), machine("oracle", true)],
        links: vec![link("n1", "oracle", None), link("oracle", "n1", Some("p1"))],
        faults: Vec::new(),
    };
    let body = serde_json::to_string(&world).unwrap();
    // A stand-in server answering the one world request on the default socket.
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
            if !request.starts_with(b"GET /v1/simulations/k7m2p9qx/world ") {
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
    let output = seedbox(&["show", "--view", "world", "k7m2p9qx"], directory.path());
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        "\
MACHINE  CONDITION  INCARNATION  EXIT  ORACLE
n1       ready      0            -     -
oracle   ready      0            -     yes

FROM    TO      LATENCY  JITTER  LOSS    FRAMES  BYTES  DROPPED  FAULT
n1      oracle  2.5ms    500µs   100ppm  12      960    4        -
oracle  n1      2.5ms    500µs   100ppm  12      960    4        cut p1

simulated time: 1s
"
    );
}

#[test]
fn root_commands_are_the_supported_workflow_without_legacy_aliases() {
    let root = tempfile::tempdir().unwrap();
    let output = seedbox(&["__complete", "--", ""], root.path());
    assert!(output.status.success());
    let mut commands: Vec<_> = String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect();
    commands.sort();
    assert_eq!(
        commands,
        [
            "check",
            "checkpoint",
            "compare",
            "completions",
            "control",
            "delete",
            "events",
            "help",
            "init",
            "logs",
            "replay",
            "run",
            "server",
            "show",
            "stop",
            "tui",
            "wait"
        ]
    );
    let help = seedbox(&["--help"], root.path());
    assert!(help.status.success());
    let help = String::from_utf8(help.stdout).unwrap();
    let command_section = help
        .split("Commands:\n")
        .nth(1)
        .unwrap()
        .split("\n\n")
        .next()
        .unwrap();
    let mut visible: Vec<_> = command_section
        .lines()
        .filter_map(|line| line.split_whitespace().next())
        .collect();
    visible.sort();
    assert_eq!(visible, commands);
    let help = seedbox(&["help", "compare"], root.path());
    assert!(String::from_utf8_lossy(&help.stdout).contains("NOT_COMPARABLE"));
}

#[test]
fn checkpoint_output_is_checked_before_connecting_and_never_overwrites() {
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("saved.seedbox"), b"existing checkpoint").unwrap();
    let existing = seedbox(
        &["checkpoint", "k7m2p9qx", "-o", "saved.seedbox"],
        root.path(),
    );
    assert_eq!(existing.status.code(), Some(2));
    assert_eq!(
        stderr(&existing),
        "error: --output saved.seedbox: already exists; choose a new file under real directories\n"
    );
    assert_eq!(
        fs::read(root.path().join("saved.seedbox")).unwrap(),
        b"existing checkpoint"
    );
    let parent = seedbox(
        &["checkpoint", "k7m2p9qx", "-o", "missing/out.seedbox"],
        root.path(),
    );
    assert_eq!(parent.status.code(), Some(2));
    assert_eq!(stderr(&parent), "error: --output missing/out.seedbox: No such file or directory (os error 2); choose a new file under real directories\n");
    std::os::unix::fs::symlink(".", root.path().join("parent")).unwrap();
    let linked = seedbox(
        &["checkpoint", "k7m2p9qx", "-o", "parent/out.seedbox"],
        root.path(),
    );
    assert_eq!(linked.status.code(), Some(2));
    assert!(stderr(&linked).starts_with("error: --output parent/out.seedbox:"));
    assert!(!root.path().join("out.seedbox").exists());
    assert!(!root.path().join(".local/state/seedbox").exists());
}

#[test]
fn run_source_and_inspection_choices_are_validated_before_connecting() {
    let root = tempfile::tempdir().unwrap();
    for args in [
        vec!["run", "project.yaml", "--from", "k7m2p9qx"],
        vec!["run", "--from", "k7m2p9qx", "--count", "2"],
        vec!["show", "--view", "world"],
        vec!["show", "k7m2p9qx", "--view", "unknown"],
        vec!["show", "k7m2p9qx", "--file", "vmm.stderr"],
    ] {
        let output = seedbox(&args, root.path());
        assert_eq!(output.status.code(), Some(2), "{args:?}");
        assert!(output.stdout.is_empty(), "{args:?}");
        assert!(
            !stderr(&output).contains("no local server"),
            "{args:?}: {}",
            stderr(&output)
        );
    }
    assert!(!root.path().join(".local/state/seedbox").exists());
}

#[test]
fn every_auto_start_command_exposes_the_explicit_server_configuration() {
    let root = tempfile::tempdir().unwrap();
    for command in [
        vec!["run"],
        vec!["replay"],
        vec!["compare"],
        vec!["show"],
        vec!["logs"],
        vec!["events"],
        vec!["wait"],
        vec!["tui"],
        vec!["stop"],
        vec!["delete"],
        vec!["checkpoint"],
        vec!["control", "pause"],
        vec!["control", "resume"],
        vec!["control", "step"],
        vec!["control", "rate"],
        vec!["control", "inject"],
        vec!["control", "heal"],
        vec!["control", "receipt"],
        vec!["server", "start"],
    ] {
        let mut args = command.clone();
        args.push("--help");
        let output = seedbox(&args, root.path());
        assert!(output.status.success(), "{command:?}: {}", stderr(&output));
        let help = String::from_utf8(output.stdout).unwrap();
        for flag in ["--state", "--full-state"] {
            assert!(help.contains(flag), "{command:?} omits {flag}: {help}");
        }
    }
}
