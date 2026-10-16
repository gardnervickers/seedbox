//! Mechanical public-action translation. No RNG, alias table or admission policy.
#[cfg(test)]
mod tests;
use seedbox_control::Action as Public;
use seedbox_driver::{
    driver::{Action, FaultHandle, HealTarget, NetworkPolicy, Target},
    execution::ExecutionConfig,
};
use seedbox_machine::clock::ClockRate;
use std::{num::NonZeroI64, time::Duration};

pub(super) fn translate(
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
            seedbox_control::validate_clock_drift(*rate_ppb, *max_rate_ppb)?;
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
            seedbox_control::validate_clock_skew(*offset_ns, *max_offset_ns)?;
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
