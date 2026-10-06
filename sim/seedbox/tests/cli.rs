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
