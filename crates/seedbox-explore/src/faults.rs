//! Sample a fault profile into keyed program inputs.
//!
//! Sampling is a pure function of the profile, the machine names and the seed,
//! using integer arithmetic only, so a seed yields the same schedule on every
//! host. Lifecycle faults (crash, pause, process pause and stop) never overlap
//! on one machine; neither do clock faults. Network faults may overlap freely.
use seedbox_control::{Action, DiskOp};
use seedbox_spec::fault_profile::{DiskOp as SpecDiskOp, DurationRange, FaultKind, FaultProfile};
use std::collections::BTreeMap;

/// A generated input. Keys and handles share the `gen-<n>` prefix: a fault's
/// start is `gen-<n>`, its teardown `gen-<n>-end`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Generated {
    pub key: String,
    pub at_ns: u64,
    pub action: Action,
}

use seedbox_engine_protocol::input_program::GENERATED_END_SUFFIX;
/// Every generated key starts with this; user program keys must not. The
/// engine gates generated inputs by this namespace.
pub use seedbox_engine_protocol::input_program::GENERATED_PREFIX as KEY_PREFIX;

/// Sample `profile` for one simulation. `machines` lists every machine
/// including the oracle; `limit_ns` is the simulated time limit, which every
/// input must precede.
pub fn generate(
    profile: &FaultProfile,
    seed: u64,
    machines: &[String],
    oracle: &str,
    limit_ns: u64,
) -> Result<Vec<Generated>, String> {
    profile.validate()?;
    let targets: Vec<&str> = if profile.targets.is_empty() {
        machines
            .iter()
            .map(String::as_str)
            .filter(|m| *m != oracle)
            .collect()
    } else {
        for target in &profile.targets {
            if target == oracle {
                return Err("fault profile cannot target the oracle".into());
            }
            if !machines.contains(target) {
                return Err(format!("fault profile target {target:?} is not a machine"));
            }
        }
        profile.targets.iter().map(String::as_str).collect()
    };
    if targets.is_empty() {
        return Err("fault profile has no machine to target".into());
    }
    let mut rng = Rng::new(seed, 0x6661_756c_7470_726f);
    let total_weight: u64 = profile.faults.iter().map(|f| u64::from(f.weight())).sum();
    let mut lifecycle_free: BTreeMap<&str, u64> = BTreeMap::new();
    let mut clock_free: BTreeMap<&str, u64> = BTreeMap::new();
    let mut disk_free: BTreeMap<&str, u64> = BTreeMap::new();
    for fault in &profile.faults {
        if let FaultKind::Partition { from, .. } = fault {
            if let Some(machine) = from.iter().find(|m| !machines.contains(m)) {
                return Err(format!(
                    "fault profile partition from {machine:?} is not a machine"
                ));
            }
        }
        if let FaultKind::DiskError {
            targets: narrowed, ..
        } = fault
        {
            for target in narrowed {
                if !targets.contains(&target.as_str()) {
                    return Err(format!(
                        "fault profile disk_error target {target:?} is not a profile target"
                    ));
                }
            }
        }
    }
    let mut inputs = Vec::new();
    let mut at = profile.start.0;
    let mut faults = 0;
    while faults < profile.max_faults {
        // Uniform gap in [interval/2, 3*interval/2]: mean `interval`.
        let interval = profile.interval.0;
        at = at.saturating_add(interval / 2 + rng.below(interval.saturating_add(1)));
        if at >= profile.end.0 || at >= limit_ns {
            break;
        }
        let kind = {
            let mut pick = rng.below(total_weight);
            profile
                .faults
                .iter()
                .find(|f| {
                    let weight = u64::from(f.weight());
                    pick < weight || {
                        pick -= weight;
                        false
                    }
                })
                .expect("pick is below the total weight")
        };
        let busy = match kind {
            FaultKind::Crash { .. }
            | FaultKind::Pause { .. }
            | FaultKind::PauseProcess { .. }
            | FaultKind::Stop { .. } => Some(&mut lifecycle_free),
            FaultKind::ClockSkew { .. } => Some(&mut clock_free),
            FaultKind::DiskError { .. } => Some(&mut disk_free),
            FaultKind::Partition { .. } | FaultKind::Shape { .. } => None,
        };
        let candidates: Vec<&str> = match kind {
            FaultKind::DiskError {
                targets: narrowed, ..
            } if !narrowed.is_empty() => narrowed.iter().map(String::as_str).collect(),
            _ => targets.clone(),
        };
        // Draw a target, then take the first free one from there, so a busy
        // machine shifts the choice instead of skipping the fault.
        let first = rng.below(candidates.len() as u64) as usize;
        let target = (0..candidates.len())
            .map(|i| candidates[(first + i) % candidates.len()])
            .find(|t| {
                busy.as_ref()
                    .is_none_or(|b| b.get(t).is_none_or(|free| *free <= at))
            });
        let Some(target) = target else {
            continue;
        };
        let handle = format!("{KEY_PREFIX}{faults}");
        let (start, end, length) = match kind {
            FaultKind::Partition { duration, from, .. } => {
                let right: Vec<String> = machines
                    .iter()
                    .filter(|m| m.as_str() != target && (from.is_empty() || from.contains(m)))
                    .cloned()
                    .collect();
                if right.is_empty() {
                    // The target is the only machine it would be cut off from.
                    continue;
                }
                (
                    Action::Partition {
                        handle: handle.clone(),
                        left: vec![target.into()],
                        right,
                    },
                    Action::Heal {
                        handle: handle.clone(),
                    },
                    rng.within(duration),
                )
            }
            FaultKind::Shape {
                duration,
                latency,
                loss_ppm,
                ..
            } => {
                let to = machines
                    .iter()
                    .filter(|m| m.as_str() != target)
                    .cloned()
                    .collect();
                (
                    Action::Shape {
                        handle: handle.clone(),
                        from: vec![target.into()],
                        to,
                        latency_ns: rng.within(latency),
                        jitter_ns: 0,
                        loss_ppm: *loss_ppm,
                        bandwidth_bps: None,
                        queue_bound_ns: None,
                        network_faults: Default::default(),
                    },
                    Action::Heal {
                        handle: handle.clone(),
                    },
                    rng.within(duration),
                )
            }
            FaultKind::Crash { downtime, .. } => (
                Action::Crash {
                    machine: target.into(),
                },
                Action::Reboot {
                    machine: target.into(),
                },
                rng.within(downtime),
            ),
            FaultKind::Pause { duration, .. } => (
                Action::Pause {
                    machine: target.into(),
                },
                Action::Resume {
                    machine: target.into(),
                },
                rng.within(duration),
            ),
            FaultKind::PauseProcess { duration, .. } => (
                Action::PauseProcess {
                    machine: target.into(),
                },
                Action::ResumeProcess {
                    machine: target.into(),
                },
                rng.within(duration),
            ),
            FaultKind::Stop { downtime, .. } => (
                Action::Stop {
                    machine: target.into(),
                },
                Action::Restart {
                    machine: target.into(),
                },
                rng.within(downtime),
            ),
            FaultKind::DiskError {
                duration, op, ppm, ..
            } => (
                Action::DiskError {
                    handle: handle.clone(),
                    machine: target.into(),
                    op: match op {
                        SpecDiskOp::Read => DiskOp::Read,
                        SpecDiskOp::Write => DiskOp::Write,
                        SpecDiskOp::Flush => DiskOp::Flush,
                        SpecDiskOp::Any => DiskOp::Any,
                    },
                    ppm: *ppm,
                },
                Action::Heal {
                    handle: handle.clone(),
                },
                rng.within(duration),
            ),
            FaultKind::ClockSkew {
                duration,
                max_offset,
                ..
            } => {
                let span = max_offset.0.saturating_mul(2).saturating_add(1);
                let offset = rng.below(span) as i64 - max_offset.0 as i64;
                (
                    Action::ClockSkew {
                        handle: handle.clone(),
                        machine: target.into(),
                        offset_ns: Some(offset),
                        max_offset_ns: None,
                    },
                    Action::Heal {
                        handle: handle.clone(),
                    },
                    rng.within(duration),
                )
            }
        };
        // A teardown past the limit would be rejected; end the fault there.
        let end_at = at.saturating_add(length).min(limit_ns.saturating_sub(1));
        if end_at <= at {
            break;
        }
        if let Some(busy) = busy {
            busy.insert(target, end_at.saturating_add(1));
        }
        inputs.push(Generated {
            key: handle.clone(),
            at_ns: at,
            action: start,
        });
        inputs.push(Generated {
            key: format!("{handle}{GENERATED_END_SUFFIX}"),
            at_ns: end_at,
            action: end,
        });
        faults += 1;
    }
    // Stable: at equal times a fault's own order and generation order hold.
    inputs.sort_by_key(|input| input.at_ns);
    Ok(inputs)
}

/// splitmix64: small, fast and fully specified, so schedules never depend on
/// a library's generator choice.
use crate::rng::Rng;

trait Within {
    fn within(&mut self, range: &DurationRange) -> u64;
}
impl Within for Rng {
    fn within(&mut self, range: &DurationRange) -> u64 {
        range.min.0 + self.below(range.max.0 - range.min.0 + 1)
    }
}

#[cfg(test)]
#[path = "faults_tests.rs"]
mod tests;
