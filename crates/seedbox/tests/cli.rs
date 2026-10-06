use std::{fs, process::Command};

#[test]
fn validation_cli_reports_structured_output_and_nonzero_errors() {
    let directory = tempfile::tempdir().unwrap();
    let source = directory.path().join("compose.yaml");
    let yaml = "name: cli-test\nservices:\n  workload: {image: example/workload:v1}\nx-seedbox:\n  version: 1\n  seed: 0\n  limits: {simulated_time: 1s, wall_time: 1m}\n  check: {service: workload}\n";
    fs::write(&source, yaml).unwrap();
    let success = Command::new(env!("CARGO_BIN_EXE_seedbox"))
        .arg("validate")
        .arg(&source)
        .arg("--json")
        .output()
        .unwrap();
    assert!(
        success.status.success(),
        "{}",
        String::from_utf8_lossy(&success.stderr)
    );
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
    let failure = Command::new(env!("CARGO_BIN_EXE_seedbox"))
        .arg("validate")
        .arg(&source)
        .output()
        .unwrap();
    assert_eq!(failure.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&failure.stderr).contains("$.services.workload.volumes"));
    assert!(!directory.path().join("runs").exists());
}

fn seedbox(args: &[&str], cwd: &std::path::Path) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_seedbox"))
        .args(args)
        .current_dir(cwd)
        .env_remove("SEEDBOX_KERNEL")
        .env_remove("SEEDBOX_ARTIFACTS")
        .env_remove("SEEDBOX_WORK")
        .env("HOME", cwd)
        .env_remove("SUDO_USER")
        .output()
        .unwrap()
}

#[test]
fn compose_argument_defaults_to_the_project_directory() {
    let directory = tempfile::tempdir().unwrap();
    let project = directory.path().join("demo");
    let created = seedbox(&["init", "demo"], directory.path());
    assert!(created.status.success());
    assert_eq!(
        String::from_utf8_lossy(&created.stdout),
        format!("CREATED: demo\n")
    );
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
    let existing = seedbox(&["init", "."], directory.path());
    assert_eq!(existing.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&existing.stderr).contains("already exists"));

    let missing = seedbox(&["validate", "nope.yaml"], directory.path());
    assert_eq!(missing.status.code(), Some(2));
    assert_eq!(
        String::from_utf8_lossy(&missing.stderr),
        "error: Compose file nope.yaml: not found\n"
    );

    let run = seedbox(&["inspect", "no-such-run"], directory.path());
    assert_eq!(
        String::from_utf8_lossy(&run.stderr),
        "error: run directory no-such-run: not found\n"
    );

    let usage = seedbox(&["run", "--margin", "0"], directory.path());
    assert_eq!(usage.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&usage.stderr).contains("positive branch count"));
}

#[cfg(target_os = "linux")]
#[test]
fn run_without_a_kernel_lists_where_it_looked() {
    let directory = tempfile::tempdir().unwrap();
    assert!(
        seedbox(&["init", "demo", "--template", "redis"], directory.path())
            .status
            .success()
    );
    let vmm = directory.path().join("seedbox-vmm");
    fs::write(&vmm, b"").unwrap();
    let output = seedbox(
        &["run", "demo", "--vmm", vmm.to_str().unwrap()],
        directory.path(),
    );
    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.starts_with("error: no guest kernel found; searched:\n"),
        "{stderr}"
    );
    assert!(stderr.contains("artifacts/bzImage"));
    assert!(stderr.contains("--kernel"));
    assert!(!directory.path().join(".seedbox/runs").exists());
}
