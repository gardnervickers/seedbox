//! Backend-independent inputs for native public-service fault acceptance.
//! This module constructs requests, not results. A drop target must come from
//! the running world's public connection observation, never a guessed tuple.
use seedbox_api::{FaultAction as Action, FaultCatalogue};
use seedbox_engine_protocol::control::{Connection, NetworkFaults};
use std::collections::BTreeSet;

pub struct FaultScenario {
    pub name: &'static str,
    pub actions: Vec<Action>,
}

pub fn recovery_scenarios(connection: Connection) -> Vec<FaultScenario> {
    let machine = "target".to_owned();
    let handle = "acceptance-fault".to_owned();
    let heal = || Action::Heal {
        handle: handle.clone(),
    };
    vec![
        FaultScenario {
            name: "application-stop-restart",
            actions: vec![
                Action::Stop {
                    machine: machine.clone(),
                },
                Action::Restart {
                    machine: machine.clone(),
                },
            ],
        },
        FaultScenario {
            name: "application-pause-resume",
            actions: vec![
                Action::PauseProcess {
                    machine: machine.clone(),
                },
                Action::ResumeProcess {
                    machine: machine.clone(),
                },
            ],
        },
        FaultScenario {
            name: "machine-pause-resume",
            actions: vec![
                Action::Pause {
                    machine: machine.clone(),
                },
                Action::Resume {
                    machine: machine.clone(),
                },
            ],
        },
        FaultScenario {
            name: "machine-crash-reboot",
            actions: vec![
                Action::Crash {
                    machine: machine.clone(),
                },
                Action::Reboot {
                    machine: machine.clone(),
                },
            ],
        },
        FaultScenario {
            name: "partition-heal",
            actions: vec![
                Action::Partition {
                    handle: handle.clone(),
                    left: vec!["client".into()],
                    right: vec![machine.clone()],
                },
                heal(),
            ],
        },
        FaultScenario {
            name: "shape-heal",
            actions: vec![
                Action::Shape {
                    handle: handle.clone(),
                    from: vec!["client".into()],
                    to: vec![machine.clone()],
                    latency_ns: 1_000_000,
                    jitter_ns: 200_000,
                    loss_ppm: 50_000,
                    bandwidth_bps: Some(100_000_000),
                    queue_bound_ns: Some(10_000_000),
                    network_faults: NetworkFaults::default(),
                },
                heal(),
            ],
        },
        FaultScenario {
            name: "connection-drop-heal",
            actions: vec![
                Action::DropConnection {
                    handle: handle.clone(),
                    connection,
                },
                heal(),
            ],
        },
        FaultScenario {
            name: "clock-skew-heal",
            actions: vec![
                Action::ClockSkew {
                    handle: handle.clone(),
                    machine: machine.clone(),
                    offset_ns: Some(2_000_000_000),
                    max_offset_ns: None,
                },
                heal(),
            ],
        },
        FaultScenario {
            name: "clock-drift-heal",
            actions: vec![
                Action::ClockDrift {
                    handle: handle.clone(),
                    machine: machine.clone(),
                    rate_ppb: Some(100_000_000),
                    max_rate_ppb: None,
                },
                heal(),
            ],
        },
        FaultScenario {
            name: "throttle-heal",
            actions: vec![
                Action::Throttle {
                    handle,
                    machine,
                    factor_permille: 2_000,
                },
                Action::Heal {
                    handle: "acceptance-fault".into(),
                },
            ],
        },
    ]
}

/// Keep this exhaustive: adding a public action must select an observable
/// acceptance family, rather than silently disappearing from the native matrix.
pub fn family(action: &Action) -> &'static str {
    match action {
        Action::Stop { .. }
        | Action::Restart { .. }
        | Action::PauseProcess { .. }
        | Action::ResumeProcess { .. } => "application",
        Action::Pause { .. }
        | Action::Resume { .. }
        | Action::Crash { .. }
        | Action::Reboot { .. }
        | Action::Throttle { .. } => "machine",
        Action::ClockSkew { .. } | Action::ClockDrift { .. } => "clock",
        Action::DropConnection { .. } | Action::Partition { .. } | Action::Shape { .. } => {
            "network"
        }
        Action::Heal { .. } => "recovery",
        // Refused until a later change adds them with their native scenarios.
        Action::DiskError { .. } | Action::DiskLatency { .. } => "disk",
        Action::Preempt { .. } => "machine",
    }
}

/// Native callers compare against the actual server catalogue before running.
/// This is coverage of the public surface, not an admission/qualification gate.
pub fn assert_catalogue_covered(catalogue: &FaultCatalogue, cases: &[FaultScenario]) {
    let planned = cases
        .iter()
        .flat_map(|case| case.actions.iter().map(Action::name))
        .collect::<BTreeSet<_>>();
    let advertised = catalogue
        .faults
        .iter()
        .map(|fault| fault.name.as_str())
        .collect::<BTreeSet<_>>();
    assert_eq!(
        advertised, planned,
        "public catalogue and native fault matrix differ"
    );
}
