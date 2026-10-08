//! World state → round plan → machine reports → committed world state.
//! Pure coordinator logic: no threads, no host time.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::hash::{Hash, Hasher};

/// Simulated nanoseconds.
pub type Time = u64;
/// A machine that cannot act on its own: only an event can wake it.
pub const NEVER: Time = u64::MAX;

/// Breaks ties between events at the same instant. World events (scenario
/// input) come before frames.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Class {
    /// Scheduled by the coordinator.
    World,
    /// Produced by a machine and routed by the coordinator.
    Frame,
}

/// Who produced an event. The world sorts before every machine.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Source {
    World,
    Machine(usize),
}

/// Total order of a machine's queued events. `seq` counts the source's own
/// outputs, so the key does not depend on which machine was processed first.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EventKey {
    pub at: Time,
    pub class: Class,
    pub source: Source,
    pub seq: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Event<P> {
    pub key: EventKey,
    pub payload: P,
}

/// Semantic parameters: changing any of them changes runs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Params {
    /// No frame arrives sooner than this after it is sent.
    pub latency: Time,
    /// Events may be delivered, and horizon stops land, up to this long
    /// before their time. Below `latency`.
    pub early: Time,
    /// The run's exact end.
    pub duration: Time,
    /// The machine whose exit ends the world.
    pub end_on: Option<usize>,
    /// Outputs one machine may buffer in one round. Exceeding it fails the
    /// attempt rather than dropping or reordering effects.
    pub max_outputs: usize,
}

impl Params {
    fn check(&self) -> Result<(), Error> {
        if self.latency == 0 {
            return Err(Error::Params("latency must be positive".into()));
        }
        if self.early >= self.latency {
            return Err(Error::Params(format!(
                "early delivery {} must be below latency {}",
                self.early, self.latency
            )));
        }
        if self.max_outputs == 0 {
            return Err(Error::Params("max_outputs must be positive".into()));
        }
        Ok(())
    }

    /// The longest round: `latency - early`.
    pub fn window(&self) -> Time {
        self.latency - self.early
    }
}

/// One machine's part in one round. Immutable once planned.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Assignment<P> {
    pub machine: usize,
    /// The machine's first possible action. An idle machine wakes here
    /// before delivering anything; it neither acts nor receives earlier.
    pub wake: Time,
    /// Run up to this time and no further.
    pub stop: Time,
    /// The stop may land this far before `stop`: 0 at an exact boundary,
    /// and at most half the round, so every round makes progress.
    pub stop_early: Time,
    /// Deliver an event once the machine's time is at least `at - early`.
    pub delivery_early: Time,
    /// Events that may be delivered in this round, in key order.
    pub incoming: Vec<Event<P>>,
    pub max_outputs: usize,
}

impl<P> Assignment<P> {
    /// The machine has finished the round once its time reaches this.
    pub fn slice_end(&self) -> Time {
        self.stop - self.stop_early
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RoundPlan<P> {
    pub round: u64,
    /// `T`: no machine can act earlier.
    pub start: Time,
    /// `B`: the round's end, exclusive.
    pub end: Time,
    /// `end` is a global boundary or the run's end, not a horizon.
    pub exact: bool,
    /// Active machines in ascending order.
    pub assignments: Vec<Assignment<P>>,
}

/// An effect a machine produced, at its actual send time.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Output<P> {
    pub sent_at: Time,
    pub payload: P,
}

/// What a machine did with its assignment.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Report<P> {
    /// Where it stopped (or exited). Before the slice end only when idle:
    /// `next` and the first undelivered event are both at or after it.
    pub time: Time,
    /// The earliest it can next act by itself, ignoring queued events;
    /// `NEVER` when only an event can wake it.
    pub next: Time,
    /// How many of the assigned events it delivered: a prefix.
    pub consumed: usize,
    /// In send order.
    pub outputs: Vec<Output<P>>,
    pub exited: Option<Time>,
    /// The machine parked or was suspended where it stopped: it can neither
    /// act nor receive until the coordinator resumes it, so it may stop
    /// short of its slice end.
    pub suspended: bool,
}

/// A routed copy of an output.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Delivery<P> {
    pub to: usize,
    pub at: Time,
    pub payload: P,
}

/// Network policy and its randomness. Owned by the coordinator and called
/// in a fixed order, so its draws do not depend on worker count.
pub trait Router<P> {
    fn route(&mut self, from: usize, sent_at: Time, payload: P, out: &mut Vec<Delivery<P>>);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MachineState {
    pub time: Time,
    pub next: Time,
    pub exited: Option<Time>,
    /// Outputs produced so far: the next output's source sequence.
    pub sent: u64,
    /// Parked, paused or down: it neither acts nor receives until the
    /// coordinator resumes it with [`World::sync`].
    pub suspended: bool,
}

impl MachineState {
    /// Runnable at `time` with nothing queued.
    pub fn at(time: Time) -> Self {
        Self {
            time,
            next: time,
            exited: None,
            sent: 0,
            suspended: false,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// The run reached its duration.
    Duration { at: Time },
    /// The designated machine exited at `exit_at`; the world committed
    /// through `committed`.
    Exited {
        machine: usize,
        exit_at: Time,
        committed: Time,
    },
    /// No machine can ever act again.
    Quiescent { committed: Time },
    /// The coordinator's between-rounds hook stopped the run.
    Stopped { committed: Time },
}

/// One committed round, for witnesses and window statistics.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RoundRecord {
    pub round: u64,
    pub start: Time,
    pub end: Time,
    pub exact: bool,
    pub active: usize,
    pub outputs: usize,
    pub deliveries: usize,
}

pub enum Step<P> {
    Round(RoundPlan<P>),
    Done(Outcome),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    Params(String),
    /// A routed event would be due inside the round that produced it.
    Causality {
        from: usize,
        to: usize,
        sent_at: Time,
        at: Time,
        round_end: Time,
    },
    OutputLimit {
        machine: usize,
        outputs: usize,
        limit: usize,
    },
    /// A report that breaks its assignment.
    Report {
        machine: usize,
        reason: String,
    },
    /// A machine failed while executing; the round was not committed.
    Machine {
        machine: usize,
        round: u64,
        message: String,
    },
    /// A worker failed outside any machine; the round was not committed.
    Worker {
        worker: usize,
        message: String,
    },
    /// The caller's between-rounds hook failed after a commit.
    Boundary(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Params(m) => write!(f, "windowed scheduler: {m}"),
            Error::Causality { from, to, sent_at, at, round_end } => write!(
                f,
                "causality violation: machine {from} sent at {sent_at} ns to machine {to} due at {at} ns, inside the round ending at {round_end} ns"
            ),
            Error::OutputLimit { machine, outputs, limit } => {
                write!(f, "machine {machine} buffered {outputs} outputs in one round; the limit is {limit}")
            }
            Error::Report { machine, reason } => write!(f, "machine {machine}: invalid round report: {reason}"),
            Error::Machine { machine, round, message } => write!(f, "machine {machine} failed in round {round}: {message}"),
            Error::Worker { worker, message } => write!(f, "worker {worker} failed: {message}"),
            Error::Boundary(m) => write!(f, "between rounds: {m}"),
        }
    }
}

impl std::error::Error for Error {}

/// The committed world: everything the coordinator decides from.
pub struct World<P> {
    params: Params,
    machines: Vec<MachineState>,
    queues: Vec<BTreeMap<EventKey, P>>,
    boundaries: BTreeSet<Time>,
    world_seq: u64,
    committed: Time,
    round: u64,
    outcome: Option<Outcome>,
    log: Vec<RoundRecord>,
    effects: Fnv,
}

impl<P: Clone + Hash> World<P> {
    /// Every machine starts runnable at time 0.
    pub fn new(machines: usize, params: Params) -> Result<Self, Error> {
        Self::with_machines(vec![MachineState::at(0); machines], params)
    }

    /// Start from committed machine states: a world resumed after a fork
    /// release or a checkpoint. The committed time is the latest machine
    /// time; nothing has been routed yet, so every `sent` starts at 0.
    pub fn with_machines(machines: Vec<MachineState>, params: Params) -> Result<Self, Error> {
        params.check()?;
        if params.end_on.is_some_and(|m| m >= machines.len()) {
            return Err(Error::Params("end_on names no machine".into()));
        }
        for (m, s) in machines.iter().enumerate() {
            if s.exited.is_none() && s.next < s.time {
                return Err(Error::Params(format!(
                    "machine {m}: next action {} is before its time {}",
                    s.next, s.time
                )));
            }
        }
        let committed = machines.iter().map(|s| s.time).max().unwrap_or(0);
        let n = machines.len();
        Ok(Self {
            params,
            machines,
            queues: (0..n).map(|_| BTreeMap::new()).collect(),
            boundaries: BTreeSet::new(),
            world_seq: 0,
            committed,
            round: 0,
            outcome: None,
            log: Vec::new(),
            effects: Fnv::new(),
        })
    }

    pub fn params(&self) -> &Params {
        &self.params
    }

    pub fn machine(&self, m: usize) -> &MachineState {
        &self.machines[m]
    }

    pub fn machines(&self) -> usize {
        self.machines.len()
    }

    /// The last committed round's end.
    pub fn committed(&self) -> Time {
        self.committed
    }

    pub fn rounds(&self) -> &[RoundRecord] {
        &self.log
    }

    /// Digest of every routed effect, in commit order.
    pub fn effects_digest(&self) -> u64 {
        self.effects.0
    }

    /// Events still queued, per machine in key order.
    pub fn pending(&self) -> impl Iterator<Item = (usize, &EventKey, &P)> {
        self.queues
            .iter()
            .enumerate()
            .flat_map(|(m, q)| q.iter().map(move |(k, p)| (m, k, p)))
    }

    /// Replace machine `m`'s committed state between rounds, after a
    /// lifecycle transition the coordinator carried out on it (a pause,
    /// resume, crash, reboot or park). Its output sequence is kept.
    pub fn sync(
        &mut self,
        m: usize,
        time: Time,
        next: Time,
        exited: Option<Time>,
        suspended: bool,
    ) -> Result<(), Error> {
        if m >= self.machines.len() {
            return Err(Error::Params(format!("sync of unknown machine {m}")));
        }
        if exited.is_none() && next < time {
            return Err(Error::Params(format!(
                "machine {m}: next action {next} is before its time {time}"
            )));
        }
        let s = &mut self.machines[m];
        s.time = time;
        s.next = if exited.is_some() { NEVER } else { next };
        s.exited = exited;
        s.suspended = suspended;
        Ok(())
    }

    /// Take the queued events of machine `m` that `keep` rejects, in key
    /// order. For the coordinator's lifecycle transitions, between rounds.
    pub fn remove_queued(
        &mut self,
        m: usize,
        mut keep: impl FnMut(&EventKey, &P) -> bool,
    ) -> Vec<(EventKey, P)> {
        let q = std::mem::take(&mut self.queues[m]);
        let mut removed = Vec::new();
        for (k, p) in q {
            if keep(&k, &p) {
                self.queues[m].insert(k, p);
            } else {
                removed.push((k, p));
            }
        }
        removed
    }

    /// Queue a coordinator event for machine `to`. Once rounds have run, it
    /// must not be due inside the committed past: every machine stands at
    /// or before the committed time, so it still delivers on time.
    pub fn schedule(&mut self, to: usize, at: Time, payload: P) -> Result<(), Error> {
        if to >= self.machines.len() {
            return Err(Error::Params(format!(
                "scheduled event for unknown machine {to}"
            )));
        }
        if self.round > 0 && at < self.committed {
            return Err(Error::Params(format!(
                "event at {at} ns is inside the committed past"
            )));
        }
        let key = EventKey {
            at,
            class: Class::World,
            source: Source::World,
            seq: self.world_seq,
        };
        self.world_seq += 1;
        self.queues[to].insert(key, payload);
        Ok(())
    }

    /// Every machine stops exactly at `at`, and nothing at or after it is
    /// delivered before it.
    pub fn add_boundary(&mut self, at: Time) -> Result<(), Error> {
        if at < self.committed && self.round > 0 {
            return Err(Error::Params(format!(
                "boundary {at} ns is in the committed past"
            )));
        }
        self.boundaries.insert(at);
        Ok(())
    }

    /// The earliest time machine `m` can next act.
    pub fn key(&self, m: usize) -> Time {
        let s = &self.machines[m];
        if s.exited.is_some() || s.suspended {
            return NEVER;
        }
        let queued = self.queues[m].keys().next().map_or(NEVER, |k| k.at);
        s.next.min(queued).max(s.time)
    }

    /// Plan the next round, or say how the world ended.
    pub fn plan(&self) -> Step<P> {
        if let Some(outcome) = self.outcome {
            return Step::Done(outcome);
        }
        let keys: Vec<Time> = (0..self.machines.len()).map(|m| self.key(m)).collect();
        let start = keys.iter().copied().min().unwrap_or(NEVER);
        if start == NEVER {
            return Step::Done(Outcome::Quiescent {
                committed: self.committed,
            });
        }
        let duration = self.params.duration;
        if start >= duration {
            return Step::Done(Outcome::Duration { at: duration });
        }
        // Boundaries at or before `start` have passed: no machine can act
        // before `start`, so each one already stands at or before them.
        let next_boundary = self
            .boundaries
            .range(start + 1..)
            .next()
            .copied()
            .unwrap_or(NEVER);
        let end = start
            .saturating_add(self.params.window())
            .min(duration)
            .min(next_boundary);
        let exact = end == duration || end == next_boundary;
        // Early delivery never crosses a later boundary or the end.
        let deliver_before = if exact {
            end
        } else {
            let after = self
                .boundaries
                .range(end..)
                .next()
                .copied()
                .unwrap_or(NEVER);
            end.saturating_add(self.params.early)
                .min(after)
                .min(duration)
        };
        let assignments = (0..self.machines.len())
            .filter(|&m| keys[m] < end)
            .map(|m| Assignment {
                machine: m,
                wake: keys[m],
                stop: end,
                stop_early: if exact {
                    0
                } else {
                    self.params.early.min((end - start) / 2)
                },
                delivery_early: self.params.early,
                incoming: self.queues[m]
                    .range(
                        ..EventKey {
                            at: deliver_before,
                            class: Class::World,
                            source: Source::World,
                            seq: 0,
                        },
                    )
                    .map(|(k, p)| Event {
                        key: *k,
                        payload: p.clone(),
                    })
                    .collect(),
                max_outputs: self.params.max_outputs,
            })
            .collect();
        Step::Round(RoundPlan {
            round: self.round,
            start,
            end,
            exact,
            assignments,
        })
    }

    /// Check every report against its assignment, route the round's outputs
    /// in a fixed order, and commit. On error nothing is committed.
    pub fn commit<R: Router<P>>(
        &mut self,
        plan: &RoundPlan<P>,
        reports: Vec<Report<P>>,
        router: &mut R,
    ) -> Result<(), Error> {
        if plan.round != self.round || self.outcome.is_some() {
            return Err(Error::Params(format!(
                "plan for round {} is stale",
                plan.round
            )));
        }
        if reports.len() != plan.assignments.len() {
            return Err(Error::Params(
                "one report is required per assignment".into(),
            ));
        }
        for (a, r) in plan.assignments.iter().zip(&reports) {
            self.check_report(a, r)?;
        }

        // Every report is valid: order the outputs independently of which
        // worker finished first, then route them.
        let mut outputs = Vec::new();
        for (a, r) in plan.assignments.iter().zip(&reports) {
            let base = self.machines[a.machine].sent;
            for (i, o) in r.outputs.iter().enumerate() {
                outputs.push((o.sent_at, a.machine, base + i as u64, o.payload.clone()));
            }
        }
        outputs.sort_by_key(|o| (o.0, o.1, o.2));
        let mut routed = Vec::new();
        let mut copies = Vec::new();
        for (sent_at, from, seq, payload) in outputs {
            copies.clear();
            router.route(from, sent_at, payload, &mut copies);
            for d in copies.drain(..) {
                if d.to >= self.machines.len() {
                    return Err(Error::Params(format!(
                        "router delivered to unknown machine {}",
                        d.to
                    )));
                }
                // A receiver that stopped before `end` delivers anything due
                // by its time plus `early`; so must anything produced here.
                if d.at < plan.end.saturating_add(self.params.early) || d.at < sent_at {
                    return Err(Error::Causality {
                        from,
                        to: d.to,
                        sent_at,
                        at: d.at,
                        round_end: plan.end,
                    });
                }
                let key = EventKey {
                    at: d.at,
                    class: Class::Frame,
                    source: Source::Machine(from),
                    seq,
                };
                routed.push((d.to, key, d.payload));
            }
        }
        let mut seen = BTreeSet::new();
        for (to, key, _) in &routed {
            if !seen.insert((*to, *key)) {
                return Err(Error::Params(format!(
                    "router delivered output {} of machine {:?} twice to {to}",
                    key.seq, key.source
                )));
            }
        }

        // Commit.
        let mut ended = None;
        let mut output_count = 0;
        for (a, r) in plan.assignments.iter().zip(reports) {
            let q = &mut self.queues[a.machine];
            for e in &a.incoming[..r.consumed] {
                q.remove(&e.key);
            }
            let s = &mut self.machines[a.machine];
            s.time = r.time;
            s.next = if r.exited.is_some() { NEVER } else { r.next };
            s.exited = r.exited;
            s.suspended = r.suspended && r.exited.is_none();
            s.sent += r.outputs.len() as u64;
            output_count += r.outputs.len();
            if let (Some(at), Some(end_on)) = (r.exited, self.params.end_on) {
                if end_on == a.machine {
                    ended = Some(Outcome::Exited {
                        machine: a.machine,
                        exit_at: at,
                        committed: plan.end,
                    });
                }
            }
        }
        let deliveries = routed.len();
        for (to, key, payload) in routed {
            (to, key, &payload).hash(&mut self.effects);
            self.queues[to].insert(key, payload);
        }
        self.log.push(RoundRecord {
            round: plan.round,
            start: plan.start,
            end: plan.end,
            exact: plan.exact,
            active: plan.assignments.len(),
            outputs: output_count,
            deliveries,
        });
        self.committed = plan.end;
        self.round += 1;
        self.outcome = ended;
        Ok(())
    }

    fn check_report(&self, a: &Assignment<P>, r: &Report<P>) -> Result<(), Error> {
        let bad = |reason: String| {
            Err(Error::Report {
                machine: a.machine,
                reason,
            })
        };
        let before = &self.machines[a.machine];
        if r.outputs.len() > a.max_outputs {
            return Err(Error::OutputLimit {
                machine: a.machine,
                outputs: r.outputs.len(),
                limit: a.max_outputs,
            });
        }
        // An idle machine whose wake falls in [slice end, stop) stays put.
        if r.time < before.time || r.time > a.stop {
            return bad(format!(
                "time {} outside {}..={}",
                r.time, before.time, a.stop
            ));
        }
        match r.exited {
            Some(at) if at < a.wake => {
                return bad(format!("exited at {at} before waking at {}", a.wake))
            }
            Some(at) if at != r.time => {
                return bad(format!("exited at {at} but stopped at {}", r.time))
            }
            // A machine may stop short only when idle: it cannot act, and
            // no assigned event wakes it, before the slice end. A suspended
            // one acts on nothing until the coordinator resumes it.
            None if !r.suspended
                && r.time < a.slice_end()
                && (r.next < a.slice_end()
                    || a.incoming
                        .get(r.consumed)
                        .is_some_and(|e| e.key.at < a.slice_end())) =>
            {
                return bad(format!(
                    "stopped at {} before {} while able to act",
                    r.time,
                    a.slice_end()
                ))
            }
            None if r.next < r.time => {
                return bad(format!(
                    "next action {} is before its time {}",
                    r.next, r.time
                ))
            }
            _ => {}
        }
        if r.consumed > a.incoming.len() {
            return bad(format!(
                "consumed {} of {} events",
                r.consumed,
                a.incoming.len()
            ));
        }
        // An undelivered event must still be in the machine's future.
        if r.exited.is_none() && !r.suspended {
            if let Some(e) = a.incoming.get(r.consumed) {
                if e.key.at < r.time {
                    return bad(format!(
                        "event due at {} not delivered by {}",
                        e.key.at, r.time
                    ));
                }
            }
        }
        let mut last = a.wake;
        for o in &r.outputs {
            if o.sent_at < last || o.sent_at > r.time {
                return bad(format!(
                    "output at {} outside {last}..={} or out of order",
                    o.sent_at, r.time
                ));
            }
            last = o.sent_at;
        }
        Ok(())
    }
}

/// FNV-1a: a stable digest with no dependencies.
struct Fnv(u64);

impl Fnv {
    fn new() -> Self {
        Self(0xcbf2_9ce4_8422_2325)
    }
}

impl Hasher for Fnv {
    fn finish(&self) -> u64 {
        self.0
    }

    fn write(&mut self, bytes: &[u8]) {
        for b in bytes {
            self.0 ^= *b as u64;
            self.0 = self.0.wrapping_mul(0x0100_0000_01b3);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params() -> Params {
        Params {
            latency: 100,
            early: 20,
            duration: 10_000,
            end_on: None,
            max_outputs: 8,
        }
    }

    fn round(w: &World<u32>) -> RoundPlan<u32> {
        match w.plan() {
            Step::Round(p) => p,
            Step::Done(o) => panic!("unexpected end {o:?}"),
        }
    }

    #[test]
    fn early_at_or_above_latency_is_rejected() {
        let p = Params {
            early: 100,
            ..params()
        };
        assert!(matches!(World::<u32>::new(2, p), Err(Error::Params(_))));
    }

    #[test]
    fn window_is_latency_less_early() {
        let w = World::<u32>::new(2, params()).unwrap();
        let p = round(&w);
        assert_eq!((p.start, p.end, p.exact), (0, 80, false));
        assert_eq!(p.assignments.len(), 2);
        assert!(p.assignments.iter().all(|a| a.stop_early == 20));
    }

    #[test]
    fn horizon_stops_land_at_most_half_a_round_early() {
        let w = World::<u32>::new(
            1,
            Params {
                latency: 100,
                early: 50,
                ..params()
            },
        )
        .unwrap();
        let p = round(&w);
        assert_eq!(
            (
                p.end,
                p.assignments[0].stop_early,
                p.assignments[0].slice_end()
            ),
            (50, 25, 25)
        );
    }

    #[test]
    fn boundary_and_duration_cap_the_round_exactly() {
        let mut w = World::<u32>::new(
            1,
            Params {
                duration: 50,
                ..params()
            },
        )
        .unwrap();
        assert_eq!((round(&w).end, round(&w).exact), (50, true));
        w = World::<u32>::new(1, params()).unwrap();
        w.add_boundary(30).unwrap();
        let p = round(&w);
        assert_eq!((p.end, p.exact, p.assignments[0].stop_early), (30, true, 0));
    }

    #[test]
    fn early_delivery_never_crosses_a_boundary() {
        let mut w = World::<u32>::new(1, params()).unwrap();
        w.add_boundary(90).unwrap();
        w.schedule(0, 85, 1).unwrap();
        w.schedule(0, 95, 2).unwrap();
        // Horizon round [0, 80): events up to 80 + 20 could be early, but
        // the boundary at 90 bounds them.
        let p = round(&w);
        assert_eq!(p.end, 80);
        assert_eq!(
            p.assignments[0]
                .incoming
                .iter()
                .map(|e| e.payload)
                .collect::<Vec<_>>(),
            vec![1]
        );
    }

    #[test]
    fn idle_machines_sit_out_and_time_jumps() {
        let mut w = World::<u32>::new(2, params()).unwrap();
        w.machines[0].next = NEVER;
        w.machines[1].next = 5_000;
        let p = round(&w);
        assert_eq!((p.start, p.end), (5_000, 5_080));
        assert_eq!(
            p.assignments
                .iter()
                .map(|a| (a.machine, a.wake))
                .collect::<Vec<_>>(),
            vec![(1, 5_000)]
        );
    }

    #[test]
    fn nothing_left_is_quiescent_and_late_start_is_duration() {
        let mut w = World::<u32>::new(1, params()).unwrap();
        w.machines[0].next = NEVER;
        assert!(matches!(
            w.plan(),
            Step::Done(Outcome::Quiescent { committed: 0 })
        ));
        w.machines[0].next = 10_000;
        assert!(matches!(
            w.plan(),
            Step::Done(Outcome::Duration { at: 10_000 })
        ));
    }

    struct Fixed(Time);
    impl Router<u32> for Fixed {
        fn route(
            &mut self,
            from: usize,
            sent_at: Time,
            payload: u32,
            out: &mut Vec<Delivery<u32>>,
        ) {
            out.push(Delivery {
                to: 1 - from,
                at: sent_at + self.0,
                payload,
            });
        }
    }

    fn report(time: Time, outputs: Vec<Output<u32>>) -> Report<u32> {
        Report {
            time,
            next: time,
            consumed: 0,
            outputs,
            exited: None,
            suspended: false,
        }
    }

    #[test]
    fn simultaneous_sends_order_by_machine_then_sequence() {
        let mut w = World::<u32>::new(2, params()).unwrap();
        let p = round(&w);
        let out = |a, b| {
            vec![
                Output {
                    sent_at: 10,
                    payload: a,
                },
                Output {
                    sent_at: 10,
                    payload: b,
                },
            ]
        };
        w.commit(
            &p,
            vec![report(80, out(1, 2)), report(80, out(3, 4))],
            &mut Fixed(100),
        )
        .unwrap();
        let order: Vec<_> = w
            .pending()
            .map(|(m, k, p)| (m, k.source, k.seq, *p))
            .collect();
        assert_eq!(
            order,
            vec![
                (0, Source::Machine(1), 0, 3),
                (0, Source::Machine(1), 1, 4),
                (1, Source::Machine(0), 0, 1),
                (1, Source::Machine(0), 1, 2)
            ]
        );
    }

    #[test]
    fn a_frame_due_exactly_at_the_round_end_plus_early_is_admitted() {
        let mut w = World::<u32>::new(2, params()).unwrap();
        let p = round(&w);
        // Sent at T = 0 with exactly `latency`: due at 100 = B + early.
        let r = vec![
            report(
                80,
                vec![Output {
                    sent_at: 0,
                    payload: 7,
                }],
            ),
            report(60, vec![]),
        ];
        w.commit(&p, r, &mut Fixed(100)).unwrap();
        assert_eq!(w.committed(), 80);
    }

    #[test]
    fn negative_control_a_short_link_is_detected() {
        let mut w = World::<u32>::new(2, params()).unwrap();
        let p = round(&w);
        let r = vec![
            report(
                80,
                vec![Output {
                    sent_at: 0,
                    payload: 7,
                }],
            ),
            report(80, vec![]),
        ];
        let e = w.commit(&p, r, &mut Fixed(99)).unwrap_err();
        assert!(
            matches!(
                e,
                Error::Causality {
                    at: 99,
                    round_end: 80,
                    ..
                }
            ),
            "{e}"
        );
        // Nothing was committed.
        assert_eq!(
            (w.committed(), w.rounds().len(), w.pending().count()),
            (0, 0, 0)
        );
    }

    #[test]
    fn reports_that_break_the_assignment_are_rejected() {
        let mut w = World::<u32>::new(2, params()).unwrap();
        w.schedule(0, 10, 9).unwrap();
        let p = round(&w);
        let cases = [
            (report(50, vec![]), "before"),
            (report(81, vec![]), "outside"),
            (
                Report {
                    consumed: 0,
                    ..report(80, vec![])
                },
                "not delivered",
            ),
        ];
        for (r, why) in cases {
            let e = w
                .commit(&p, vec![r, report(80, vec![])], &mut Fixed(100))
                .unwrap_err();
            assert!(e.to_string().contains(why), "{e}");
        }
        let many = (0..9)
            .map(|i| Output {
                sent_at: 0,
                payload: i,
            })
            .collect();
        let e = w
            .commit(
                &p,
                vec![report(80, many), report(80, vec![])],
                &mut Fixed(100),
            )
            .unwrap_err();
        assert!(matches!(
            e,
            Error::OutputLimit {
                machine: 0,
                outputs: 9,
                limit: 8
            }
        ));
    }

    #[test]
    fn the_designated_exit_ends_after_its_round() {
        let mut w = World::<u32>::new(
            2,
            Params {
                end_on: Some(1),
                ..params()
            },
        )
        .unwrap();
        let p = round(&w);
        let exit = Report {
            time: 33,
            next: NEVER,
            consumed: 0,
            outputs: vec![],
            exited: Some(33),
            suspended: false,
        };
        w.commit(&p, vec![report(80, vec![]), exit], &mut Fixed(100))
            .unwrap();
        assert!(matches!(
            w.plan(),
            Step::Done(Outcome::Exited {
                machine: 1,
                exit_at: 33,
                committed: 80
            })
        ));
    }
}
