use seedbox_test::process::Guarded;
use std::{collections::BTreeMap, path::Path, process::Command};

#[test]
fn python_projects_share_client_and_use_a_dedicated_runtime_controller() {
    let temp = tempfile::tempdir().unwrap();
    let destination = temp.path().join("projects");
    let examples = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/faults");
    assert!(Command::new("python3")
        .arg(examples.join("generate.py"))
        .arg(&destination)
        .guarded_status()
        .unwrap()
        .success());
    let fixed = seedbox_application::config::load(
        &destination.join("fixed/seedbox.yaml"),
        &BTreeMap::new(),
    )
    .unwrap();
    let broken = seedbox_application::config::load(
        &destination.join("broken/seedbox.yaml"),
        &BTreeMap::new(),
    )
    .unwrap();
    assert_eq!(fixed.services.len(), 3);
    assert_eq!(fixed.control.as_ref().unwrap().service, "oracle");
    assert_eq!(fixed.check.service, "oracle");
    assert_eq!(fixed.seed, broken.seed);
    for name in ["client", "oracle"] {
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
    assert_eq!(fixed.schedule.stop_tolerance_ns, 0);
    let no_faults = seedbox_application::config::load(
        &destination.join("broken-no-faults/seedbox.yaml"),
        &BTreeMap::new(),
    )
    .unwrap();
    assert!(no_faults.services["oracle"]
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
        let project: serde_yaml_ng::Value = serde_yaml_ng::from_str(
            &std::fs::read_to_string(destination.join(variant).join("seedbox.yaml")).unwrap(),
        )
        .unwrap();
        assert_eq!(project["simulator"]["quantum"].as_str(), Some("1ms"));
        assert_eq!(project["simulator"]["machineWorkers"].as_u64(), Some(1));
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

#[path = "fault_examples/portable.rs"]
mod portable;

#[test]
fn actual_python_services_and_independent_client_verify_two_restarts() {
    portable::retry_controls();
    portable::scenarios();
    println!("portable Python workload checks passed");
}
