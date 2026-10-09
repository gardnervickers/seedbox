//! The windowed scheduler over the shared world: rounds planned by
//! [`crate::window`], machines executed serially or on a work-stealing pool,
//! and the world's policies (faults, runtime control, triggers, scenario
//! input, lifecycle, forks and checkpoints) carried out by the coordinator
//! between rounds. See docs/windowed-scheduler.md.
//!
//! Every decision is made on the coordinator thread from committed state in
//! a fixed order: machine reports are absorbed sorted by (time, machine,
//! channel order), world boundaries are applied when no live machine can act
//! before them, and the events they produce are queued as exact boundaries.
//! Worker count and host completion order never enter a decision.

use crate::links;
use crate::network_counters::{
    self, DELIVERED, LOSS_DROPPED, PARTITION_DROPPED, QUEUE_DROPPED, RECEIVER_DROPPED,
};
use crate::window as sw;
use crate::world::{self, Crash, Payload, SliceRequest, State, Stop, World, WorldMachine};
use crate::world_events as events;
use crate::{control, faults};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::fmt;
use std::time::{Duration, Instant};

/// The coordinator's errors are plain messages, like the machines' own.
impl From<sw::Error> for String {
    fn from(e: sw::Error) -> Self {
        e.to_string()
    }
}

/// What travels between the coordinator and a machine.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Msg {
    /// A frame on the wire, sent by `from`.
    Frame { from: usize, bytes: Vec<u8> },
    /// Scenario serial input: delivered up to `early` ahead, like a frame.
    Serial(Vec<u8>),
    /// A management command or control reply. Never delivered early: its
    /// time is an exact round boundary.
    Control(Vec<u8>),
    /// A scenario partition, applied when its sender delivers it, in order
    /// with the frames the sender routes.
    Partition { from: usize, to: usize, on: bool },
}

impl Msg {
    fn strict(&self) -> bool {
        matches!(self, Msg::Control(_))
    }
}

/// One segment of a run: to its end, to a fork point, or to a checkpoint.
pub struct Run<'a> {
    /// The world's configuration, for runtime inspection results.
    pub cfg: &'a Value,
    pub duration: u64,
    /// End the run when this machine exits.
    pub end_on: Option<&'a str>,
    /// Stop with `barrier` once every machine is parked at this fork point.
    pub fork: Option<Fork>,
    /// Stop with `checkpoint` once every machine and world boundary is at
    /// or after this time, before anything happens at it.
    pub checkpoint: Option<u64>,
    /// With `fork`, any other end is an error.
    pub require_barrier: bool,
}

/// Where every machine parks when a segment boots to a fork point.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fork {
    /// At the guest's launch barrier.
    Barrier,
    /// At each machine's first deterministic point at or after this time.
    At(u64),
}
impl Fork {
    fn at(self) -> Option<u64> {
        match self {
            Self::Barrier => None,
            Self::At(t) => Some(t),
        }
    }
}
/// `boot.fork_at_ns`: a time, or the launch barrier when absent.
impl From<Option<u64>> for Fork {
    fn from(at: Option<u64>) -> Self {
        at.map_or(Self::Barrier, Self::At)
    }
}

/// How a segment ended. Reports carry its snake_case name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RunOutcome {
    /// The run reached its duration, or only paused or down machines remain.
    Duration,
    /// No machine can act again.
    Quiescent,
    /// The machine named by `end_on` exited.
    EndMachineExited,
    /// Stopped before anything happens at the checkpoint time.
    Checkpoint,
    /// Every machine parked at the fork point.
    Barrier,
    /// A fault or control request failed the run.
    FaultFailed,
}
impl RunOutcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Duration => "duration",
            Self::Quiescent => "quiescent",
            Self::EndMachineExited => "end_machine_exited",
            Self::Checkpoint => "checkpoint",
            Self::Barrier => "barrier",
            Self::FaultFailed => "fault_failed",
        }
    }
}
impl fmt::Display for RunOutcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}
impl PartialEq<&str> for RunOutcome {
    fn eq(&self, other: &&str) -> bool {
        self.as_str() == *other
    }
}

/// Performance settings. None of them changes a run's results.
#[derive(Clone, Debug, Default)]
pub struct Workers {
    /// Machine threads; 1 runs every machine on the calling thread.
    pub count: usize,
    /// Host time to waste before each run of machine `i`, times `n - i`
    /// (testing only): later machines finish first.
    pub skew: Duration,
}

/// One machine as the window executor sees it. Effects it produces in a
/// round are kept here, in channel order, for the coordinator to absorb.
struct Guest<'a, M> {
    index: usize,
    m: &'a mut M,
    skew: Duration,
    /// Watch the console for an armed trigger this round.
    observe: bool,
    /// Park at the first deterministic point at or after this time.
    fork_at: Option<u64>,
    /// Receiver outcomes: (nominal time, sender, accepted).
    received: Vec<(u64, usize, bool)>,
    /// Fault effects: (time, id, success).
    effects: Vec<(u64, u32, bool)>,
    /// Runtime control requests: (time, frame).
    requests: Vec<(u64, Vec<u8>)>,
    /// Committed console bytes under observation: (time, byte).
    console: Vec<(u64, u8)>,
    /// Times a control reply reached the controller.
    replies: Vec<u64>,
    /// Exited in the last round.
    exited_now: bool,
}

impl<'a, M: WorldMachine> Guest<'a, M> {
    fn new(index: usize, m: &'a mut M, skew: Duration) -> Self {
        Self {
            index,
            m,
            skew,
            observe: false,
            fork_at: None,
            received: Vec::new(),
            effects: Vec::new(),
            requests: Vec::new(),
            console: Vec::new(),
            replies: Vec::new(),
            exited_now: false,
        }
    }

    fn suspended(&self) -> bool {
        self.m.parked || self.m.paused || self.m.down
    }

    fn exited_at(&self) -> Option<u64> {
        matches!(self.m.state, State::Exited(_)).then_some(self.m.time)
    }

    /// The machine's own next action, ignoring queued events.
    fn next(&self) -> u64 {
        if self.suspended() {
            return sw::NEVER;
        }
        match self.m.state {
            State::Exited(_) => sw::NEVER,
            State::Running => self.m.time,
            State::Halted if self.m.has_pending_irq() => self.m.time,
            State::Halted => self.m.next_deadline().max(self.m.time),
        }
    }

    fn state(&self) -> sw::MachineState {
        sw::MachineState {
            time: self.m.time,
            next: self.next(),
            exited: self.exited_at(),
            sent: 0,
            suspended: self.suspended() && self.exited_at().is_none(),
        }
    }

    /// Move the slice's committed effects into the round's record.
    fn drain(&mut self) {
        for (id, at, success) in std::mem::take(&mut self.m.fault_effects) {
            self.effects.push((at, id, success));
        }
        for (at, line) in std::mem::take(&mut self.m.control_requests) {
            self.requests.push((at, line));
        }
        if let Some((at, byte)) = self.m.committed_console.take() {
            self.console.push((at, byte));
        }
    }

    fn push(
        &self,
        a: &sw::Assignment<Msg>,
        outputs: &mut Vec<sw::Output<Msg>>,
        at: u64,
        payload: Msg,
    ) -> Result<(), String> {
        if outputs.len() == a.max_outputs {
            return Err(format!(
                "{}: more than {} outputs in one round",
                self.m.name, a.max_outputs
            ));
        }
        outputs.push(sw::Output {
            sent_at: at,
            payload,
        });
        Ok(())
    }

    /// Deliver the assigned events the machine's time has reached, in key
    /// order. With `slack`, a non-strict event may arrive `delivery_early`
    /// before its time; a strict one never does.
    fn deliver(
        &mut self,
        a: &sw::Assignment<Msg>,
        i: &mut usize,
        outputs: &mut Vec<sw::Output<Msg>>,
        slack: bool,
    ) -> Result<(), String> {
        while let Some(e) = a.incoming.get(*i).filter(|e| {
            let early = if slack && !e.payload.strict() {
                a.delivery_early
            } else {
                0
            };
            e.key.at <= self.m.time.saturating_add(early)
        }) {
            let now = self.m.time;
            match &e.payload {
                Msg::Frame { from, bytes } => {
                    let accepted = self.m.deliver_frame(bytes)?;
                    self.received.push((e.key.at, *from, accepted));
                }
                Msg::Serial(bytes) => self.m.deliver_serial(bytes)?,
                Msg::Control(bytes) => {
                    if self.m.has_control() {
                        self.m.deliver_control(bytes)?;
                    } else {
                        self.m.deliver_serial(bytes)?;
                    }
                    if bytes.starts_with(b"SBX_REPLY1 ") {
                        self.replies.push(now);
                    }
                }
                Msg::Partition { .. } => {
                    // Partition state is consulted when the sender routes:
                    // the coordinator applies it in order with the frames.
                    self.push(a, outputs, now, e.payload.clone())?;
                }
            }
            *i += 1;
        }
        Ok(())
    }
}

impl<M: WorldMachine> sw::Machine<Msg> for Guest<'_, M> {
    type Context = M::Clock;

    fn run(
        &mut self,
        clock: &mut M::Clock,
        a: &sw::Assignment<Msg>,
    ) -> Result<sw::Report<Msg>, String> {
        if !self.skew.is_zero() {
            std::thread::sleep(self.skew);
        }
        let slice_end = a.slice_end();
        let mut outputs = Vec::new();
        let mut frames = Vec::new();
        let mut i = 0;
        let mut exited = None;
        let mut suspended = false;
        let mut stalled = None;
        loop {
            if matches!(self.m.state, State::Exited(_)) {
                exited = Some(self.m.time);
                break;
            }
            if self.suspended() {
                suspended = true;
                break;
            }
            // Park at the fork time, or as soon after it as owed I/O cost
            // allows; the coordinator parks machines that cannot act before it.
            if self.fork_at.is_some_and(|t| self.m.time >= t) && self.m.owed_io().is_none() {
                self.m.park()?;
                suspended = true;
                break;
            }
            // An event the machine has already passed is due now, whatever its
            // state and even when it stands at or past the slice end: a
            // skewed release or a resumed segment can leave it there.
            self.deliver(a, &mut i, &mut outputs, false)?;
            if self.m.state == State::Halted {
                // Wake at the machine's own next action or the next event's
                // nominal time, as the reference scheduler's key does. An
                // idle machine that cannot act before the slice end stays.
                let event = a.incoming.get(i).map_or(sw::NEVER, |e| e.key.at);
                let wake = self.next().min(event);
                if wake >= slice_end {
                    break;
                }
                self.m.wake(wake)?;
            }
            if self.m.time >= slice_end {
                break;
            }
            self.deliver(a, &mut i, &mut outputs, true)?;
            let (event, strict) = a
                .incoming
                .get(i)
                .map_or((sw::NEVER, false), |e| (e.key.at, e.payload.strict()));
            let mut limit = a.stop.min(event);
            let mut early = if limit == a.stop {
                a.stop_early
            } else if strict {
                0
            } else {
                a.delivery_early
            };
            if self.fork_at.is_some_and(|t| self.m.time >= t) {
                // Owed I/O is settled before parking, exactly.
                let owed = self.m.owed_io().unwrap_or(0);
                limit = limit.min(self.m.time.saturating_add(owed));
                early = 0;
            }
            let before = (self.m.time, i, outputs.len());
            self.m.observe_console = self.observe;
            let request = SliceRequest {
                hard_limit: limit,
                early_slack: early,
                observe_console: self.observe,
            };
            let observation = world::SliceObservation::begin(self.m, request)?;
            let stop = self.m.advance(clock, request, &mut frames)?;
            observation.validate(self.m, request, stop)?;
            self.drain();
            let now = self.m.time;
            for bytes in frames.drain(..) {
                let from = self.index;
                self.push(a, &mut outputs, now, Msg::Frame { from, bytes })?;
            }
            match stop {
                Stop::Exited => {
                    exited = Some(self.m.time);
                    self.exited_now = true;
                    break;
                }
                Stop::Barrier => {
                    // Parked at the guest's launch barrier.
                    suspended = true;
                    break;
                }
                Stop::Reached | Stop::Halted | Stop::Sent | Stop::Control => {}
            }
            // Equal-time returns are legal (a send, an early event, a control
            // request), but the identical stalled state twice is a scheduler bug.
            if (self.m.time, i, outputs.len()) == before
                && matches!(stop, Stop::Reached | Stop::Halted)
            {
                if stalled == Some(before) {
                    return Err(format!(
                        "{}: no progress at {} toward {limit}",
                        self.m.name, self.m.time
                    ));
                }
                stalled = Some(before);
            } else {
                stalled = None;
            }
        }
        Ok(sw::Report {
            time: self.m.time,
            next: if exited.is_some() {
                sw::NEVER
            } else {
                self.next()
            },
            consumed: i,
            outputs,
            exited,
            suspended,
        })
    }
}

/// The coordinator: the world's policies and their randomness, applied on
/// the calling thread in commit order, and the between-rounds hook.
struct Coord<'a, M: WorldMachine> {
    run: &'a Run<'a>,
    names: Vec<String>,
    faults: &'a mut faults::Faults,
    control: &'a mut control::Control,
    factory: &'a M::Factory,
    boot: &'a [M::Boot],
    run_seed: u64,
    branch_identity: Option<String>,
    incarnations: &'a mut [Vec<Value>],
    paused_ingress: &'a mut [u64],
    partitioned: &'a mut BTreeMap<(usize, usize), bool>,
    macs: &'a [[u8; 6]],
    dropped: &'a mut u64,
    delivered: &'a mut u64,
    link_counts: &'a mut network_counters::LinkCounts,
    semantic: &'a mut network_counters::NetworkCounters,
    latency: u64,
    /// Each machine's committed lifecycle: (paused, down).
    life: Vec<(bool, bool)>,
    /// Why the hook stopped the run.
    stopped: Option<RunOutcome>,
    /// A failed crash transaction: the world is terminal.
    failure: Option<String>,
    host: &'a mut dyn FnMut() -> Result<(), String>,
}

impl<M: WorldMachine> Coord<'_, M> {
    fn count(&mut self, at: u64, from: usize, to: usize, outcome: usize) {
        self.link_counts.entry((from, to)).or_default()[outcome] += 1;
        self.semantic.record(at, from, to, outcome);
    }

    fn counter_snapshot(&self) -> Value {
        network_counters::snapshot(|m| &self.names[m], self.semantic.counts())
    }

    fn fault_failed(&self) -> bool {
        (self.control.error.is_some()
            || (self.faults.error.is_some() && self.control.pending.is_none()))
            && !self.control.has_undelivered_reply()
    }

    /// Absorb the last round's effects, in a fixed order: receiver outcomes,
    /// fault effects, console bytes, exits, delivered replies, then requests.
    fn absorb(&mut self, guests: &mut [Guest<'_, M>], committed: u64) -> Result<(), String> {
        let mut received = Vec::new();
        let mut effects = Vec::new();
        let mut console = Vec::new();
        let mut replies = Vec::new();
        let mut requests = Vec::new();
        for (m, g) in guests.iter_mut().enumerate() {
            received.extend(g.received.drain(..).map(|(at, from, ok)| (at, from, m, ok)));
            effects.extend(
                g.effects
                    .drain(..)
                    .enumerate()
                    .map(|(j, (at, id, ok))| (at, m, j, id, ok)),
            );
            console.extend(
                g.console
                    .drain(..)
                    .enumerate()
                    .map(|(j, (at, byte))| (at, m, j, byte)),
            );
            replies.extend(g.replies.drain(..).map(|at| (at, m)));
            requests.extend(
                g.requests
                    .drain(..)
                    .enumerate()
                    .map(|(j, (at, line))| (at, m, j, line)),
            );
        }
        received.sort();
        for (at, from, to, accepted) in received {
            if accepted {
                *self.delivered += 1;
                self.count(at, from, to, DELIVERED);
            } else {
                *self.dropped += 1;
                self.count(at, from, to, RECEIVER_DROPPED);
            }
        }
        effects.sort();
        for (at, m, _, id, success) in effects {
            if id == 0 && success {
                if let Err(error) = self.faults.management_ready(m, at, self.latency) {
                    self.faults.error = Some(error);
                }
            } else {
                self.faults.effect(m, id, at, success);
            }
        }
        console.sort();
        for (at, m, _, byte) in console {
            self.control.triggers.committed(
                &self.names[m],
                at,
                byte,
                self.control.latency,
                committed,
            )?;
        }
        for (m, g) in guests.iter_mut().enumerate() {
            if !std::mem::take(&mut g.exited_now) {
                continue;
            }
            let ids: Vec<u32> = self
                .control
                .triggers
                .records
                .iter()
                .filter_map(|r| r.fault_id)
                .collect();
            for id in ids {
                self.faults.target_exited(id, m, g.m.time);
            }
            if let Some(id) = self.control.pending.as_ref().and_then(|p| p.fault) {
                self.faults.target_exited(id, m, g.m.time);
            }
        }
        replies.sort();
        for (at, _) in replies {
            self.control.mark_delivered(at)?;
        }
        requests.sort();
        for (at, m, _, line) in requests {
            if self.run.fork.is_some() {
                return Err(world::control_before_fork(&self.names[m], at));
            }
            if let Err(e) = self.control.request(m, at, &line, committed) {
                self.control.reject_request(&self.names[m], at, &line, e);
            }
        }
        self.control.observe_faults(self.faults, committed)?;
        Ok(())
    }

    /// Carry out a lifecycle transition at world boundary `t`, before any
    /// machine runs at `t`.
    fn lifecycle(
        &mut self,
        plan: &mut sw::World<Msg>,
        guests: &mut [Guest<'_, M>],
        m: usize,
        op: faults::Lifecycle,
        t: u64,
    ) -> Result<(), String> {
        if guests[m].m.time > t {
            return Err("lifecycle boundary is in committed machine past".into());
        }
        match op {
            faults::Lifecycle::Pause => {
                self.paused_ingress[m] = 0;
                guests[m].m.pause()?;
            }
            faults::Lifecycle::Resume => guests[m].m.resume(t)?,
            faults::Lifecycle::Crash => {
                // Management commands queued for the dead incarnation go
                // with it; frames in world transit stay queued for the
                // reboot to sort out.
                let discarded: usize = plan
                    .remove_queued(m, |_, p| !p.strict())
                    .iter()
                    .map(|(_, p)| match p {
                        Msg::Control(bytes) => bytes.len(),
                        _ => 0,
                    })
                    .sum();
                let result = world::crash_machine(
                    guests[m].m,
                    Crash {
                        factory: self.factory,
                        boot: &self.boot[m],
                        run_seed: self.run_seed,
                        branch_identity: self.branch_identity.as_deref(),
                        triggers: &mut self.control.triggers,
                        discarded,
                        incarnations: &mut self.incarnations[m],
                        at: t,
                    },
                );
                if let Err(mut error) = result {
                    world::terminate_workers(guests.iter_mut().map(|g| &mut *g.m), &mut error);
                    self.failure = Some(error.clone());
                    return Err(error);
                }
                if let Some(factor) = self.faults.throttle(m) {
                    guests[m].m.throttle(factor)?;
                }
            }
            faults::Lifecycle::Throttle(factor) => guests[m].m.throttle(factor)?,
            faults::Lifecycle::Reboot => {
                // Frames that reached it before `t` belonged to the dead
                // NIC and are dropped; later ones arrive normally.
                for (key, p) in plan.remove_queued(m, |k, p| p.strict() || k.at >= t) {
                    match p {
                        Msg::Frame { from, .. } => {
                            *self.dropped += 1;
                            self.count(key.at, from, m, RECEIVER_DROPPED);
                        }
                        Msg::Serial(_) | Msg::Control(_) => {}
                        Msg::Partition { from, to, on } => {
                            self.partitioned.insert((from, to), on);
                        }
                    }
                }
                guests[m].m.boot_at(t)?;
            }
        }
        let g = &guests[m];
        self.life[m] = (g.m.paused, g.m.down);
        sync(plan, m, g)
    }

    /// The hook: absorb the last round, then apply every world boundary no
    /// live machine can act before, in the reference order (control, then
    /// triggers, then faults), and register the next ones as exact round
    /// boundaries.
    fn between_rounds(
        &mut self,
        plan: &mut sw::World<Msg>,
        guests: &mut [Guest<'_, M>],
    ) -> Result<sw::Flow, String> {
        let committed = plan.committed();
        let duration = self.run.duration;
        let n = guests.len();
        self.absorb(guests, committed)?;
        if self.fault_failed() {
            self.stopped = Some(RunOutcome::FaultFailed);
            return Ok(sw::Flow::Stop);
        }
        (self.host)()?;
        let until_barrier = self.run.fork.is_some();
        let fork_at = self.run.fork.and_then(Fork::at);
        loop {
            let start = (0..n).map(|m| plan.key(m)).min().unwrap_or(sw::NEVER);
            let control_boundary = self.control.boundary();
            let trigger_boundary = self.control.triggers.boundary();
            let fault_boundary = self.faults.next_boundary();
            // A checkpoint precedes every action at T, including exact-time faults.
            if self.run.checkpoint.is_some_and(|t| {
                start >= t && control_boundary >= t && fault_boundary >= t && trigger_boundary >= t
            }) {
                self.stopped = Some(RunOutcome::Checkpoint);
                return Ok(sw::Flow::Stop);
            }
            if let Some(t) = fork_at {
                park_for_fork(plan, guests, t)?;
            }
            if until_barrier && guests.iter().all(|g| g.m.parked || g.exited_at().is_some()) {
                if let Some(g) = guests.iter().find(|g| !g.m.parked) {
                    return Err(format!("{} exited before the fork point", g.m.name));
                }
                self.stopped = Some(RunOutcome::Barrier);
                return Ok(sw::Flow::Stop);
            }
            self.semantic.advance(
                start
                    .min(control_boundary)
                    .min(trigger_boundary)
                    .min(fault_boundary)
                    .min(duration),
            );
            if control_boundary <= start
                && control_boundary <= trigger_boundary
                && control_boundary < duration
            {
                self.apply_control(plan, guests, control_boundary)?;
                continue;
            }
            if trigger_boundary <= start
                && trigger_boundary < duration
                && trigger_boundary <= fault_boundary
            {
                if guests.iter().any(|g| g.m.time > trigger_boundary) {
                    return Err("trigger boundary is in committed machine past".into());
                }
                self.control
                    .triggers
                    .apply_boundary(self.faults, trigger_boundary)?;
                continue;
            }
            if fault_boundary <= start && fault_boundary < duration {
                self.apply_faults(plan, guests, fault_boundary)?;
                if self.faults.error.is_some() && self.control.pending.is_none() {
                    self.stopped = Some(RunOutcome::FaultFailed);
                    return Ok(sw::Flow::Stop);
                }
                continue;
            }
            // Every boundary ahead ends a round exactly. One at the committed
            // time has passed; nothing is ever due before it.
            for b in [
                control_boundary,
                trigger_boundary,
                fault_boundary,
                self.run.checkpoint.unwrap_or(sw::NEVER),
                fork_at.unwrap_or(sw::NEVER),
            ] {
                if b != sw::NEVER && b >= committed {
                    plan.add_boundary(b)?;
                }
            }
            break;
        }
        for g in guests.iter_mut() {
            g.observe = self.control.triggers.observes(&g.m.name);
            g.fork_at = fork_at;
        }
        Ok(sw::Flow::Continue)
    }

    /// The control barrier at `at`: refuse what the machines can no longer
    /// do, answer an inspection, then admit or reply.
    fn apply_control(
        &mut self,
        plan: &mut sw::World<Msg>,
        guests: &[Guest<'_, M>],
        at: u64,
    ) -> Result<(), String> {
        if guests.iter().any(|g| g.m.time > at) {
            return Err("control boundary is in committed machine past".into());
        }
        let names = |keep: fn(&Guest<'_, M>) -> bool| -> Vec<String> {
            (guests.iter().filter(|g| keep(g)))
                .map(|g| g.m.name.clone())
                .collect()
        };
        self.control
            .reject_unavailable(&names(|g| g.exited_at().is_some()));
        self.control
            .reject_unthrottleable(&names(|g| !g.m.supports_throttle()));
        if self.control.wants_inspect() {
            let machines = (guests.iter().enumerate())
                .map(|(i, g)| world::inspect_machine(&g.m.name, g.m.state, self.faults, i))
                .collect();
            let counters = self.counter_snapshot();
            let services = &self.run.cfg["control"]["placement"];
            (self.control).answer_inspect(services, machines, self.faults, counters)?;
        }
        if let Some(bytes) = self.control.apply_boundary(self.faults, at)? {
            let target = self
                .control
                .controller
                .ok_or("control reply without a controller")?;
            plan.schedule(target, at, Msg::Control(bytes))?;
            plan.add_boundary(at)?;
        }
        Ok(())
    }

    /// The fault boundary at `at`: apply due faults, send their commands,
    /// then carry out lifecycle transitions.
    fn apply_faults(
        &mut self,
        plan: &mut sw::World<Msg>,
        guests: &mut [Guest<'_, M>],
        at: u64,
    ) -> Result<(), String> {
        if guests.iter().any(|g| g.m.time > at) {
            return Err("fault boundary is in committed machine past".into());
        }
        let commands = self.faults.apply_due(at);
        for i in self.faults.take_changed_network() {
            let snapshot = self.counter_snapshot();
            self.faults.set_network_counters(i, snapshot);
        }
        for (target, command) in commands {
            plan.schedule(target, at, Msg::Control(command))?;
            plan.add_boundary(at)?;
        }
        // After the commands, so a crash at this boundary discards
        // those its machine was just sent.
        for (target, op) in self.faults.take_lifecycle() {
            self.lifecycle(plan, guests, target, op, at)?;
        }
        self.control.observe_faults(self.faults, at)
    }
}

/// A machine that cannot act before the fork time `t` parks where it is;
/// one owing I/O settles it first, in its own run.
fn park_for_fork<M: WorldMachine>(
    plan: &mut sw::World<Msg>,
    guests: &mut [Guest<'_, M>],
    t: u64,
) -> Result<(), String> {
    for (m, g) in guests.iter_mut().enumerate() {
        if !g.m.parked
            && !g.m.paused
            && !g.m.down
            && g.exited_at().is_none()
            && plan.key(m) >= t
            && g.m.owed_io().is_none()
        {
            g.m.park()?;
            sync(plan, m, g)?;
        }
    }
    Ok(())
}

impl<M: WorldMachine> sw::Router<Msg> for Coord<'_, M> {
    fn route(&mut self, src: usize, sent_at: u64, msg: Msg, out: &mut Vec<sw::Delivery<Msg>>) {
        let frame = match msg {
            Msg::Frame { bytes, .. } => bytes,
            Msg::Partition { from, to, on } => {
                self.partitioned.insert((from, to), on);
                return;
            }
            Msg::Serial(_) | Msg::Control(_) => return,
        };
        if frame.len() < 14 {
            return;
        }
        let mut dst_mac = [0; 6];
        dst_mac.copy_from_slice(&frame[0..6]);
        for dst in links::candidates(src, dst_mac, self.macs) {
            let last = self.faults.links.last.get(&(src, dst)).copied();
            let cut = *self.partitioned.get(&(src, dst)).unwrap_or(&false);
            let at = match self.faults.route(src, dst, sent_at, &frame, cut) {
                links::Outcome::Cut => {
                    *self.dropped += 1;
                    self.count(sent_at, src, dst, PARTITION_DROPPED);
                    continue;
                }
                links::Outcome::Lost => {
                    *self.dropped += 1;
                    self.count(sent_at, src, dst, LOSS_DROPPED);
                    continue;
                }
                links::Outcome::QueueDropped => {
                    *self.dropped += 1;
                    self.count(sent_at, src, dst, QUEUE_DROPPED);
                    continue;
                }
                links::Outcome::Deliver(at) => at,
            };
            let mut accepted = false;
            for (at, bytes) in self.faults.deliveries(src, dst, at, &frame) {
                // A frame arriving while its receiver is down meets no NIC; one
                // arriving while it is paused waits, up to a fixed backlog.
                let (paused, down) = self.life[dst];
                match events::arrival(
                    at,
                    self.faults.next_lifecycle(dst),
                    down,
                    paused,
                    self.paused_ingress[dst],
                    events::PAUSED_INGRESS_FRAMES,
                ) {
                    events::Arrival::Queue => {}
                    events::Arrival::Held => self.paused_ingress[dst] += 1,
                    events::Arrival::Drop => {
                        *self.dropped += 1;
                        self.count(at, src, dst, RECEIVER_DROPPED);
                        continue;
                    }
                }
                out.push(sw::Delivery {
                    to: dst,
                    at,
                    payload: Msg::Frame { from: src, bytes },
                });
                accepted = true;
            }
            // Restore the base FIFO reservation only when every copy was
            // rejected. A rejected copy cannot undo an accepted original.
            if !accepted {
                match last {
                    Some(last) => {
                        self.faults.links.last.insert((src, dst), last);
                    }
                    None => {
                        self.faults.links.last.remove(&(src, dst));
                    }
                }
            }
        }
    }
}

/// Tell the plan where guest `g`, machine `m`, now stands.
fn sync<M: WorldMachine>(
    plan: &mut sw::World<Msg>,
    m: usize,
    g: &Guest<'_, M>,
) -> Result<(), String> {
    let s = g.state();
    Ok(plan.sync(m, s.time, s.next, s.exited, s.suspended)?)
}

fn hook<M: WorldMachine>(
    plan: &mut sw::World<Msg>,
    guests: &mut [Guest<'_, M>],
    coord: &mut Coord<'_, M>,
) -> Result<sw::Flow, String> {
    coord.between_rounds(plan, guests)
}

/// Run one segment with every machine on the calling thread, using the
/// world's clock. Returns the outcome word and the scheduler's report
/// section; queued events are left in `world.events` and
/// `world.strict_events`.
pub fn simulate<M: WorldMachine>(
    world: &mut World<M>,
    run: &Run,
    skew: Duration,
    boundary: &mut dyn FnMut() -> Result<(), String>,
) -> Result<(RunOutcome, Value), String> {
    windowed(
        world,
        run,
        skew,
        1,
        boundary,
        |plan, guests, clock, coord| sw::run_serial(plan, guests, clock, coord, hook::<M>),
    )
}

/// As [`simulate`], with machines on a work-stealing pool of `workers`
/// threads. Each thread opens its own clock with `make_clock`.
pub fn simulate_parallel<M, F>(
    world: &mut World<M>,
    run: &Run,
    workers: Workers,
    make_clock: F,
    boundary: &mut dyn FnMut() -> Result<(), String>,
) -> Result<(RunOutcome, Value), String>
where
    M: WorldMachine + Send,
    F: Fn(usize) -> Result<M::Clock, String> + Sync,
{
    let count = workers.count;
    windowed(
        world,
        run,
        workers.skew,
        count,
        boundary,
        |plan, guests, _, coord| {
            sw::run_parallel(plan, guests, count, make_clock, coord, hook::<M>)
        },
    )
}

/// Outputs one machine may buffer in one round.
const MAX_OUTPUTS: usize = 1 << 16;

fn windowed<M, X>(
    world: &mut World<M>,
    run: &Run,
    skew: Duration,
    workers: usize,
    boundary: &mut dyn FnMut() -> Result<(), String>,
    execute: X,
) -> Result<(RunOutcome, Value), String>
where
    M: WorldMachine,
    X: FnOnce(
        &mut sw::World<Msg>,
        &mut [Guest<'_, M>],
        &mut M::Clock,
        &mut Coord<'_, M>,
    ) -> Result<(sw::Outcome, sw::Stats), sw::Error>,
{
    world.validate()?;
    if run.checkpoint.is_some() {
        M::checkpoint_boundary(&world.clock)?;
    }
    let n = world.machines.len();
    let end_on = match run.end_on {
        Some(name) => Some(
            world
                .machines
                .iter()
                .position(|m| m.name == name)
                .ok_or(format!("end_on_exit: unknown machine {name}"))?,
        ),
        None => None,
    };
    let params = sw::Params {
        latency: world.lookahead,
        early: world.early,
        duration: run.duration,
        end_on,
        max_outputs: MAX_OUTPUTS,
    };
    let World {
        failure,
        machines,
        factory,
        boot,
        run_seed,
        branch_identity,
        incarnations,
        paused_ingress,
        clock,
        events,
        seq,
        latency,
        partitioned,
        faults,
        control,
        strict_events,
        macs,
        dropped,
        delivered,
        link_counts,
        semantic_counts,
        slices,
        ..
    } = world;
    let names: Vec<String> = machines.iter().map(|m| m.name.clone()).collect();
    let mut guests: Vec<Guest<M>> = machines
        .iter_mut()
        .enumerate()
        .map(|(i, m)| Guest::new(i, m, skew * (n - i) as u32))
        .collect();
    // Effects committed before this segment are absorbed before anything runs.
    for g in &mut guests {
        g.drain();
    }
    let states = guests.iter().map(Guest::state).collect();
    let mut plan = sw::World::<Msg>::with_machines(states, params)?;
    // Events queued by an earlier segment or the plan keep their order:
    // management first at equal times, as strict delivery has it.
    for (m, q) in strict_events.iter_mut().enumerate() {
        for ((at, _), bytes) in std::mem::take(q) {
            plan.schedule(m, at, Msg::Control(bytes))?;
            plan.add_boundary(at)?;
        }
    }
    for (m, q) in events.iter_mut().enumerate() {
        for ((at, _), p) in std::mem::take(q) {
            let msg = match p {
                Payload::Frame { from, bytes } => Msg::Frame { from, bytes },
                Payload::Serial(bytes) => Msg::Serial(bytes),
                Payload::Partition { from, to, on } => Msg::Partition { from, to, on },
            };
            plan.schedule(m, at, msg)?;
        }
    }
    let life = guests.iter().map(|g| (g.m.paused, g.m.down)).collect();
    let mut coord = Coord::<M> {
        run,
        names,
        faults,
        control,
        factory,
        boot,
        run_seed: *run_seed,
        branch_identity: branch_identity.clone(),
        incarnations,
        paused_ingress,
        partitioned,
        macs,
        dropped,
        delivered,
        link_counts,
        semantic: semantic_counts,
        latency: *latency,
        life,
        stopped: None,
        failure: None,
        host: boundary,
    };
    let started = Instant::now();
    let result = execute(&mut plan, &mut guests, clock, &mut coord);
    let host_seconds = started.elapsed().as_secs_f64();
    let stopped = coord.stopped;
    if let Some(reason) = coord.failure.take() {
        *failure = Some(reason);
    }
    drop(coord);
    drop(guests);
    // Leave queued events where the report and the next segment look for them.
    for (to, key, msg) in plan.pending() {
        *seq += 1;
        match msg {
            Msg::Control(bytes) => {
                strict_events[to].insert((key.at, *seq), bytes.clone());
            }
            Msg::Frame { from, bytes } => {
                events[to].insert(
                    (key.at, *seq),
                    Payload::Frame {
                        from: *from,
                        bytes: bytes.clone(),
                    },
                );
            }
            Msg::Serial(bytes) => {
                events[to].insert((key.at, *seq), Payload::Serial(bytes.clone()));
            }
            Msg::Partition { from, to: b, on } => {
                events[to].insert(
                    (key.at, *seq),
                    Payload::Partition {
                        from: *from,
                        to: *b,
                        on: *on,
                    },
                );
            }
        }
    }
    let (outcome, stats) = result.map_err(|e| match e {
        // The host's own error, as it gave it.
        sw::Error::Boundary(message) => message,
        other => other.to_string(),
    })?;
    *slices += stats.rounds;
    let committed = plan.committed();
    let (word, exit) = match outcome {
        sw::Outcome::Duration { .. } => (RunOutcome::Duration, None),
        // Paused or down machines with nothing left to resume or reboot
        // them sit out the rest of the run.
        sw::Outcome::Quiescent { .. } => (
            if machines.iter().any(|m| m.paused || m.down) {
                RunOutcome::Duration
            } else {
                RunOutcome::Quiescent
            },
            None,
        ),
        sw::Outcome::Exited {
            machine, exit_at, ..
        } => (
            RunOutcome::EndMachineExited,
            Some(json!({"machine": machines[machine].name, "exit_ns": exit_at})),
        ),
        sw::Outcome::Stopped { .. } => (stopped.ok_or("stopped without a reason")?, None),
    };
    if run.require_barrier && run.fork.is_some() && word != RunOutcome::Barrier {
        return Err(format!("launch barrier not reached: {word}"));
    }
    let rounds = plan.rounds();
    let evidence = json!({
        "name": "windowed",
        "machine_workers": workers.max(1),
        "committed_ns": committed,
        "end_machine_exit": exit,
        "rounds": stats.rounds,
        "exact_rounds": rounds.iter().filter(|r| r.exact).count(),
        "machine_runs": stats.machine_runs,
        "migrations": stats.migrations,
        "effects_digest": format!("{:016x}", plan.effects_digest()),
        "window_ns": {
            "min": stats.window_ns_min,
            "max": stats.window_ns_max,
            "mean": if stats.rounds == 0 { 0 } else { (stats.window_ns_sum / stats.rounds as u128) as u64 },
        },
        "host": {
            "seconds": host_seconds,
            "coordinator_s": stats.coordinator.as_secs_f64(),
            "machine_s": stats.machine.as_secs_f64(),
            "round_wall_s": stats.round_wall.as_secs_f64(),
        },
    });
    Ok((word, evidence))
}

#[cfg(test)]
#[path = "world_windowed_tests.rs"]
mod tests;
