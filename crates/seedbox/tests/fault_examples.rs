use std::{collections::BTreeMap, path::Path, process::Command};

#[test]
fn python_projects_share_client_and_use_a_dedicated_runtime_controller() {
    let temp = tempfile::tempdir().unwrap();
    let destination = temp.path().join("projects");
    let examples = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/faults");
    assert!(Command::new("python3")
        .arg(examples.join("generate.py"))
        .arg(&destination)
        .status()
        .unwrap()
        .success());
    let fixed =
        seedbox::config::load(&destination.join("fixed/compose.yaml"), &BTreeMap::new()).unwrap();
    let broken =
        seedbox::config::load(&destination.join("broken/compose.yaml"), &BTreeMap::new()).unwrap();
    assert_eq!(fixed.services.len(), 3);
    assert_eq!(fixed.control.as_ref().unwrap().service, "scenario");
    assert_eq!(fixed.check.service, "scenario");
    assert_eq!(fixed.seed, broken.seed);
    for name in ["client", "scenario"] {
        assert_eq!(fixed.services[name].command, broken.services[name].command);
    }
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
    assert!(no_faults.services["scenario"]
        .command
        .as_ref()
        .unwrap()
        .contains(&"--no-faults".into()));
    assert!(no_faults.services["server"]
        .command
        .as_ref()
        .unwrap()
        .contains(&"--broken".into()));
    for variant in ["fixed", "broken", "broken-no-faults"] {
        for source in ["main.py", "scenario.py", "Dockerfile"] {
            assert_eq!(
                std::fs::read(destination.join(variant).join("workload").join(source)).unwrap(),
                std::fs::read(examples.join("workload").join(source)).unwrap()
            );
        }
        for source in ["__init__.py", "control.py"] {
            assert_eq!(
                std::fs::read(
                    destination
                        .join(variant)
                        .join("workload/seedbox")
                        .join(source)
                )
                .unwrap(),
                std::fs::read(examples.join("../../python/seedbox").join(source)).unwrap()
            );
        }
        assert!(!destination.join(variant).join("workload/main.rs").exists());
    }
}

#[test]
fn actual_python_services_and_independent_client_verify_two_restarts() {
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/faults/portable.py");
    let result = Command::new("python3").arg(script).output().unwrap();
    assert!(
        result.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(
        String::from_utf8_lossy(&result.stdout).contains("portable Python workload checks passed")
    );
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
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/faults/run.py");
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
            "--io-check",
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
fn output_reservation_allows_supplied_guest_binaries_and_retains_subsequent_failure() {
    let temp = tempfile::tempdir().unwrap();
    let init = temp.path().join("compose-init");
    let bytes = b"\x7fELFprecreated integration input";
    std::fs::write(&init, bytes).unwrap();
    let probe = temp.path().join("io-check");
    std::fs::write(&probe, bytes).unwrap();
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/faults/run.py");
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
        .arg("--io-check")
        .arg(&probe)
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
    assert_eq!(std::fs::read(probe).unwrap(), bytes);
}
