//! The windowed scheduler over the shared world: rounds planned by
//! [`crate::window`], machines executed serially or on a work-stealing pool,
//! and the world's policies (faults, runtime control, triggers, scenario
//! input, lifecycle, forks and checkpoints) carried out by the coordinator
//! between rounds. See docs/internals.md#parallel-scheduling.
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
pub use seedbox_engine_protocol::host_control::{Boundary, BoundaryPosition};
use seedbox_engine_protocol::observation::{Frame, FrameFate, Kind as ObservationKind};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::time::{Duration, Instant};

/// A host callback runs while every machine is stopped. Admissions made here
/// are scheduled before the next guest round.
pub trait BoundaryCallbacks {
    fn boundary(
        &mut self,
        boundary: Boundary,
        admission: &mut crate::intervention::Admission<'_>,
    ) -> Result<(), String>;
    /// Playback-only stop before a due effect. No admission scope is supplied.
    fn before_fault(&mut self, _boundary: Boundary) -> Result<(), String> {
        Ok(())
    }
}

impl<F> BoundaryCallbacks for F
where
    F: FnMut(Boundary, &mut crate::intervention::Admission<'_>) -> Result<(), String>,
{
    fn boundary(
        &mut self,
        boundary: Boundary,
        admission: &mut crate::intervention::Admission<'_>,
    ) -> Result<(), String> {
        self(boundary, admission)
    }
}

pub type BoundaryHandler<'a> =
    dyn FnMut(Boundary, &mut crate::intervention::Admission<'_>) -> Result<(), String> + 'a;
pub type CallbackHandler<'a> = dyn BoundaryCallbacks + 'a;

struct CallbackAdapter<'a, 'b>(&'a mut BoundaryHandler<'b>);
impl BoundaryCallbacks for CallbackAdapter<'_, '_> {
    fn boundary(
        &mut self,
        boundary: Boundary,
        admission: &mut crate::intervention::Admission<'_>,
    ) -> Result<(), String> {
        (self.0)(boundary, admission)
    }
}

/// The coordinator's errors are plain messages, like the machines' own.
impl From<sw::Error> for String {
    fn from(e: sw::Error) -> Self {
        e.to_string()
    }
}

/// What travels between the coordinator and a machine.
#[derive(Clone, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
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

pub use seedbox_engine_protocol::report::RunOutcome;

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
    observations: crate::observation::Buffer,
    /// Exited in the last round.
    exited_now: bool,
}

/// Machine capability and exited-target policy shared by all host-origin inputs.
fn validate_host_action<M: WorldMachine>(
    guests: &[Guest<'_, M>],
    action: &seedbox_control::Action,
) -> Result<(), String> {
    if let Some(machine) = action.machine() {
        if guests
            .iter()
            .any(|guest| guest.m.name == machine && guest.exited_at().is_some())
        {
            if let Some(message) = crate::control::exited_target_error(action) {
                return Err(format!("{machine}: {message}"));
            }
        }
    }
    if let seedbox_control::Action::Throttle { machine, .. } = action {
        if guests
            .iter()
            .any(|guest| guest.m.name == *machine && !guest.m.supports_throttle())
        {
            return Err(format!(
                "{machine}: throttle is unsupported by this backend"
            ));
        }
    }
    Ok(())
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
            observations: crate::observation::Buffer::default(),
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
        for entry in self.m.observations.drain() {
            self.observations.push(entry);
        }
        for (id, at, success) in std::mem::take(&mut self.m.fault_effects) {
            self.effects.push((at, id, success));
        }
        for (at, line) in std::mem::take(&mut self.m.control_requests) {
            self.requests.push((at, line));
        }
        if let Some((at, byte)) = self.m.committed_console.take() {
            self.console.push((at, byte));
        }
        for (at, bytes) in std::mem::take(&mut self.m.committed_output) {
            self.console
                .extend(bytes.into_iter().map(|byte| (at, byte)));
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
        // Finite scheduling opportunities perturb host completion order only.
        for _ in 0..self.skew.as_micros() {
            std::thread::yield_now();
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
    observations: Option<&'a crate::observation::Publisher>,
    run: &'a Run<'a>,
    names: Vec<String>,
    faults: &'a mut faults::Faults,
    input_program: &'a mut crate::input_program::Program,
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
    interactive_checkpoint: bool,
    resume_boundary: bool,
    /// A failed crash transaction: the world is terminal.
    failure: Option<String>,
    host: &'a mut CallbackHandler<'a>,
    published_boundary: Boundary,
    prior_rounds: u64,
    boundaries: &'a mut u64,
    application_ready: bool,
    fault_statuses: Vec<faults::RowStatus>,
    published_rejections: usize,
    /// Routed frames not yet published; empty without an observation socket.
    frames: Vec<Frame>,
}

impl<M: WorldMachine> Coord<'_, M> {
    /// Called only after policy mutations, never on idle execution rounds.
    /// This cache is runtime-only and starts at the segment's existing state.
    fn publish_faults(&mut self, at: u64) {
        let Some(publisher) = self.observations else {
            return;
        };
        for rejected in &self.input_program.rejections()[self.published_rejections..] {
            publisher.publish(
                "",
                0,
                at,
                ObservationKind::OverrideRejected {
                    input_key: rejected.key.clone(),
                    at_ns: rejected.at_ns,
                    reason: rejected.reason.clone(),
                },
            );
        }
        self.published_rejections = self.input_program.rejections().len();
        for (index, row) in self.faults.log.iter().enumerate() {
            if self.fault_statuses.get(index) == Some(&row.status) {
                continue;
            }
            publisher.publish(
                "",
                0,
                at,
                seedbox_engine_protocol::observation::Kind::FaultChanged {
                    id: row.id,
                    input_key: self.input_program.key_for(row.id).map(str::to_owned),
                    action: row.action.to_string(),
                    machine: row.machine.clone(),
                    status: row.status,
                    applied_time_ns: row.applied_at_ns,
                    plan: self
                        .faults
                        .plan(row.id)
                        .map(|plan| Box::new(plan.action.clone())),
                },
            );
            if let Some(status) = self.fault_statuses.get_mut(index) {
                *status = row.status;
            } else {
                self.fault_statuses.push(row.status);
            }
        }
    }

    /// Seed observers from retained fault state when entering a segment. Applied
    /// rows are replayed in application order so a later heal stays effective.
    /// This reads retained history without changing it or running any action.
    fn publish_initial_faults(&self, at: u64) {
        let Some(publisher) = self.observations else {
            return;
        };
        for rejected in self.input_program.rejections() {
            publisher.publish(
                "",
                0,
                at,
                ObservationKind::OverrideRejected {
                    input_key: rejected.key.clone(),
                    at_ns: rejected.at_ns,
                    reason: rejected.reason.clone(),
                },
            );
        }
        let mut rows: Vec<_> = self
            .faults
            .log
            .iter()
            .filter(|row| row.status != faults::RowStatus::Unrequested)
            .collect();
        rows.sort_by_key(|row| (row.applied_at_ns.unwrap_or(u64::MAX), row.id));
        for row in rows {
            publisher.publish(
                "",
                0,
                at,
                ObservationKind::FaultChanged {
                    id: row.id,
                    input_key: self.input_program.key_for(row.id).map(str::to_owned),
                    action: row.action.to_string(),
                    machine: row.machine.clone(),
                    status: row.status,
                    applied_time_ns: row.applied_at_ns,
                    plan: self
                        .faults
                        .plan(row.id)
                        .map(|plan| Box::new(plan.action.clone())),
                },
            );
        }
    }

    /// Record a routed frame copy for observers. Without an observation
    /// socket this does nothing; it never touches policy state.
    fn observe_frame(
        &mut self,
        from: usize,
        to: usize,
        sent_ns: u64,
        due_ns: Option<u64>,
        bytes: usize,
        fate: FrameFate,
    ) {
        if self.observations.is_none() {
            return;
        }
        self.frames.push(Frame {
            from: self.names[from].clone(),
            to: self.names[to].clone(),
            sent_ns,
            due_ns,
            bytes: u32::try_from(bytes).unwrap_or(u32::MAX),
            fate,
        });
        if self.frames.len() >= FRAMES_PER_OBSERVATION {
            self.publish_frames(sent_ns);
        }
    }

    fn publish_frames(&mut self, at: u64) {
        let Some(publisher) = self.observations else {
            return;
        };
        if self.frames.is_empty() {
            return;
        }
        let frames = std::mem::take(&mut self.frames);
        publisher.publish("", 0, at, ObservationKind::Frames { frames });
    }

    fn count(&mut self, at: u64, from: usize, to: usize, outcome: usize) {
        self.link_counts.entry((from, to)).or_default()[outcome] += 1;
        self.semantic.record(at, from, to, outcome);
    }

    fn counter_snapshot(&self) -> Value {
        network_counters::snapshot_value(|m| &self.names[m], self.semantic.counts())
    }

    fn fault_failed(&self) -> bool {
        (self.control.error.is_some()
            || (self.faults.error.is_some() && self.control.pending.is_none()))
            && !self.control.has_undelivered_reply()
    }

    /// Absorb the last round's effects, in a fixed order: receiver outcomes,
    /// fault effects, console bytes, exits, delivered replies, then requests.
    fn absorb(&mut self, guests: &mut [Guest<'_, M>], committed: u64) -> Result<(), String> {
        let mut machine_events = Vec::new();
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
        let faults_changed = effects.iter().any(|(_, _, _, id, _)| *id != 0)
            || guests.iter().any(|guest| guest.exited_now);
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
                match self.faults.management_ready(m, at, self.latency) {
                    Err(error) => self.faults.error = Some(error),
                    Ok(()) => machine_events.push((
                        at,
                        m,
                        0,
                        seedbox_engine_protocol::observation::Kind::MachineReady,
                    )),
                }
                if !self.application_ready {
                    self.application_ready = self.faults.initial_applications_ready();
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
            if let State::Exited(code) = g.m.state {
                machine_events.push((
                    g.m.time,
                    m,
                    1,
                    seedbox_engine_protocol::observation::Kind::MachineExited { code },
                ));
            }
            self.faults.machine_exited(m, g.m.time);
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
        let mut observations = Vec::new();
        for (m, guest) in guests.iter_mut().enumerate() {
            observations.extend(
                guest
                    .observations
                    .drain()
                    .enumerate()
                    .map(|(j, entry)| (entry.time_ns, m, j, entry)),
            );
        }
        if let Some(publisher) = self.observations {
            // Preserve log channel order, then lifecycle order at equal times.
            // Machine events bypass log buffers, so log overflow cannot hide them.
            for (at, m, order, kind) in machine_events {
                observations.push((
                    at,
                    m,
                    usize::MAX - 1 + order,
                    crate::observation::Entry {
                        time_ns: at,
                        incarnation: guests[m].m.incarnation(),
                        kind,
                    },
                ));
            }
            observations.sort_by_key(|(at, m, j, _)| (*at, *m, *j));
            for (at, m, _, entry) in observations {
                publisher.publish(&self.names[m], entry.incarnation, at, entry.kind);
            }
        }
        self.publish_frames(committed);
        if faults_changed {
            self.publish_faults(committed);
        }
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
        let previous_incarnation = guests[m].m.incarnation();
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
                )
                .and_then(|()| {
                    // The successor is not usable until its active throttle is
                    // restored. A failed RPC leaves the whole crash terminal.
                    if let Some(factor) = self.faults.throttle(m) {
                        guests[m].m.throttle(factor)?;
                    }
                    Ok(())
                });
                if let Err(mut error) = result {
                    world::terminate_workers(guests.iter_mut().map(|g| &mut *g.m), &mut error);
                    self.failure = Some(error.clone());
                    return Err(error);
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
        sync(plan, m, g)?;
        if let Some(publisher) = self.observations {
            use seedbox_engine_protocol::observation::{Kind, MachineTransition};
            let transition = match op {
                faults::Lifecycle::Pause => MachineTransition::Paused,
                faults::Lifecycle::Resume => MachineTransition::Resumed,
                faults::Lifecycle::Crash => MachineTransition::Crashed,
                faults::Lifecycle::Reboot => MachineTransition::Rebooted,
                faults::Lifecycle::Throttle(factor_permille) => {
                    MachineTransition::Throttled { factor_permille }
                }
            };
            let incarnation = if op == faults::Lifecycle::Crash {
                previous_incarnation
            } else {
                g.m.incarnation()
            };
            publisher.publish(
                &self.names[m],
                incarnation,
                t,
                Kind::MachineLifecycle { transition },
            );
        }
        Ok(())
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
        // Capture returns ownership without advancing simulation. Reenter its
        // existing boundary instead of applying it twice or creating a phantom
        // replay position for later interactive faults.
        let resuming = std::mem::take(&mut self.resume_boundary);
        let mut flow = if resuming {
            sw::Flow::Continue
        } else {
            self.advance_boundaries(plan, guests)?
        };
        if !resuming {
            *self.boundaries = self
                .boundaries
                .checked_add(1)
                .ok_or("host boundary index overflow")?;
        }
        let terminal = match flow {
            sw::Flow::Continue => plan.outcome().is_some(),
            sw::Flow::Stop => self.stopped == Some(RunOutcome::FaultFailed),
        };
        let boundary = Boundary {
            position: BoundaryPosition {
                boundary_index: *self.boundaries,
                completed_rounds: self
                    .prior_rounds
                    .checked_add(plan.rounds().len() as u64)
                    .ok_or("host boundary round count overflow")?,
                simulated_time_ns: plan.committed().max(self.semantic.frontier()),
            },
            terminal,
            application_ready: self.application_ready,
        };
        let validate_action =
            |action: &seedbox_control::Action| validate_host_action(guests, action);
        let mut admission =
            crate::intervention::Admission::new(self.faults, boundary, &validate_action)
                .with_program(self.input_program)
                .with_checkpoint_support(matches!(flow, sw::Flow::Continue) && !terminal);
        self.published_boundary = boundary;
        self.host.boundary(boundary, &mut admission)?;
        if admission.checkpoint_requested() {
            self.stopped = Some(RunOutcome::Checkpoint);
            self.interactive_checkpoint = true;
            return Ok(sw::Flow::Stop);
        }
        if (resuming || admission.changed()) && matches!(flow, sw::Flow::Continue) {
            // A resumed definition may replace pending inputs or add one at
            // this checkpoint's time. Register and drain them before any guest
            // runs, while preserving the existing host boundary identity.
            // Newly admitted live effects need the same ordering.
            flow = self.advance_boundaries(plan, guests)?;
        }
        Ok(flow)
    }

    fn advance_boundaries(
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
        // An oracle exit ends the experiment before future actions, including
        // during idle intervals where the next peer wake is much later.
        if matches!(plan.outcome(), Some(sw::Outcome::Exited { .. })) {
            return Ok(sw::Flow::Continue);
        }
        let until_barrier = self.run.fork.is_some();
        let fork_at = self.run.fork.and_then(Fork::at);
        loop {
            let start = (0..n).map(|m| plan.key(m)).min().unwrap_or(sw::NEVER);
            let control_boundary = self.control.boundary();
            let trigger_boundary = self.control.triggers.boundary();
            let fault_boundary = self.faults.next_boundary();
            let input_boundary = self.input_program.next_time();
            // A checkpoint precedes every action at T, including exact-time faults.
            if self.run.checkpoint.is_some_and(|t| {
                start >= t
                    && control_boundary >= t
                    && fault_boundary >= t
                    && trigger_boundary >= t
                    && input_boundary >= t
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
                    .min(input_boundary)
                    .min(duration),
            );
            if control_boundary <= start
                && control_boundary <= trigger_boundary
                && control_boundary <= input_boundary
                && control_boundary < duration
            {
                self.apply_control(plan, guests, control_boundary)?;
                continue;
            }
            if trigger_boundary <= start
                && trigger_boundary < duration
                && trigger_boundary <= fault_boundary
                && trigger_boundary <= input_boundary
            {
                if guests.iter().any(|g| g.m.time > trigger_boundary) {
                    return Err("trigger boundary is in committed machine past".into());
                }
                self.control
                    .triggers
                    .apply_boundary(self.faults, trigger_boundary)?;
                self.publish_faults(trigger_boundary);
                continue;
            }
            // At equal times, guest controls and triggers retain priority.
            // Admit one document input, then drain all its due effects before
            // admitting the next same-time input; no guest runs between them.
            if input_boundary <= start
                && input_boundary <= fault_boundary
                && input_boundary < duration
            {
                if guests.iter().any(|g| g.m.time > input_boundary) {
                    return Err("input boundary is in committed machine past".into());
                }
                self.input_program
                    .admit_due(self.faults, input_boundary, &|action| {
                        validate_host_action(guests, action)
                    })?;
                self.publish_faults(input_boundary);
                self.apply_faults(plan, guests, input_boundary)?;
                if self.faults.error.is_some() && self.control.pending.is_none() {
                    self.stopped = Some(RunOutcome::FaultFailed);
                    return Ok(sw::Flow::Stop);
                }
                continue;
            }
            if fault_boundary <= start
                && fault_boundary < duration
                && fault_boundary < input_boundary
            {
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
                input_boundary,
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
            (self.control).answer_inspect(machines, self.faults, counters)?;
        }
        if let Some(bytes) = self.control.apply_boundary(self.faults, at)? {
            let target = self
                .control
                .controller
                .ok_or("control reply without a controller")?;
            plan.schedule(target, at, Msg::Control(bytes))?;
            plan.add_boundary(at)?;
        }
        self.publish_faults(at);
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
        if self.faults.has_effect_at(at) {
            self.host.before_fault(self.published_boundary)?;
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
        self.control.observe_faults(self.faults, at)?;
        self.publish_faults(at);
        Ok(())
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
                    self.observe_frame(
                        src,
                        dst,
                        sent_at,
                        None,
                        frame.len(),
                        FrameFate::PartitionDropped,
                    );
                    continue;
                }
                links::Outcome::Lost => {
                    *self.dropped += 1;
                    self.count(sent_at, src, dst, LOSS_DROPPED);
                    self.observe_frame(
                        src,
                        dst,
                        sent_at,
                        None,
                        frame.len(),
                        FrameFate::LossDropped,
                    );
                    continue;
                }
                links::Outcome::QueueDropped => {
                    *self.dropped += 1;
                    self.count(sent_at, src, dst, QUEUE_DROPPED);
                    self.observe_frame(
                        src,
                        dst,
                        sent_at,
                        None,
                        frame.len(),
                        FrameFate::QueueDropped,
                    );
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
                        self.observe_frame(
                            src,
                            dst,
                            sent_at,
                            Some(at),
                            bytes.len(),
                            FrameFate::ReceiverDropped,
                        );
                        continue;
                    }
                }
                self.observe_frame(
                    src,
                    dst,
                    sent_at,
                    Some(at),
                    bytes.len(),
                    FrameFate::Scheduled,
                );
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
    boundary: &mut BoundaryHandler<'_>,
) -> Result<(RunOutcome, Value), String> {
    simulate_with_callbacks(world, run, skew, &mut CallbackAdapter(boundary))
}

/// As [`simulate`], with a separate playback-only pre-effect hook.
pub fn simulate_with_callbacks<M: WorldMachine>(
    world: &mut World<M>,
    run: &Run,
    skew: Duration,
    boundary: &mut CallbackHandler<'_>,
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
    boundary: &mut BoundaryHandler<'_>,
) -> Result<(RunOutcome, Value), String>
where
    M: WorldMachine + Send,
    F: Fn(usize) -> Result<M::Clock, String> + Sync,
{
    simulate_parallel_with_callbacks(
        world,
        run,
        workers,
        make_clock,
        &mut CallbackAdapter(boundary),
    )
}

/// As [`simulate_parallel`], with a separate playback-only pre-effect hook.
pub fn simulate_parallel_with_callbacks<M, F>(
    world: &mut World<M>,
    run: &Run,
    workers: Workers,
    make_clock: F,
    boundary: &mut CallbackHandler<'_>,
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
/// Frames per observation datagram; well under its size limit.
const FRAMES_PER_OBSERVATION: usize = 96;

fn windowed<M, X>(
    world: &mut World<M>,
    run: &Run,
    skew: Duration,
    workers: usize,
    boundary: &mut CallbackHandler<'_>,
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
        observations,
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
        input_program,
        control,
        strict_events,
        macs,
        dropped,
        delivered,
        link_counts,
        semantic_counts,
        slices,
        boundaries,
        scheduler_continuation,
        ..
    } = world;
    if let Some(plan) = scheduler_continuation.as_ref() {
        if plan.params() != &params {
            return Err("checkpoint continuation scheduler parameters changed".into());
        }
        if events.iter().any(|q| !q.is_empty()) || strict_events.iter().any(|q| !q.is_empty()) {
            return Err("checkpoint continuation has duplicate queued events".into());
        }
    }
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
    let resume_boundary = scheduler_continuation.is_some();
    let mut plan = if let Some(mut plan) = scheduler_continuation.take() {
        for (index, guest) in guests.iter().enumerate() {
            sync(&mut plan, index, guest)?;
        }
        plan
    } else {
        let states = guests.iter().map(Guest::state).collect();
        sw::World::<Msg>::with_machines(states, params)?
    };
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
    let application_ready = faults.initial_applications_ready();
    let fault_statuses = if observations.is_some() {
        faults.log.iter().map(|row| row.status).collect()
    } else {
        Vec::new()
    };
    let published_boundary = Boundary {
        position: BoundaryPosition {
            boundary_index: *boundaries,
            completed_rounds: *slices,
            simulated_time_ns: plan.committed(),
        },
        application_ready,
        terminal: false,
    };
    let published_rejections = input_program.rejections().len();
    let mut coord = Coord::<M> {
        published_rejections,
        published_boundary,
        application_ready,
        fault_statuses,
        frames: Vec::new(),
        observations: observations.as_deref(),
        run,
        names,
        faults,
        input_program,
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
        interactive_checkpoint: false,
        resume_boundary,
        failure: None,
        host: boundary,
        prior_rounds: slices
            .checked_sub(plan.rounds().len() as u64)
            .ok_or("checkpoint continuation round count mismatch")?,
        boundaries,
    };
    coord.publish_initial_faults(plan.committed());
    let started = Instant::now();
    let result = execute(&mut plan, &mut guests, clock, &mut coord);
    let host_seconds = started.elapsed().as_secs_f64();
    let stopped = coord.stopped;
    let interactive_checkpoint = coord.interactive_checkpoint;
    if let Some(reason) = coord.failure.take() {
        *failure = Some(reason);
    }
    drop(coord);
    drop(guests);
    if result.is_err() {
        for machine in machines.iter_mut() {
            machine.observations.clear();
        }
    }
    // Leave queued events where the report and the next segment look for them.
    for (to, key, msg) in plan.pending() {
        if interactive_checkpoint {
            break;
        }
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
    if interactive_checkpoint {
        *scheduler_continuation = Some(plan);
    }
    Ok((word, evidence))
}

#[cfg(test)]
#[path = "world_windowed_tests.rs"]
mod tests;
