//! Host checks for preparation smoke inputs and strict comparison helpers.
//! The 4-write/1-worker/180-second variant is ONLY a prep/fast test.
//! bd-207 acceptance must run canonical examples/etcd unchanged.
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
        assert_eq!(volumes[0].source, format!("{member}-data"));
    }
    assert_eq!(fs::read(canonical_path).unwrap(), before);
}

#[test]
fn passing_acceptance_requires_completed_oracle_zero_not_just_matching_evidence() {
    use seedbox_api::{ExecutionTermination, Phase, SimulationStatus};
    for phase in [
        Phase::Queued,
        Phase::Preparing,
        Phase::Running,
        Phase::Finalizing,
        Phase::Completed,
    ] {
        for oracle_exit_code in [None, Some(0), Some(1)] {
            for termination in [
                Some(ExecutionTermination::OracleExited),
                Some(ExecutionTermination::Stopped),
                Some(ExecutionTermination::Error {
                    message: "infrastructure failure".into(),
                }),
                None,
            ] {
                let expected = phase == Phase::Completed
                    && oracle_exit_code == Some(0)
                    && termination == Some(ExecutionTermination::OracleExited);
                let status = SimulationStatus {
                    phase,
                    oracle_exit_code,
                    termination,
                    ..SimulationStatus::default()
                };
                assert_eq!(require_pass(&status).is_ok(), expected, "{status:?}");
            }
        }
    }
}

#[test]
fn forged_wire_verdict_cannot_replace_missing_oracle_exit() {
    let status = seedbox_api::SimulationStatus {
        phase: seedbox_api::Phase::Completed,
        termination: Some(seedbox_api::ExecutionTermination::OracleExited),
        ..Default::default()
    };
    let mut wire = serde_json::to_value(&status).unwrap();
    wire["verdict"] = "passed".into();
    let received = serde_json::from_value(wire).unwrap();
    assert!(require_pass(&received).is_err());
}

#[test]
fn backend_parameter_is_explicit_and_rejects_legacy_or_wrong_backend() {
    let path = std::path::Path::new("project.yaml");
    let pv_args = submit_arguments(path, BASELINE_SEED, Backend::Pv).unwrap();
    assert!(!pv_args.iter().any(|arg| arg == "--backend"));
    let tcg_args = submit_arguments(path, BASELINE_SEED, Backend::Tcg).unwrap();
    let mut expected_tcg = pv_args.clone();
    expected_tcg.extend([std::ffi::OsString::from("--backend"), "tcg".into()]);
    assert_eq!(tcg_args, expected_tcg);
    for backend in [Backend::Pv, Backend::Tcg] {
        let valid = serde_json::json!({"backend": backend.profile(), "driver_world": {}});
        assert!(require_backend(&valid, backend).is_ok());
        for invalid in [
            serde_json::json!({"backend": "seedbox-kvm", "driver_world": {}}),
            serde_json::json!({"backend": backend.profile()}),
            serde_json::json!({"backend": backend.profile(), "driver_world": null}),
        ] {
            assert!(require_backend(&invalid, backend).is_err());
        }
    }
    assert!(require_backend(
        &serde_json::json!({"backend": "driver-pv", "driver_world": {}}),
        Backend::Tcg
    )
    .is_err());
    assert!(require_backend(
        &serde_json::json!({"backend": "driver-tcg", "driver_world": {}}),
        Backend::Pv
    )
    .is_err());
}

#[test]
fn real_go_oracle_output_requires_all_requested_acknowledgements_and_settled_checks() {
    assert!(require_oracle_output(
        b"LOAD acked=4 failed=1 elapsed=1s\nRESULT pass acked=4 failed=1 rev=42 hash=11\n"
    )
    .is_ok());
    for invalid in [
        "RESULT fail acked=4 failed=0 rev=42 hash=11",
        "RESULT pass acked=40 failed=0 rev=42 hash=11",
        "RESULT pass acked=3 failed=0 rev=42 hash=11",
        "RESULT pass acked=4 failed=0",
        "RESULT incomplete acked=4 target=4",
        "",
    ] {
        assert!(
            require_oracle_output(invalid.as_bytes()).is_err(),
            "{invalid}"
        );
    }
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
    let commands = seeds.map(|seed| submit_arguments(&project, seed, Backend::Pv).unwrap());
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
