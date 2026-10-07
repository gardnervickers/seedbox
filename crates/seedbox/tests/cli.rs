use std::{
    fs,
    path::Path,
    process::{Command, Output},
};

/// Run the CLI with a scrubbed environment: no `SEEDBOX_*`, `CARGO*` or
/// `SUDO_USER` variables, `HOME` at `cwd`, and `PATH` set to `path` (empty if
/// `None`), so no default can come from the developer's machine.
fn seedbox_with(args: &[&str], cwd: &Path, path: Option<&Path>) -> Output {
    Command::new(env!("CARGO_BIN_EXE_seedbox"))
        .args(args)
        .current_dir(cwd)
        .env_clear()
        .env("HOME", cwd)
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
fn timebase_help_values_and_replay_uses_only_the_recorded_choice() {
    let directory = tempfile::tempdir().unwrap();
    let help = seedbox(&["help", "run"], directory.path());
    assert!(help.status.success());
    let help = String::from_utf8_lossy(&help.stdout);
    assert!(help.contains("--timebase <TIMEBASE>"));
    assert!(help.contains("[default: counter]"));
    assert!(help.contains("Count retired guest branches with the host PMU"));
    assert!(help.contains("Advance time at guest exits, syscalls and clock reads"));
    let bad = seedbox(&["run", "--timebase", "future"], directory.path());
    assert_eq!(bad.status.code(), Some(2));
    assert!(stderr(&bad).contains("--timebase <TIMEBASE>"));
    assert!(stderr(&bad).contains("possible values: counter, pv"));
    let help = seedbox(&["help", "replay"], directory.path());
    assert!(help.status.success());
    assert!(!String::from_utf8_lossy(&help.stdout).contains("--timebase"));
    let override_attempt = seedbox(&["replay", ".", "--timebase", "pv"], directory.path());
    assert_eq!(override_attempt.status.code(), Some(2));
    assert!(stderr(&override_attempt).contains("unexpected argument '--timebase'"));
    assert!(seedbox(&["init", "demo"], directory.path())
        .status
        .success());
    for value in ["counter", "pv"] {
        let output = seedbox(
            &["run", "no-such.yaml", "--timebase", value],
            directory.path(),
        );
        assert_eq!(output.status.code(), Some(2));
        assert_eq!(
            stderr(&output),
            "error: Compose file no-such.yaml: not found\n"
        );
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
