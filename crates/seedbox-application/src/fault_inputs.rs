//! Pure input-program validation and fault target selection.

use crate::{Error, Result};
use seedbox_driver::driver::faults::ActionTargets;

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
        fault_action_targets(configuration, &input.action)
            .map_err(|error| Error::Setup(format!("program {}: {error}", input.key)))?;
    }
    Ok(())
}

/// Pure target selection using the same shape compiler as engine admission.
/// This does not contact a worker or touch a simulation's RNG, state or IDs.
pub fn fault_action_targets(
    configuration: &crate::config::Simulation,
    action: &seedbox_control::Action,
) -> Result<ActionTargets> {
    let names: Vec<_> = configuration.services.keys().cloned().collect();
    seedbox_driver::driver::faults::action_targets(action, &names, configuration.network.latency_ns)
        .map_err(Error::Setup)
}

#[cfg(test)]
mod program_tests {
    use super::*;
    use seedbox_control::Action;
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
            },
        };
        let heal = TimedInput {
            timeout_ns: seedbox_engine_protocol::input_program::default_timeout_ns(),
            key: "heal".into(),
            at_ns: 10,
            action: Action::Heal {
                handle: "slow".into(),
            },
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
        };
        assert!(validate_input_program(&config, &[unknown]).is_err());
        config.control = Some(crate::config::Control {
            service: "db".into(),
        });
        assert!(validate_input_program(&config, &[heal]).is_ok());
    }
}
