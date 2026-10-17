//! Pure input-program validation and fault target selection.

use crate::{Error, Result};
use seedbox_engine_protocol::world_config::{LinkConfig, NetworkConfig};

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
    let faults = configured_faults(configuration)?;
    for input in program {
        faults
            .validate_action_shape(&input.action)
            .map_err(|error| Error::Setup(format!("program {}: {error}", input.key)))?;
    }
    Ok(())
}

/// Pure target selection using the same shape compiler as engine admission.
/// This does not contact a worker or touch a simulation's RNG, state or IDs.
pub fn fault_action_targets(
    configuration: &crate::config::Simulation,
    action: &seedbox_control::Action,
) -> Result<seedbox_engine_core::faults::ActionTargets> {
    configured_faults(configuration)?
        .action_targets(action)
        .map_err(Error::Setup)
}

fn configured_faults(
    configuration: &crate::config::Simulation,
) -> Result<seedbox_engine_core::faults::Faults> {
    let names: Vec<_> = configuration.services.keys().cloned().collect();
    let network = NetworkConfig {
        latency_ns: configuration.network.latency_ns,
        jitter_ns: configuration.network.jitter_ns,
        bandwidth_bps: configuration.network.bandwidth_bps,
        queue_bound_ns: configuration.network.queue_bound_ns,
        links: configuration
            .network
            .links
            .iter()
            .map(|link| LinkConfig {
                from: link.from.clone(),
                to: link.to.clone(),
                latency_ns: link.latency_ns,
                jitter_ns: link.jitter_ns,
                loss_ppm: link.loss_ppm,
                bandwidth_bps: link.bandwidth_bps,
                queue_bound_ns: link.queue_bound_ns,
            })
            .collect(),
    };
    let links = seedbox_engine_core::links::Links::from_network(0, &names, &network)
        .map_err(Error::Setup)?;
    let faults = seedbox_engine_core::faults::Faults::with_plans(Vec::new(), names, links)
        .map_err(Error::Setup)?;
    Ok(faults)
}

#[cfg(test)]
mod program_tests {
    use super::*;
    use seedbox_control::Action;
    use seedbox_engine_protocol::input_program::TimedInput;
    #[test]
    fn program_validates_targets_handles_order_keys_and_deadlines_without_ids() {
        let mut config = crate::config::parse(
            "services:\n  db:\n    image: example:v1\n",
            std::path::Path::new("/fixture/compose.yaml"),
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
