//! Pure input-program validation and fault target selection.

use crate::{Error, Result};
use seedbox_driver_execution::driver_world::ActionTargets;
use seedbox_engine_protocol::input_program::InputAction;

/// Validate a client's schedule before preparing or starting a simulation.
/// Service and machine names coincide in the current one-service-per-machine ABI.
pub fn validate_input_program(
    configuration: &crate::config::Simulation,
    program: &[seedbox_engine_protocol::input_program::TimedInput],
) -> Result<()> {
    seedbox_engine_protocol::input_program::validate_program(
        program,
        configuration.limits.simulated_time_ns,
    )
    .map_err(Error::Setup)?;
    if program.is_empty() {
        return Ok(());
    }
    for input in program {
        let context = |error: String| Error::Setup(format!("program {}: {error}", input.key));
        match &input.action {
            InputAction::Fault(action) => {
                fault_action_targets(configuration, action)
                    .map_err(|error| context(error.to_string()))?;
            }
            // Rule and branch shapes were checked with the program; no
            // Driver arms a program trigger, answers a choice or branches yet.
            InputAction::Rule(_) | InputAction::Branch(_) => {
                return Err(context(seedbox_engine_protocol::not_implemented(
                    input.action.name(),
                )))
            }
        }
    }
    Ok(())
}

/// Pure target selection using the same shape compiler as engine admission.
/// This does not contact a worker or touch a simulation's RNG, state or IDs.
pub fn fault_action_targets(
    configuration: &crate::config::Simulation,
    action: &seedbox_engine_protocol::control::Action,
) -> Result<ActionTargets> {
    let names: Vec<_> = configuration.services.keys().cloned().collect();
    seedbox_driver_execution::driver_world::action_targets(
        action,
        &names,
        configuration.network.latency_ns,
    )
    .map_err(Error::Setup)
}

#[cfg(test)]
mod program_tests {
    use super::*;
    use seedbox_engine_protocol::control::Action;
    use seedbox_engine_protocol::input_program::TimedInput;
    #[test]
    fn program_validates_targets_handles_order_keys_and_deadlines_without_ids() {
        let mut config = crate::config::parse(
            "name: test\nmachines:\n  db: {image: example:v1}\noracle: {image: example/oracle}\n",
            std::path::Path::new("/fixture/seedbox.yaml"),
            &Default::default(),
        )
        .unwrap();
        let slow = |key: &str, at_ns| TimedInput {
            timeout_ns: seedbox_engine_protocol::input_program::default_timeout_ns(),
            key: key.into(),
            at_ns,
            action: Action::Throttle {
                handle: "slow".into(),
                machine: "db".into(),
                factor_permille: 2000,
            }
            .into(),
        };
        let heal = TimedInput {
            timeout_ns: seedbox_engine_protocol::input_program::default_timeout_ns(),
            key: "heal".into(),
            at_ns: 10,
            action: Action::Heal {
                handle: "slow".into(),
            }
            .into(),
        };
        let entries = [slow("first", 10), heal.clone(), slow("again", 20)];
        validate_input_program(&config, &entries).unwrap();
        // Handle availability is decided against the world when due. Editing
        // an earlier action can turn a later heal into a recorded rejection.
        validate_input_program(&config, std::slice::from_ref(&heal)).unwrap();
        validate_input_program(&config, &[slow("first", 1), slow("again", 2)]).unwrap();
        assert!(validate_input_program(&config, &[slow("same", 1), slow("same", 2)]).is_err());
        assert!(validate_input_program(&config, &[slow("later", 2), slow("earlier", 1)]).is_err());
        assert!(
            validate_input_program(&config, &[slow("late", config.limits.simulated_time_ns)])
                .is_err()
        );
        config.limits.simulated_time_ns = u64::MAX;
        assert!(validate_input_program(&config, &[slow("overflow", u64::MAX - 1)]).is_err());
        let mut unknown = slow("unknown", 1);
        unknown.action = Action::Stop {
            machine: "missing".into(),
        }
        .into();
        assert!(validate_input_program(&config, &[unknown]).is_err());
        config.control = Some(crate::config::Control {
            service: "db".into(),
        });
        assert!(validate_input_program(&config, &[heal]).is_ok());
    }

    #[test]
    fn documented_yaml_overrides_parse_as_program_inputs() {
        // The example in docs/faults.md.
        let document = r#"
- {id: slow-fsync, atNs: 1000000000,
   action: {action: disk_latency, handle: slow, machine: db, op: flush, latency_ns: 20000000}}
- {id: reboot-db, atNs: 0,
   action: {action: trigger, when: {kind: lifecycle, machine: db, transition: crashed},
            then: {action: reboot, machine: db}}}
"#;
        let overrides: Vec<seedbox_spec::definition::Override> =
            serde_yaml_ng::from_str(document).unwrap();
        let names: Vec<_> = overrides
            .into_iter()
            .map(|entry| {
                let input: TimedInput = serde_json::from_value(serde_json::json!({
                    "key": entry.id, "at_ns": entry.at_ns, "action": entry.action,
                }))
                .unwrap();
                input.action.validate().unwrap();
                input.action.name()
            })
            .collect();
        assert_eq!(names, ["disk_latency", "trigger"]);
    }

    #[test]
    fn defined_only_faults_are_refused_before_preparation() {
        let config = crate::config::parse(
            "name: test\nmachines:\n  db: {image: example:v1}\noracle: {image: example/oracle}\n",
            std::path::Path::new("/fixture/seedbox.yaml"),
            &Default::default(),
        )
        .unwrap();
        let input = TimedInput {
            timeout_ns: seedbox_engine_protocol::input_program::default_timeout_ns(),
            key: "eio".into(),
            at_ns: 10,
            action: Action::DiskError {
                handle: "eio".into(),
                machine: "db".into(),
                op: seedbox_engine_protocol::control::DiskOp::Write,
                ppm: 1_000_000,
                effect: Default::default(),
                scope: Default::default(),
            }
            .into(),
        };
        let error = validate_input_program(&config, &[input]).unwrap_err();
        assert!(
            error
                .to_string()
                .contains(&seedbox_engine_protocol::not_implemented("disk_error")),
            "{error}"
        );
        let trigger: TimedInput = serde_json::from_value(serde_json::json!({
            "key": "reboot", "at_ns": 10,
            "action": {
                "action": "trigger",
                "when": {"kind": "lifecycle", "machine": "db", "transition": "crashed"},
                "then": {"action": "reboot", "machine": "db"},
            },
        }))
        .unwrap();
        let error = validate_input_program(&config, &[trigger]).unwrap_err();
        assert_eq!(
            error.to_string(),
            format!(
                "program reboot: {}",
                seedbox_engine_protocol::not_implemented("trigger")
            )
        );
        for (key, action, name) in [
            ("a0", serde_json::json!({"action": "anchor"}), "anchor"),
            (
                "pick",
                serde_json::json!({"action": "choice", "machine": "db", "incarnation": 0,
                                   "session": 1, "site": "s", "occurrence": 1, "select": 0}),
                "choice",
            ),
        ] {
            let input: TimedInput = serde_json::from_value(
                serde_json::json!({"key": key, "at_ns": 10, "action": action}),
            )
            .unwrap();
            let error = validate_input_program(&config, &[input]).unwrap_err();
            assert_eq!(
                error.to_string(),
                format!(
                    "program {key}: {}",
                    seedbox_engine_protocol::not_implemented(name)
                )
            );
        }
    }
}
