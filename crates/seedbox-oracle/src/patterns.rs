//! Jepsen's standard fault schedules over the [`World`]'s actions: kill and
//! restart rounds, process pauses, partition rounds and coordinated power
//! failures. Every choice is drawn from the scenario's [`Random`] and every
//! fault still in force is recorded, so [`Nemesis::heal`] undoes it. A
//! pattern given too few nodes to act on returns an error.
use crate::{control::Action, random::RandomExt, Error, Nemesis, Pace, Random, World};
use std::{collections::BTreeSet, time::Duration};

/// Wait with the world's simulated sleep.
pub struct Sleep;

impl Pace for Sleep {
    fn wait(&mut self, world: &mut dyn World, duration: Duration) -> Result<(), Error> {
        if duration.is_zero() {
            return Ok(());
        }
        world.sleep(duration)
    }
}

/// Jepsen's target sizes for a round's victims.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Subset {
    One,
    Minority,
    Majority,
    All,
}

impl Subset {
    pub const ALL: [Subset; 4] = [Self::One, Self::Minority, Self::Majority, Self::All];

    pub fn count(self, nodes: usize) -> usize {
        match self {
            Self::One => 1,
            Self::Minority => nodes.saturating_sub(1) / 2,
            Self::Majority => nodes / 2 + 1,
            Self::All => nodes,
        }
        .clamp(1, nodes.max(1))
    }
}

/// Refuse a pattern that has too few nodes to act on.
fn need(nodes: &[String], at_least: usize, pattern: &str) -> Result<(), Error> {
    if nodes.len() < at_least {
        return Err(Error(format!(
            "{pattern} needs at least {at_least} nodes, has {}",
            nodes.len()
        )));
    }
    Ok(())
}

/// A random subset of `nodes` of the given size, sorted.
pub fn pick(random: &mut dyn Random, nodes: &[String], subset: Subset) -> Vec<String> {
    let mut chosen = random.sample(nodes, subset.count(nodes.len()));
    chosen.sort();
    chosen
}

/// How a partition round splits the nodes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Split {
    /// A random minority from the rest.
    Minority,
    /// Shuffled halves, as even as possible.
    Halves,
    /// One random node from every other.
    Isolate,
}

impl Split {
    /// The two sides, each sorted and nonempty for two or more nodes. Fewer
    /// nodes make an empty side; [`Partitions`] refuses them.
    pub fn sides(self, random: &mut dyn Random, nodes: &[String]) -> (Vec<String>, Vec<String>) {
        let left = match self {
            Self::Minority => pick(random, nodes, Subset::Minority),
            Self::Halves => {
                let mut shuffled = nodes.to_vec();
                random.shuffle(&mut shuffled);
                shuffled.truncate(nodes.len() / 2);
                shuffled
            }
            Self::Isolate => vec![random.choice(nodes).clone()],
        };
        let mut left: Vec<String> = left;
        left.sort();
        let right = nodes
            .iter()
            .filter(|n| !left.contains(n))
            .cloned()
            .collect();
        (left, right)
    }
}

/// Faults in force, undone together. Every action goes through
/// [`InForce::apply`], which records what it installs.
#[derive(Debug, Default)]
pub struct InForce {
    handles: Vec<String>,
    stopped: BTreeSet<String>,
    paused_processes: BTreeSet<String>,
    paused: BTreeSet<String>,
    crashed: BTreeSet<String>,
    issued: u64,
}

impl InForce {
    /// A fresh handle name for this nemesis's next fault.
    pub fn name(&mut self, kind: &str) -> String {
        self.issued += 1;
        format!("{kind}-{}", self.issued)
    }

    /// Apply `action` and record its effect.
    pub fn apply(&mut self, world: &mut dyn World, action: &Action) -> Result<(), Error> {
        world.apply(action)?;
        let set = |set: &mut BTreeSet<String>, machine: &str, on: bool| {
            if on {
                set.insert(machine.into());
            } else {
                set.remove(machine);
            }
        };
        match action {
            Action::Stop { machine } => set(&mut self.stopped, machine, true),
            Action::Restart { machine } => set(&mut self.stopped, machine, false),
            Action::PauseProcess { machine } => set(&mut self.paused_processes, machine, true),
            Action::ResumeProcess { machine } => set(&mut self.paused_processes, machine, false),
            Action::Pause { machine } => set(&mut self.paused, machine, true),
            Action::Resume { machine } => set(&mut self.paused, machine, false),
            Action::Crash { machine } => set(&mut self.crashed, machine, true),
            Action::Reboot { machine } => set(&mut self.crashed, machine, false),
            Action::Heal { handle } => self.handles.retain(|h| h != handle),
            Action::Partition { handle, .. }
            | Action::Shape { handle, .. }
            | Action::DropConnection { handle, .. }
            | Action::ClockSkew { handle, .. }
            | Action::ClockDrift { handle, .. }
            | Action::Throttle { handle, .. }
            | Action::DiskError { handle, .. }
            | Action::DiskLatency { handle, .. }
            | Action::Preempt { handle, .. } => self.handles.push(handle.clone()),
        }
        Ok(())
    }

    /// Apply one lifecycle action to each machine in order.
    pub fn each(
        &mut self,
        world: &mut dyn World,
        machines: &[String],
        action: impl Fn(String) -> Action,
    ) -> Result<(), Error> {
        for machine in machines {
            self.apply(world, &action(machine.clone()))?;
        }
        Ok(())
    }

    /// Whether nothing is in force.
    pub fn is_empty(&self) -> bool {
        self.handles.is_empty()
            && self.stopped.is_empty()
            && self.paused_processes.is_empty()
            && self.paused.is_empty()
            && self.crashed.is_empty()
    }

    /// Undo everything still in force, newest handle first: heal handles,
    /// resume paused machines, reboot crashed ones, then resume and restart
    /// processes. Every undo is tried; the first failure is returned.
    pub fn heal(&mut self, world: &mut dyn World) -> Result<(), Error> {
        let mut undo: Vec<Action> = Vec::new();
        undo.extend(self.handles.iter().rev().map(|handle| Action::Heal {
            handle: handle.clone(),
        }));
        let machines = |set: &BTreeSet<String>, action: fn(String) -> Action| {
            set.iter().cloned().map(action).collect::<Vec<_>>()
        };
        undo.extend(machines(&self.paused, |machine| Action::Resume { machine }));
        undo.extend(machines(&self.crashed, |machine| Action::Reboot {
            machine,
        }));
        undo.extend(machines(&self.paused_processes, |machine| {
            Action::ResumeProcess { machine }
        }));
        undo.extend(machines(&self.stopped, |machine| Action::Restart {
            machine,
        }));
        let mut first = None;
        for action in undo {
            if let Err(error) = self.apply(world, &action) {
                first.get_or_insert(Error(format!("{}: {error}", action.name())));
            }
        }
        first.map_or(Ok(()), Err)
    }
}

/// When a schedule injects faults: after `warm_up` of fault-free workload,
/// rounds for `hold`, then `tail` of fault-free workload. Durations are
/// simulated time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Phases {
    pub warm_up: Duration,
    pub hold: Duration,
    pub tail: Duration,
}

/// One round of a fault pattern: inject, wait, undo, wait.
pub trait Round {
    fn round(
        &mut self,
        faults: &mut InForce,
        world: &mut dyn World,
        pace: &mut dyn Pace,
        random: &mut dyn Random,
    ) -> Result<(), Error>;
}

/// A [`Nemesis`] that runs one [`Round`] pattern over [`Phases`].
pub struct Schedule<R> {
    pub phases: Phases,
    pub round: R,
    pub pace: Box<dyn Pace>,
    /// Rounds started so far.
    pub rounds: u64,
    faults: InForce,
}

impl<R: Round> Schedule<R> {
    pub fn new(phases: Phases, round: R) -> Self {
        Self {
            phases,
            round,
            pace: Box::new(Sleep),
            rounds: 0,
            faults: InForce::default(),
        }
    }

    /// Wait through `pace` instead of the world's sleep.
    pub fn with_pace(mut self, pace: impl Pace + 'static) -> Self {
        self.pace = Box::new(pace);
        self
    }
}

impl<R: Round> Nemesis for Schedule<R> {
    fn run(&mut self, world: &mut dyn World, random: &mut dyn Random) -> Result<(), Error> {
        self.pace.wait(world, self.phases.warm_up)?;
        let start = world.now_ns()?;
        let hold = u64::try_from(self.phases.hold.as_nanos()).unwrap_or(u64::MAX);
        while world.now_ns()?.saturating_sub(start) < hold {
            self.rounds += 1;
            self.round
                .round(&mut self.faults, world, self.pace.as_mut(), random)?;
        }
        self.pace.wait(world, self.phases.tail)
    }

    fn heal(&mut self, world: &mut dyn World) -> Result<(), Error> {
        self.faults.heal(world)
    }
}

/// No faults: the workload runs for the schedule's phases undisturbed.
pub struct Quiet;

impl Round for Quiet {
    fn round(
        &mut self,
        _: &mut InForce,
        world: &mut dyn World,
        pace: &mut dyn Pace,
        _: &mut dyn Random,
    ) -> Result<(), Error> {
        pace.wait(world, Duration::from_secs(1))
    }
}

/// Jepsen's kill nemesis: stop (SIGKILL) a random one, minority, majority
/// or all of `nodes`, wait `down`, restart them, wait `up`.
pub struct KillRestart {
    pub nodes: Vec<String>,
    pub down: (Duration, Duration),
    pub up: (Duration, Duration),
}

impl Round for KillRestart {
    fn round(
        &mut self,
        faults: &mut InForce,
        world: &mut dyn World,
        pace: &mut dyn Pace,
        random: &mut dyn Random,
    ) -> Result<(), Error> {
        need(&self.nodes, 1, "kill-restart")?;
        let subset = *random.choice(&Subset::ALL);
        let victims = pick(random, &self.nodes, subset);
        faults.each(world, &victims, |machine| Action::Stop { machine })?;
        pace.wait(world, random.duration(self.down.0, self.down.1))?;
        faults.each(world, &victims, |machine| Action::Restart { machine })?;
        pace.wait(world, random.duration(self.up.0, self.up.1))
    }
}

/// Jepsen's pause nemesis: SIGSTOP one random node's processes for
/// `length`, resume them, then `gap` of quiet.
pub struct ProcessPauses {
    pub nodes: Vec<String>,
    pub length: Duration,
    pub gap: Duration,
}

impl Round for ProcessPauses {
    fn round(
        &mut self,
        faults: &mut InForce,
        world: &mut dyn World,
        pace: &mut dyn Pace,
        random: &mut dyn Random,
    ) -> Result<(), Error> {
        need(&self.nodes, 1, "process pauses")?;
        let machine = random.choice(&self.nodes).clone();
        faults.apply(
            world,
            &Action::PauseProcess {
                machine: machine.clone(),
            },
        )?;
        pace.wait(world, self.length)?;
        faults.apply(world, &Action::ResumeProcess { machine })?;
        pace.wait(world, self.gap)
    }
}

/// Partition rounds: a [`Split`] cut for `length`, healed, then `gap`.
pub struct Partitions {
    pub nodes: Vec<String>,
    pub split: Split,
    pub length: (Duration, Duration),
    pub gap: (Duration, Duration),
}

impl Round for Partitions {
    fn round(
        &mut self,
        faults: &mut InForce,
        world: &mut dyn World,
        pace: &mut dyn Pace,
        random: &mut dyn Random,
    ) -> Result<(), Error> {
        need(&self.nodes, 2, "partitions")?;
        let (left, right) = self.split.sides(random, &self.nodes);
        let handle = faults.name("partition");
        faults.apply(
            world,
            &Action::Partition {
                handle: handle.clone(),
                left,
                right,
            },
        )?;
        pace.wait(world, random.duration(self.length.0, self.length.1))?;
        faults.apply(world, &Action::Heal { handle })?;
        pace.wait(world, random.duration(self.gap.0, self.gap.1))
    }
}

/// Coordinated power failure: crash every node, wait `down`, reboot every
/// node this round crashed, then wait `up`. A refused crash still reboots
/// the nodes already crashed before reporting it.
pub struct CrashAll {
    pub nodes: Vec<String>,
    pub down: Duration,
    pub up: Duration,
}

impl Round for CrashAll {
    fn round(
        &mut self,
        faults: &mut InForce,
        world: &mut dyn World,
        pace: &mut dyn Pace,
        _: &mut dyn Random,
    ) -> Result<(), Error> {
        need(&self.nodes, 1, "crash-all")?;
        let mut crashed = Vec::new();
        let mut failure = None;
        for machine in &self.nodes {
            match faults.apply(
                world,
                &Action::Crash {
                    machine: machine.clone(),
                },
            ) {
                Ok(()) => crashed.push(machine.clone()),
                Err(error) => {
                    failure = Some(Error(format!("crash {machine}: {error}")));
                    break;
                }
            }
        }
        if !crashed.is_empty() {
            pace.wait(world, self.down)?;
        }
        for machine in crashed {
            if let Err(error) = faults.apply(
                world,
                &Action::Reboot {
                    machine: machine.clone(),
                },
            ) {
                failure.get_or_insert(Error(format!("reboot {machine}: {error}")));
            }
        }
        if let Some(failure) = failure {
            return Err(failure);
        }
        pace.wait(world, self.up)
    }
}

#[cfg(test)]
mod tests;
