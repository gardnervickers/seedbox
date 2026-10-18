//! Portable request-matrix tests only; these do not execute or prove faults.
#[path = "support/fault_scenarios.rs"]
mod scenarios;
use scenarios::{assert_catalogue_covered, family, recovery_scenarios};
use seedbox_api::{
    ControlCommand, ControlRequest, FaultAction, FaultCatalogue, FaultCategory, FaultDescriptor,
};
use std::collections::BTreeSet;

fn observed_connection_fixture() -> seedbox_control::Connection {
    seedbox_control::Connection {
        from: "client".into(),
        to: "target".into(),
        src_addr: "10.0.0.2".parse().unwrap(),
        dst_addr: "10.0.0.3".parse().unwrap(),
        src_port: 40001,
        dst_port: 9099,
        incarnation: 7,
    }
}
const NAMES: [&str; 15] = [
    "drop_connection",
    "stop",
    "restart",
    "pause_process",
    "resume_process",
    "partition",
    "shape",
    "heal",
    "clock_skew",
    "clock_drift",
    "throttle",
    "pause",
    "resume",
    "crash",
    "reboot",
];

#[test]
fn recovery_matrix_covers_all_public_actions_without_claiming_native_results() {
    let cases = recovery_scenarios(observed_connection_fixture());
    assert_eq!(cases.len(), 10);
    assert_eq!(
        cases
            .iter()
            .map(|case| case.name)
            .collect::<BTreeSet<_>>()
            .len(),
        cases.len()
    );
    let names = cases
        .iter()
        .flat_map(|case| case.actions.iter().map(FaultAction::name))
        .collect::<BTreeSet<_>>();
    assert_eq!(names, NAMES.into_iter().collect());
    let families = cases
        .iter()
        .flat_map(|case| case.actions.iter().map(family))
        .collect::<BTreeSet<_>>();
    assert_eq!(
        families,
        ["application", "machine", "network", "clock", "recovery"]
            .into_iter()
            .collect()
    );
}

#[test]
fn every_fault_payload_round_trips_the_public_injection_request() {
    for case in recovery_scenarios(observed_connection_fixture()) {
        for (index, action) in case.actions.into_iter().enumerate() {
            let request = ControlRequest {
                request_id: format!("{}-{index}", case.name),
                command: ControlCommand::InjectFault { action },
            };
            let encoded = serde_json::to_vec(&request).unwrap();
            assert_eq!(
                serde_json::from_slice::<ControlRequest>(&encoded).unwrap(),
                request
            );
        }
    }
}

#[test]
fn connection_tuple_and_recovery_handles_are_preserved() {
    let observed = observed_connection_fixture();
    for case in recovery_scenarios(observed.clone()) {
        let [first, second] = case.actions.as_slice() else {
            panic!("fault/recovery pair required")
        };
        if let FaultAction::DropConnection { connection, .. } = first {
            assert_eq!(connection, &observed);
        }
        match (first, second) {
            (FaultAction::Stop { machine: a }, FaultAction::Restart { machine: b })
            | (
                FaultAction::PauseProcess { machine: a },
                FaultAction::ResumeProcess { machine: b },
            )
            | (FaultAction::Pause { machine: a }, FaultAction::Resume { machine: b })
            | (FaultAction::Crash { machine: a }, FaultAction::Reboot { machine: b }) => {
                assert_eq!(a, b)
            }
            (
                FaultAction::DropConnection { handle: a, .. }
                | FaultAction::Partition { handle: a, .. }
                | FaultAction::Shape { handle: a, .. }
                | FaultAction::ClockSkew { handle: a, .. }
                | FaultAction::ClockDrift { handle: a, .. }
                | FaultAction::Throttle { handle: a, .. },
                FaultAction::Heal { handle: b },
            ) => assert_eq!(a, b),
            _ => panic!("{} is not a matching recovery pair", case.name),
        }
    }
}

#[test]
fn catalogue_coverage_detects_advertised_but_unplanned_actions() {
    let cases = recovery_scenarios(observed_connection_fixture());
    let descriptor = |name: &str| FaultDescriptor {
        name: name.into(),
        category: FaultCategory::Recovery,
        title: name.into(),
        description: "metadata fixture, not native evidence".into(),
        schema: serde_json::json!({}),
    };
    let mut catalogue = FaultCatalogue {
        simulation: "deadbeef".parse().unwrap(),
        faults: NAMES.into_iter().map(descriptor).collect(),
    };
    assert_catalogue_covered(&catalogue, &cases);
    catalogue.faults.push(descriptor("new-action"));
    assert!(std::panic::catch_unwind(|| assert_catalogue_covered(&catalogue, &cases)).is_err());
}
