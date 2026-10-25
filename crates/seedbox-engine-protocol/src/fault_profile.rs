//! Fault profiles: the faults a user wants, as ranges and weights, for the
//! server to sample into one run's program.
//!
//! A profile is a request option, never part of a definition. A seeded
//! sampler in `seedbox-engine-core` (deterministic policy), run by the server
//! at submission, draws it once from the run's seed into ordinary keyed
//! program inputs:
//! fault `n` starts at input `gen-<n>` and its teardown (a heal, resume,
//! reboot or restart) is `gen-<n>-end`. The resolved definition retains those
//! inputs, so rerun and replay execute them and never sample the profile
//! again. Each entry is the [`Action`](crate::control::Action) it generates,
//! with ranges in place of values and the action's own snake_case field
//! names, and is checked by that action's own validator. Checks that need
//! the world (target names, the network's latency floor) happen when it is
//! sampled. Nothing samples a profile yet: every submission that carries one
//! is refused.
use crate::control::{self, DiskEffect, DiskOp, DiskScope};
use serde::{Deserialize, Serialize};

/// Generated faults never exceed this many. Each takes two program inputs
/// (start and teardown) and fault IDs, never trigger IDs, so a full profile
/// leaves half of [`crate::input_program::MAX_INPUTS`] for authored inputs;
/// submission checks the sum.
pub const MAX_GENERATED_FAULTS: u32 = 1024;
const _: () = assert!(2 * MAX_GENERATED_FAULTS as usize <= crate::input_program::MAX_INPUTS);
/// The fault count a profile allows when it does not say.
pub const DEFAULT_MAX_FAULTS: u32 = 64;

/// Faults start between `start_ns` and `end_ns` of simulated time, one gap
/// apart; each gap is drawn uniformly from half to one and a half times
/// `interval_ns`. Each start draws an entry by weight and a target, then the
/// entry's ranges. Generated starts wait for readiness and quiet periods; see
/// [`crate::input_program::GENERATED_PREFIX`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FaultProfile {
    #[serde(default, skip_serializing_if = "is_zero")]
    pub start_ns: u64,
    pub end_ns: u64,
    pub interval_ns: u64,
    #[serde(
        default = "default_max_faults",
        skip_serializing_if = "is_default_max_faults"
    )]
    pub max_faults: u32,
    /// Machines faults may target; empty means every machine but the oracle.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub targets: Vec<String>,
    /// The kinds of fault to draw, by weight.
    pub faults: Vec<FaultKind>,
}

fn default_max_faults() -> u32 {
    DEFAULT_MAX_FAULTS
}
fn is_default_max_faults(value: &u32) -> bool {
    *value == DEFAULT_MAX_FAULTS
}
fn is_zero(value: &u64) -> bool {
    *value == 0
}

impl FaultProfile {
    /// The program inputs the profile can generate: a start and a teardown
    /// per fault.
    pub fn max_inputs(&self) -> usize {
        2 * self.max_faults as usize
    }
    /// Shape checks that need no world. Target names are checked against
    /// the simulation when the profile is sampled.
    pub fn validate(&self) -> Result<(), String> {
        if self.end_ns <= self.start_ns {
            return Err("fault profile end_ns must be after its start_ns".into());
        }
        if self.interval_ns == 0 {
            return Err("fault profile interval_ns must be positive".into());
        }
        if self.max_faults == 0 || self.max_faults > MAX_GENERATED_FAULTS {
            return Err(format!(
                "fault profile max_faults must be 1 to {MAX_GENERATED_FAULTS}"
            ));
        }
        if self.targets.iter().any(String::is_empty) {
            return Err("fault profile targets must be nonempty machine names".into());
        }
        if self.faults.iter().all(|fault| fault.weight() == 0) {
            return Err("fault profile needs a fault kind with positive weight".into());
        }
        for fault in &self.faults {
            fault
                .validate()
                .map_err(|error| format!("fault profile {}: {error}", fault.name()))?;
        }
        Ok(())
    }
}

/// An inclusive range of nanoseconds, sampled uniformly.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DurationRange {
    pub min: u64,
    pub max: u64,
}

impl DurationRange {
    fn validate(&self, field: &str) -> Result<(), String> {
        if self.min == 0 || self.max < self.min {
            return Err(format!("{field} needs 0 < min <= max"));
        }
        Ok(())
    }
}

fn weight() -> u32 {
    1
}
fn is_one(value: &u32) -> bool {
    *value == 1
}

/// One kind of generated fault, named by the action it starts. A fault with a
/// duration or downtime is followed by its teardown. `weight` defaults to 1.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum FaultKind {
    /// Cut one target off from `from`, or from every other machine
    /// (the oracle included) when `from` is empty, then heal.
    Partition {
        #[serde(default = "weight", skip_serializing_if = "is_one")]
        weight: u32,
        duration_ns: DurationRange,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        from: Vec<String>,
    },
    /// Delay and drop the traffic leaving one target, then heal. Every
    /// latency in the range, with `jitter_ns`, must be a valid link policy;
    /// the network's latency floor is checked when the profile is sampled.
    Shape {
        #[serde(default = "weight", skip_serializing_if = "is_one")]
        weight: u32,
        duration_ns: DurationRange,
        latency_ns: DurationRange,
        #[serde(default, skip_serializing_if = "is_zero")]
        jitter_ns: u64,
        #[serde(default, skip_serializing_if = "is_zero")]
        loss_ppm: u64,
    },
    /// Destroy one target's VM, then reboot it.
    Crash {
        #[serde(default = "weight", skip_serializing_if = "is_one")]
        weight: u32,
        downtime_ns: DurationRange,
    },
    /// Freeze one target's whole VM, then resume it.
    Pause {
        #[serde(default = "weight", skip_serializing_if = "is_one")]
        weight: u32,
        duration_ns: DurationRange,
    },
    /// SIGSTOP one target's application, then continue it.
    PauseProcess {
        #[serde(default = "weight", skip_serializing_if = "is_one")]
        weight: u32,
        duration_ns: DurationRange,
    },
    /// Kill one target's application, then start it again.
    Stop {
        #[serde(default = "weight", skip_serializing_if = "is_one")]
        weight: u32,
        downtime_ns: DurationRange,
    },
    /// Step one target's wall clock by up to `max_offset_ns` either way, then
    /// step it back.
    ClockSkew {
        #[serde(default = "weight", skip_serializing_if = "is_one")]
        weight: u32,
        duration_ns: DurationRange,
        max_offset_ns: u64,
    },
    /// Run one target's clock up to `max_rate_ppb` fast or slow, then restore it.
    ClockDrift {
        #[serde(default = "weight", skip_serializing_if = "is_one")]
        weight: u32,
        duration_ns: DurationRange,
        max_rate_ppb: u64,
    },
    /// Scale one target's CPU cost by `factor_permille`, then heal.
    Throttle {
        #[serde(default = "weight", skip_serializing_if = "is_one")]
        weight: u32,
        duration_ns: DurationRange,
        factor_permille: u64,
    },
    /// A `disk_error` on one target, then heal. As in the action, `op` and
    /// `ppm` are required. Only machines with a retained disk can take it;
    /// `targets` narrows the profile's targets further.
    DiskError {
        #[serde(default = "weight", skip_serializing_if = "is_one")]
        weight: u32,
        duration_ns: DurationRange,
        op: DiskOp,
        ppm: u64,
        #[serde(default, skip_serializing_if = "DiskEffect::is_eio")]
        effect: DiskEffect,
        #[serde(default, skip_serializing_if = "DiskScope::is_all")]
        scope: DiskScope,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        targets: Vec<String>,
    },
    /// A `disk_latency` on one target with a latency drawn from `latency_ns`,
    /// then heal. As in the action, `op` is required. Only machines with a
    /// retained disk can take it.
    DiskLatency {
        #[serde(default = "weight", skip_serializing_if = "is_one")]
        weight: u32,
        duration_ns: DurationRange,
        latency_ns: DurationRange,
        op: DiskOp,
        #[serde(default, skip_serializing_if = "is_zero")]
        jitter_ns: u64,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        targets: Vec<String>,
    },
    /// A `preempt` on one target, then heal.
    Preempt {
        #[serde(default = "weight", skip_serializing_if = "is_one")]
        weight: u32,
        duration_ns: DurationRange,
        ppm: u64,
    },
}

impl FaultKind {
    /// The action this kind starts.
    pub fn name(&self) -> &'static str {
        match self {
            Self::Partition { .. } => "partition",
            Self::Shape { .. } => "shape",
            Self::Crash { .. } => "crash",
            Self::Pause { .. } => "pause",
            Self::PauseProcess { .. } => "pause_process",
            Self::Stop { .. } => "stop",
            Self::ClockSkew { .. } => "clock_skew",
            Self::ClockDrift { .. } => "clock_drift",
            Self::Throttle { .. } => "throttle",
            Self::DiskError { .. } => "disk_error",
            Self::DiskLatency { .. } => "disk_latency",
            Self::Preempt { .. } => "preempt",
        }
    }

    pub fn weight(&self) -> u32 {
        match self {
            Self::Partition { weight, .. }
            | Self::Shape { weight, .. }
            | Self::Crash { weight, .. }
            | Self::Pause { weight, .. }
            | Self::PauseProcess { weight, .. }
            | Self::Stop { weight, .. }
            | Self::ClockSkew { weight, .. }
            | Self::ClockDrift { weight, .. }
            | Self::Throttle { weight, .. }
            | Self::DiskError { weight, .. }
            | Self::DiskLatency { weight, .. }
            | Self::Preempt { weight, .. } => *weight,
        }
    }

    /// Every range is well formed, and every value the entry can draw is
    /// one its action accepts.
    fn validate(&self) -> Result<(), String> {
        match self {
            Self::Partition {
                duration_ns, from, ..
            } => {
                duration_ns.validate("duration_ns")?;
                if from.iter().any(String::is_empty) {
                    return Err("from must name machines".into());
                }
                Ok(())
            }
            Self::Pause { duration_ns, .. } | Self::PauseProcess { duration_ns, .. } => {
                duration_ns.validate("duration_ns")
            }
            Self::Crash { downtime_ns, .. } | Self::Stop { downtime_ns, .. } => {
                downtime_ns.validate("downtime_ns")
            }
            Self::Shape {
                duration_ns,
                latency_ns,
                jitter_ns,
                loss_ppm,
                ..
            } => {
                duration_ns.validate("duration_ns")?;
                latency_ns.validate("latency_ns")?;
                // The same checks as every shape action, at both ends of the
                // range; the floor is the world's, so it waits for sampling.
                for latency in [latency_ns.min, latency_ns.max] {
                    control::validate_link(latency, *jitter_ns, *loss_ppm, None, None, 0)?;
                }
                Ok(())
            }
            Self::ClockSkew {
                duration_ns,
                max_offset_ns,
                ..
            } => {
                duration_ns.validate("duration_ns")?;
                control::validate_clock_skew(None, Some(*max_offset_ns))
            }
            Self::ClockDrift {
                duration_ns,
                max_rate_ppb,
                ..
            } => {
                duration_ns.validate("duration_ns")?;
                control::validate_clock_drift(None, Some(*max_rate_ppb))
            }
            Self::Throttle {
                duration_ns,
                factor_permille,
                ..
            } => {
                duration_ns.validate("duration_ns")?;
                if !control::THROTTLE_PERMILLE.contains(factor_permille) {
                    return Err(format!(
                        "factor_permille must be {} to {}",
                        control::THROTTLE_PERMILLE.start(),
                        control::THROTTLE_PERMILLE.end()
                    ));
                }
                Ok(())
            }
            Self::DiskError {
                duration_ns,
                op,
                ppm,
                effect,
                scope,
                targets,
                ..
            } => {
                duration_ns.validate("duration_ns")?;
                if targets.iter().any(String::is_empty) {
                    return Err("targets must name machines".into());
                }
                control::validate_disk_error(*op, *ppm, *effect, *scope)
            }
            Self::DiskLatency {
                duration_ns,
                latency_ns,
                jitter_ns,
                targets,
                ..
            } => {
                duration_ns.validate("duration_ns")?;
                latency_ns.validate("latency_ns")?;
                if targets.iter().any(String::is_empty) {
                    return Err("targets must name machines".into());
                }
                control::validate_disk_latency(latency_ns.min, *jitter_ns)?;
                control::validate_disk_latency(latency_ns.max, *jitter_ns)
            }
            Self::Preempt {
                duration_ns, ppm, ..
            } => {
                duration_ns.validate("duration_ns")?;
                control::validate_preempt(*ppm)
            }
        }
    }
}

#[cfg(test)]
mod tests;
