//! Normal CLI/service etcd acceptance inputs and strict comparison checks.
//! Native execution belongs to bd-207 after Driver preparation and disks land.
#[path = "support/etcd_project.rs"]
mod etcd_project;

use etcd_project::*;
use seedbox_api::{ComparisonStatus, SimulationComparison};
use std::{collections::BTreeMap, fs};

#[test]
fn bounded_fixture_keeps_three_real_members_persistent_disks_and_go_oracle() {
    let original = canonical_project();
    let fixture = acceptance_project();
    assert_eq!(fixture.machines, original.machines);
    assert_eq!(
        fixture
            .machines
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        ["n1", "n2", "n3"]
    );
    for (name, member) in &fixture.machines {
        assert_eq!(member.image.as_deref(), Some("quay.io/coreos/etcd:v3.5.2"));
        assert_eq!(member.volumes.len(), 1);
        assert_eq!(member.volumes["data"].target, "/data");
        let command = member.command.as_ref().unwrap();
        assert!(command.contains(&format!("--data-dir=/data/{name}")));
        assert!(command.contains(
            &"--initial-cluster=n1=http://n1:2380,n2=http://n2:2380,n3=http://n3:2380".to_owned()
        ));
    }
    let mut expected_oracle = original.oracle;
    expected_oracle.build.as_mut().unwrap().context =
        example_directory().join("workload").canonicalize().unwrap();
    expected_oracle.command = Some(
        [
            "--endpoints",
            "n1:2379,n2:2379,n3:2379",
            "--duration",
            "180s",
            "--concurrency",
            "1",
            "--writes",
            "4",
        ]
        .map(String::from)
        .to_vec(),
    );
    assert_eq!(fixture.oracle, expected_oracle);
    assert_eq!(fixture.network, original.network);
    assert_eq!(fixture.simulator, original.simulator);
    assert_eq!((WRITES_PER_WORKER, CONCURRENCY), (4, 1));
}

#[test]
fn fixture_loads_via_normal_project_parser_without_modifying_example() {
    let canonical_path = example_directory().join("seedbox.yaml");
    let before = fs::read(&canonical_path).unwrap();
    let directory = tempfile::tempdir().unwrap();
    let file = write_acceptance_project(directory.path());
    let configuration = seedbox_application::config::load(&file, &BTreeMap::new()).unwrap();
    assert_eq!(configuration.services.len(), 4);
    assert_eq!(configuration.check.service, "oracle");
    assert_eq!(configuration.check.exit_code, 0);
    // Native authoring enables the oracle controller implicitly, even though
    // this particular Go oracle only talks to etcd over the network. TCG must
    // actually support that contract; the harness must not disable it.
    assert_eq!(configuration.control.as_ref().unwrap().service, "oracle");
    for member in ["n1", "n2", "n3"] {
        let volumes = &configuration.services[member].volumes;
        assert_eq!(volumes.len(), 1);
        assert_eq!(volumes[0].target, "/data");
    }
    assert_eq!(fs::read(canonical_path).unwrap(), before);
}

fn matching_comparison() -> SimulationComparison {
    SimulationComparison {
        baseline: "abcdefgh".parse().unwrap(),
        simulation: "jkmnpqrs".parse().unwrap(),
        status: ComparisonStatus::Match,
        outcome_matches: true,
        witness_matches: Some(true),
        full_state_matches: Some(true),
        fault_history_matches: true,
    }
}

#[test]
fn comparison_requires_witness_full_state_outcome_and_fault_history() {
    assert!(require_match(&matching_comparison()).is_ok());
    for field in [
        "status-mismatch",
        "status-incomparable",
        "outcome",
        "witness",
        "missing-witness",
        "state",
        "missing-state",
        "faults",
    ] {
        let mut comparison = matching_comparison();
        match field {
            "status-mismatch" => comparison.status = ComparisonStatus::Mismatch,
            "status-incomparable" => comparison.status = ComparisonStatus::NotComparable,
            "outcome" => comparison.outcome_matches = false,
            "witness" => comparison.witness_matches = Some(false),
            "missing-witness" => comparison.witness_matches = None,
            "state" => comparison.full_state_matches = Some(false),
            "missing-state" => comparison.full_state_matches = None,
            "faults" => comparison.fault_history_matches = false,
            _ => unreachable!(),
        }
        assert!(require_match(&comparison).is_err(), "accepted {field}");
    }
}

#[test]
fn seed_plan_keeps_repeat_identity_without_demanding_distinct_seed_witnesses() {
    let directory = tempfile::tempdir().unwrap();
    let project = directory.path().join("project with spaces.yaml");
    let seeds = [BASELINE_SEED, BASELINE_SEED, EXTRA_SEEDS[0], EXTRA_SEEDS[1]];
    let commands = seeds.map(|seed| submit_arguments(&project, seed));
    assert_eq!(commands[0], commands[1]);
    for (command, seed) in commands.iter().zip(["123", "123", "124", "125"]) {
        assert_eq!(
            command,
            &[
                std::ffi::OsString::from("run"),
                project.as_os_str().to_owned(),
                "--seed".into(),
                seed.into(),
                "--paused".into(),
                "--detach".into(),
            ]
        );
    }
    // This validator intentionally doesn't know the seed. Different seeds may
    // legitimately produce identical results; each still needs a passing oracle.
    assert!(require_match(&matching_comparison()).is_ok());
}
