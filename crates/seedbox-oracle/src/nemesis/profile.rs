//! A fault profile: the faults an oracle wants, written as data. Which kinds,
//! on which machines, in what window, how often and with what ranges; the
//! oracle ships it (YAML or JSON) and runs it as its [`Nemesis`].
//!
//! Each entry is named by the [`Action`] it starts and carries that action's
//! own snake_case fields, with `{min, max}` ranges where the profile draws a
//! value. [`Profile::validate`] checks every entry with its action's own
//! validator, so an entry is refused with the message the action would get.
//! Checks that need the world, such as the network's latency floor, are the
//! world's when the action is applied.
use crate::{
    control::{self, Action},
    patterns::{InForce, Sleep},
    random::RandomExt,
    Error, Nemesis, Pace, Random, World,
};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeSet, time::Duration};

/// The fault count a profile allows when it does not say.
pub const DEFAULT_MAX_FAULTS: u32 = 64;

/// Faults start between `start_ns` and `end_ns` after [`Nemesis::run`]
/// begins, one gap apart; each gap is drawn uniformly from half to one and a
/// half times `interval_ns`. Each start draws an entry by weight, then a
/// target among `targets` with no fault of this profile in force (a start
/// with none free is skipped), then the entry's ranges, and holds the fault
/// for its drawn duration before ending it. At most `max_faults` start. `run`
/// returns at `end_ns`, or once the last fault ends if that is later; faults
/// still in force when a failure cuts it short are ended by
/// [`Nemesis::heal`].
///
/// Every draw comes from the scenario's [`Random`] in that order, so a seed
/// fixes the applied sequence.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Profile {
    #[serde(default, skip_serializing_if = "is_zero")]
    pub start_ns: u64,
    pub end_ns: u64,
    pub interval_ns: u64,
    #[serde(
        default = "default_max_faults",
        skip_serializing_if = "is_default_max_faults"
    )]
    pub max_faults: u32,
    /// The machines faults target, by name. The oracle's world cannot list
    /// machines, so the profile names them.
    pub targets: Vec<String>,
    /// The kinds of fault to draw, by weight.
    pub faults: Vec<Fault>,
    #[serde(skip)]
    state: State,
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
fn weight() -> u32 {
    1
}
fn is_one(value: &u32) -> bool {
    *value == 1
}

/// What a running profile has in force and still has to end.
struct State {
    faults: InForce,
    /// Teardowns not yet due, in the order their faults started.
    ending: Vec<Ending>,
    /// Targets with a fault of this profile in force.
    busy: BTreeSet<String>,
    pace: Box<dyn Pace>,
}

impl Default for State {
    fn default() -> Self {
        Self {
            faults: InForce::default(),
            ending: Vec::new(),
            busy: BTreeSet::new(),
            pace: Box::new(Sleep),
        }
    }
}

impl std::fmt::Debug for State {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("State")
            .field("faults", &self.faults)
            .field("ending", &self.ending)
            .field("busy", &self.busy)
            .finish_non_exhaustive()
    }
}

#[derive(Debug)]
struct Ending {
    at_ns: u64,
    action: Action,
    target: String,
}

impl Profile {
    /// Wait through `pace` instead of the world's sleep.
    pub fn with_pace(mut self, pace: impl Pace + 'static) -> Self {
        self.state.pace = Box::new(pace);
        self
    }

    /// The profile's own bounds, then every entry with its action's
    /// validator. [`Nemesis::run`] checks this before applying anything.
    pub fn validate(&self) -> Result<(), Error> {
        let refuse = |message: String| Err(Error(format!("fault profile {message}")));
        if self.end_ns <= self.start_ns {
            return refuse("end_ns must be after its start_ns".into());
        }
        if self.interval_ns == 0 {
            return refuse("interval_ns must be positive".into());
        }
        if self.max_faults == 0 {
            return refuse("max_faults must be positive".into());
        }
        if self.targets.is_empty() || self.targets.iter().any(String::is_empty) {
            return refuse("targets must name at least one machine".into());
        }
        let distinct: BTreeSet<&String> = self.targets.iter().collect();
        if distinct.len() != self.targets.len() {
            return refuse("targets must be distinct".into());
        }
        if self.faults.iter().all(|fault| fault.weight() == 0) {
            return refuse("needs a fault kind with positive weight".into());
        }
        for fault in &self.faults {
            if let Err(error) = fault.validate(&self.targets) {
                return refuse(format!("{}: {error}", fault.name()));
            }
        }
        Ok(())
    }

    fn until(&mut self, world: &mut dyn World, at_ns: u64) -> Result<(), Error> {
        let now = world.now_ns()?;
        if at_ns > now {
            self.state
                .pace
                .wait(world, Duration::from_nanos(at_ns - now))?;
        }
        Ok(())
    }

    fn gap(&self, random: &mut dyn Random) -> u64 {
        let half = self.interval_ns / 2;
        random.range(
            self.interval_ns - half,
            self.interval_ns.saturating_add(half),
        )
    }

    /// Draw and apply one fault; false when every target is busy.
    fn start(&mut self, world: &mut dyn World, random: &mut dyn Random) -> Result<bool, Error> {
        let total: u64 = self.faults.iter().map(|f| u64::from(f.weight())).sum();
        let mut pick = random.below(total);
        let fault = self
            .faults
            .iter()
            .find(|fault| {
                let weight = u64::from(fault.weight());
                if pick < weight {
                    return true;
                }
                pick -= weight;
                false
            })
            .expect("a draw below the total weight lands on an entry");
        let free: Vec<String> = self
            .targets
            .iter()
            .filter(|target| !self.state.busy.contains(*target))
            .cloned()
            .collect();
        if free.is_empty() {
            return Ok(false);
        }
        let target = random.choice(&free).clone();
        let drawn = fault.draw(&target, &self.targets, random, &mut self.state.faults);
        self.state.faults.apply(world, &drawn.action)?;
        let applied = world.now_ns()?;
        self.state.busy.insert(target.clone());
        self.state.ending.push(Ending {
            at_ns: applied.saturating_add(drawn.hold_ns),
            action: drawn.teardown,
            target,
        });
        Ok(true)
    }
}

impl Nemesis for Profile {
    fn run(&mut self, world: &mut dyn World, random: &mut dyn Random) -> Result<(), Error> {
        self.validate()?;
        let origin = world.now_ns()?;
        let end = origin.saturating_add(self.end_ns);
        let mut next = origin
            .saturating_add(self.start_ns)
            .saturating_add(self.gap(random));
        let mut started = 0;
        loop {
            let start = (started < self.max_faults && next < end).then_some(next);
            // The first fault to end, of those due together.
            let ending = self
                .state
                .ending
                .iter()
                .enumerate()
                .min_by_key(|(_, ending)| ending.at_ns)
                .map(|(index, ending)| (index, ending.at_ns));
            match (start, ending) {
                (_, Some((index, at))) if start.is_none_or(|start| at <= start) => {
                    self.until(world, at)?;
                    let action = self.state.ending[index].action.clone();
                    self.state.faults.apply(world, &action)?;
                    let ended = self.state.ending.remove(index);
                    self.state.busy.remove(&ended.target);
                }
                (Some(at), _) => {
                    self.until(world, at)?;
                    if self.start(world, random)? {
                        started += 1;
                    }
                    next = at.saturating_add(self.gap(random));
                }
                (None, _) => break,
            }
        }
        self.until(world, end)
    }

    fn heal(&mut self, world: &mut dyn World) -> Result<(), Error> {
        self.state.ending.clear();
        self.state.busy.clear();
        self.state.faults.heal(world)
    }
}

/// An inclusive range, drawn uniformly.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Range {
    pub min: u64,
    pub max: u64,
}

impl Range {
    fn validate(&self, field: &str) -> Result<(), String> {
        if self.min == 0 || self.max < self.min {
            return Err(format!("{field} needs 0 < min <= max"));
        }
        Ok(())
    }

    fn draw(&self, random: &mut dyn Random) -> u64 {
        random.range(self.min, self.max)
    }
}

/// A nonzero value within `max` either way.
fn signed(random: &mut dyn Random, max: u64) -> i64 {
    let magnitude = random.range(1, max) as i64;
    if random.below(2) == 0 {
        magnitude
    } else {
        -magnitude
    }
}

/// One kind of fault, named by the action it starts and ended by that
/// action's counterpart: a heal, resume, reboot or restart. `weight`
/// defaults to 1.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Fault {
    /// Cut one target off from `from`, or from the other targets when `from`
    /// is empty, then heal.
    Partition {
        #[serde(default = "weight", skip_serializing_if = "is_one")]
        weight: u32,
        duration_ns: Range,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        from: Vec<String>,
    },
    /// Delay and drop the traffic from one target to `to`, or to the other
    /// targets when `to` is empty, then heal. Every latency in the range,
    /// with `jitter_ns` and `loss_ppm`, must be a valid link policy.
    Shape {
        #[serde(default = "weight", skip_serializing_if = "is_one")]
        weight: u32,
        duration_ns: Range,
        latency_ns: Range,
        #[serde(default, skip_serializing_if = "is_zero")]
        jitter_ns: u64,
        #[serde(default, skip_serializing_if = "is_zero")]
        loss_ppm: u64,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        to: Vec<String>,
    },
    /// Destroy one target's VM, then reboot it.
    Crash {
        #[serde(default = "weight", skip_serializing_if = "is_one")]
        weight: u32,
        downtime_ns: Range,
    },
    /// Freeze one target's whole VM, then resume it.
    Pause {
        #[serde(default = "weight", skip_serializing_if = "is_one")]
        weight: u32,
        duration_ns: Range,
    },
    /// SIGSTOP one target's application, then continue it.
    PauseProcess {
        #[serde(default = "weight", skip_serializing_if = "is_one")]
        weight: u32,
        duration_ns: Range,
    },
    /// Kill one target's application, then start it again.
    Stop {
        #[serde(default = "weight", skip_serializing_if = "is_one")]
        weight: u32,
        downtime_ns: Range,
    },
    /// Step one target's wall clock by a nonzero offset drawn within
    /// `max_offset_ns` either way, then heal.
    ClockSkew {
        #[serde(default = "weight", skip_serializing_if = "is_one")]
        weight: u32,
        duration_ns: Range,
        max_offset_ns: u64,
    },
    /// Run one target's clock fast or slow by a nonzero rate drawn within
    /// `max_rate_ppb`, then heal.
    ClockDrift {
        #[serde(default = "weight", skip_serializing_if = "is_one")]
        weight: u32,
        duration_ns: Range,
        max_rate_ppb: u64,
    },
    /// Scale one target's CPU cost by `factor_permille`, then heal.
    Throttle {
        #[serde(default = "weight", skip_serializing_if = "is_one")]
        weight: u32,
        duration_ns: Range,
        factor_permille: u64,
    },
}

/// A drawn fault: the action that starts it, the one that ends it, and how
/// long it holds.
struct Drawn {
    action: Action,
    teardown: Action,
    hold_ns: u64,
}

/// `side` without `target`, or the other targets when `side` is empty.
fn far_side(side: &[String], target: &str, targets: &[String]) -> Vec<String> {
    let side = if side.is_empty() { targets } else { side };
    side.iter().filter(|m| *m != target).cloned().collect()
}

impl Fault {
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
            | Self::Throttle { weight, .. } => *weight,
        }
    }

    /// Every range is well formed, and every value the entry can draw, on
    /// every target, is one its action accepts.
    fn validate(&self, targets: &[String]) -> Result<(), String> {
        let sides = |field: &str, side: &[String]| -> Result<(), String> {
            if side.iter().any(String::is_empty) {
                return Err(format!("{field} must name machines"));
            }
            match targets
                .iter()
                .find(|target| far_side(side, target, targets).is_empty())
            {
                Some(target) => Err(format!("{field} needs a machine other than {target}")),
                None => Ok(()),
            }
        };
        match self {
            Self::Partition {
                duration_ns, from, ..
            } => {
                duration_ns.validate("duration_ns")?;
                sides("from", from)
            }
            Self::Shape {
                duration_ns,
                latency_ns,
                jitter_ns,
                loss_ppm,
                to,
                ..
            } => {
                duration_ns.validate("duration_ns")?;
                latency_ns.validate("latency_ns")?;
                // The same checks as every shape action, at both ends of
                // the range; the floor is the world's, checked on apply.
                for latency in [latency_ns.min, latency_ns.max] {
                    control::validate_link(latency, *jitter_ns, *loss_ppm, None, None, 0)?;
                }
                sides("to", to)
            }
            Self::Pause { duration_ns, .. } | Self::PauseProcess { duration_ns, .. } => {
                duration_ns.validate("duration_ns")
            }
            Self::Crash { downtime_ns, .. } | Self::Stop { downtime_ns, .. } => {
                downtime_ns.validate("downtime_ns")
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
                control::validate_throttle(*factor_permille)
            }
        }
    }

    /// Draw this fault on `target`: its duration first, then its values.
    fn draw(
        &self,
        target: &str,
        targets: &[String],
        random: &mut dyn Random,
        faults: &mut InForce,
    ) -> Drawn {
        let machine = target.to_owned();
        let mut handle = || faults.name(self.name());
        let healed = |handle: &String| Action::Heal {
            handle: handle.clone(),
        };
        match self {
            Self::Partition {
                duration_ns, from, ..
            } => {
                let handle = handle();
                Drawn {
                    hold_ns: duration_ns.draw(random),
                    teardown: healed(&handle),
                    action: Action::Partition {
                        handle,
                        left: vec![machine],
                        right: far_side(from, target, targets),
                    },
                }
            }
            Self::Shape {
                duration_ns,
                latency_ns,
                jitter_ns,
                loss_ppm,
                to,
                ..
            } => {
                let handle = handle();
                Drawn {
                    hold_ns: duration_ns.draw(random),
                    teardown: healed(&handle),
                    action: Action::Shape {
                        handle,
                        from: vec![machine],
                        to: far_side(to, target, targets),
                        latency_ns: latency_ns.draw(random),
                        jitter_ns: *jitter_ns,
                        loss_ppm: *loss_ppm,
                        bandwidth_bps: None,
                        queue_bound_ns: None,
                        network_faults: control::NetworkFaults::default(),
                    },
                }
            }
            Self::Crash { downtime_ns, .. } => Drawn {
                hold_ns: downtime_ns.draw(random),
                action: Action::Crash {
                    machine: machine.clone(),
                },
                teardown: Action::Reboot { machine },
            },
            Self::Pause { duration_ns, .. } => Drawn {
                hold_ns: duration_ns.draw(random),
                action: Action::Pause {
                    machine: machine.clone(),
                },
                teardown: Action::Resume { machine },
            },
            Self::PauseProcess { duration_ns, .. } => Drawn {
                hold_ns: duration_ns.draw(random),
                action: Action::PauseProcess {
                    machine: machine.clone(),
                },
                teardown: Action::ResumeProcess { machine },
            },
            Self::Stop { downtime_ns, .. } => Drawn {
                hold_ns: downtime_ns.draw(random),
                action: Action::Stop {
                    machine: machine.clone(),
                },
                teardown: Action::Restart { machine },
            },
            Self::ClockSkew {
                duration_ns,
                max_offset_ns,
                ..
            } => {
                let handle = handle();
                Drawn {
                    hold_ns: duration_ns.draw(random),
                    teardown: healed(&handle),
                    action: Action::ClockSkew {
                        handle,
                        machine,
                        offset_ns: Some(signed(random, *max_offset_ns)),
                        max_offset_ns: None,
                    },
                }
            }
            Self::ClockDrift {
                duration_ns,
                max_rate_ppb,
                ..
            } => {
                let handle = handle();
                Drawn {
                    hold_ns: duration_ns.draw(random),
                    teardown: healed(&handle),
                    action: Action::ClockDrift {
                        handle,
                        machine,
                        rate_ppb: Some(signed(random, *max_rate_ppb)),
                        max_rate_ppb: None,
                    },
                }
            }
            Self::Throttle {
                duration_ns,
                factor_permille,
                ..
            } => {
                let handle = handle();
                Drawn {
                    hold_ns: duration_ns.draw(random),
                    teardown: healed(&handle),
                    action: Action::Throttle {
                        handle,
                        machine,
                        factor_permille: *factor_permille,
                    },
                }
            }
        }
    }
}

#[cfg(test)]
mod tests;
