//! Public faults compile to the same typed actions and targets used by Driver admission.
use crate::{
    driver::{Action, FaultHandle, HealTarget, NetworkPolicy, Target},
    execution::ExecutionConfig,
};
use seedbox_engine_protocol::control::Action as Public;
use seedbox_machine::clock::ClockRate;
use std::{num::NonZeroI64, time::Duration};

/// Translate a public action without resolving targets, sampling RNG or allocating IDs.
pub fn translate(
    action: &Public,
    timeout_ns: u64,
) -> Result<(Action, Option<FaultHandle>), String> {
    let target = |name: &String| Target::Name(name.clone());
    let targets = |names: &[String]| names.iter().map(target).collect();
    let handle =
        |name: &str| FaultHandle::named(name.to_owned()).map_err(|error| error.to_string());
    let deadline = Duration::from_nanos(timeout_ns);
    let (action, alias) = match action {
        Public::DropConnection {
            handle: alias,
            connection,
        } => (
            Action::DropConnection {
                connection: connection.clone(),
            },
            Some(handle(alias)?),
        ),
        Public::Stop { machine } => (
            Action::Stop {
                target: target(machine),
                deadline,
            },
            None,
        ),
        Public::Restart { machine } => (
            Action::Restart {
                target: target(machine),
                deadline,
            },
            None,
        ),
        Public::PauseProcess { machine } => (
            Action::PauseProcess {
                target: target(machine),
                deadline,
            },
            None,
        ),
        Public::ResumeProcess { machine } => (
            Action::ResumeProcess {
                target: target(machine),
                deadline,
            },
            None,
        ),
        Public::Pause { machine } => (Action::Pause(target(machine)), None),
        Public::Resume { machine } => (Action::Resume(target(machine)), None),
        Public::Crash { machine } => (Action::Crash(target(machine)), None),
        Public::Reboot { machine } => (Action::Reboot(target(machine)), None),
        Public::Spawn { machine } => (
            Action::Spawn {
                name: machine.clone(),
            },
            None,
        ),
        Public::Partition {
            handle: alias,
            left,
            right,
        } => (
            Action::Partition {
                left: targets(left),
                right: targets(right),
            },
            Some(handle(alias)?),
        ),
        Public::Shape {
            handle: alias,
            from,
            to,
            latency_ns,
            jitter_ns,
            loss_ppm,
            bandwidth_bps,
            queue_bound_ns,
            network_faults,
        } => (
            Action::Shape {
                from: targets(from),
                to: targets(to),
                policy: NetworkPolicy {
                    latency_ns: *latency_ns,
                    jitter_ns: *jitter_ns,
                    loss_ppm: *loss_ppm,
                    bandwidth_bps: *bandwidth_bps,
                    queue_bound_ns: *queue_bound_ns,
                },
                network_faults: *network_faults,
            },
            Some(handle(alias)?),
        ),
        Public::Heal { handle: alias } => (Action::Heal(HealTarget::Handle(handle(alias)?)), None),
        Public::Throttle {
            handle: alias,
            machine,
            factor_permille,
        } => {
            let factor = u32::try_from(*factor_permille)
                .map_err(|_| "execution rate factor exceeds u32".to_owned())?;
            (
                Action::Throttle {
                    target: target(machine),
                    config: ExecutionConfig::new(factor).map_err(|error| error.to_string())?,
                },
                Some(handle(alias)?),
            )
        }
        Public::ClockDrift {
            handle: alias,
            machine,
            rate_ppb,
            max_rate_ppb,
        } => {
            seedbox_engine_protocol::control::validate_clock_drift(*rate_ppb, *max_rate_ppb)?;
            let action = match (rate_ppb, max_rate_ppb) {
                (Some(rate), None) => Action::Drift {
                    target: target(machine),
                    rate: ClockRate::new(*rate).map_err(|error| error.to_string())?,
                },
                (None, Some(bound)) => Action::DriftBounded {
                    target: target(machine),
                    max_rate_ppb: *bound,
                },
                _ => return Err("clock drift requires one exact value or bound".into()),
            };
            (action, Some(handle(alias)?))
        }
        Public::ClockSkew {
            handle: alias,
            machine,
            offset_ns,
            max_offset_ns,
        } => {
            seedbox_engine_protocol::control::validate_clock_skew(*offset_ns, *max_offset_ns)?;
            let action = match (offset_ns, max_offset_ns) {
                (Some(offset), None) => Action::StepRealtime {
                    target: target(machine),
                    nanoseconds: NonZeroI64::new(*offset)
                        .ok_or_else(|| "clock skew must be nonzero".to_owned())?,
                },
                (None, Some(bound)) => Action::StepRealtimeBounded {
                    target: target(machine),
                    max_offset_ns: *bound,
                },
                _ => return Err("clock skew requires one exact value or bound".into()),
            };
            (action, Some(handle(alias)?))
        }
    };
    Ok((action, alias))
}

impl Action {
    pub(crate) fn command(
        &self,
        input_handle: impl Fn(
            &crate::driver::InputKey,
        )
            -> Result<crate::oracle::service::Command, crate::oracle::OperationError>,
    ) -> Result<crate::oracle::service::Command, crate::oracle::OperationError> {
        use crate::oracle::{
            service::{Amount, Command, Effect},
            Target as Wire,
        };
        use crate::pv::ApplicationCommand as App;
        let wire = |target: &Target| match target {
            Target::Machine(id) => Wire::Key(super::control::Control::source(*id)),
            Target::Name(name) => Wire::Machine(name.clone()),
        };
        let effect = |target: &Target, effect| Command::Effect {
            target: wire(target),
            effect,
        };
        match self {
            Self::Pause(target) => Ok(effect(target, Effect::Pause)),
            Self::Resume(target) => Ok(effect(target, Effect::Resume)),
            Self::Crash(target) => Ok(effect(target, Effect::Crash)),
            Self::Reboot(target) => Ok(effect(target, Effect::Reboot)),
            Self::Shutdown(target) => Ok(effect(target, Effect::Shutdown)),
            Self::Stop { target, .. } => Ok(effect(target, Effect::Process(App::Stop))),
            Self::Restart { target, .. } => Ok(effect(target, Effect::Process(App::Start))),
            Self::PauseProcess { target, .. } => Ok(effect(target, Effect::Process(App::Pause))),
            Self::ResumeProcess { target, .. } => Ok(effect(target, Effect::Process(App::Resume))),
            Self::Throttle { target, config } => Ok(Command::Throttle {
                target: wire(target),
                config: *config,
            }),
            Self::Heal(HealTarget::Handle(handle)) => Ok(Command::Heal {
                handle: handle.0.clone(),
            }),
            Self::Heal(HealTarget::Input(input)) => input_handle(input),
            Self::Spawn { name } => Ok(Command::Spawn {
                machine: name.clone(),
            }),
            Self::Partition { left, right } => Ok(Command::Partition {
                left: left.iter().map(wire).collect(),
                right: right.iter().map(wire).collect(),
            }),
            Self::Shape {
                from,
                to,
                policy,
                network_faults,
            } => Ok(Command::Shape {
                from: from.iter().map(wire).collect(),
                to: to.iter().map(wire).collect(),
                policy: *policy,
                network_faults: *network_faults,
            }),
            Self::DropConnection { connection } => Ok(Command::DropConnection {
                connection: connection.clone(),
            }),
            Self::DriftBounded {
                target,
                max_rate_ppb,
            } => Ok(Command::Drift {
                target: wire(target),
                ppb: Amount::Seeded(*max_rate_ppb),
            }),
            Self::StepRealtimeBounded {
                target,
                max_offset_ns,
            } => Ok(Command::StepRealtime {
                target: wire(target),
                nanoseconds: Amount::Seeded(*max_offset_ns),
            }),
            Self::Drift { target, rate } => Ok(Command::Drift {
                target: wire(target),
                ppb: Amount::Exact(i64::from(rate.ppb())),
            }),
            Self::StepRealtime {
                target,
                nanoseconds,
            } => Ok(Command::StepRealtime {
                target: wire(target),
                nanoseconds: Amount::Exact(nanoseconds.get()),
            }),
        }
    }
}

/// Names selected by pure target compilation, without admission or execution effects.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActionTargets {
    pub machines: Vec<String>,
    pub links: Vec<(String, String)>,
}

/// Compile public fault arguments using the executing Driver command model.
/// Names describe the configured inventory, not live availability. Heals need an
/// observed active handle, so no targets are guessed here. This draws no RNG,
/// allocates no IDs and reserves no handles.
pub fn action_targets(
    action: &Public,
    names: &[String],
    latency_floor_ns: u64,
) -> Result<ActionTargets, String> {
    use crate::oracle::{OperationError, Target as Wire};
    use std::collections::BTreeSet;
    let (action, _) = translate(
        action,
        seedbox_engine_protocol::control::PROCESS_EFFECT_TIMEOUT_NS,
    )?;
    let command = action
        .command(|_| Err(OperationError::invalid("input handle requires live state")))
        .map_err(|error| error.message)?;
    let resolve = |target: &Wire| {
        let (Wire::Machine(name) | Wire::Service(name) | Wire::Key(name)) = target;
        match names.iter().filter(|candidate| *candidate == name).count() {
            1 => Ok(name.clone()),
            0 => Err(OperationError::invalid("unknown machine target")),
            _ => Err(OperationError::invalid("ambiguous machine target")),
        }
    };
    let compiled = command
        .compile_targets(resolve, resolve, || Ok(latency_floor_ns))
        .map_err(|error| error.message)?;
    let mut machines = BTreeSet::new();
    let mut links = BTreeSet::new();
    if let Some(machine) = compiled.machine {
        machines.insert(machine);
    }
    if let Some((left, right)) = compiled.sides {
        for from in &left {
            for to in &right {
                links.insert((from.clone(), to.clone()));
                if matches!(command, crate::oracle::service::Command::Partition { .. }) {
                    links.insert((to.clone(), from.clone()));
                }
            }
        }
    }
    if let Some(connection) = compiled.connection {
        links.insert(connection);
    }
    for (from, to) in &links {
        machines.insert(from.clone());
        machines.insert(to.clone());
    }
    Ok(ActionTargets {
        machines: machines.into_iter().collect(),
        links: links.into_iter().collect(),
    })
}

#[cfg(test)]
mod tests;
