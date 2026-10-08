//! One deterministic world coordinator over stopped architecture primitives.
//! MachineState is stored once in the real machine/proxy; it is never refreshed
//! from an independently mutable scheduler mirror.
use crate::world_events as events;
use crate::{control, faults, links, network_counters, rng::Rng};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::ops::{Deref, DerefMut};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    Running,
    Halted,
    Exited(u8),
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stop {
    Reached,
    Halted,
    Exited,
    Sent,
    Barrier,
    Control,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reseed {
    Barrier,
    Page,
    Unsupported,
}
impl Reseed {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Barrier => "barrier",
            Self::Page => "time_page",
            Self::Unsupported => "unsupported",
        }
    }
}

/// The single scheduler-visible record. Execution primitives commit time and
/// observed CPU state at advance/complete/restore boundaries. World orchestration
/// owns pause/park/down/incarnation transitions; adapters execute their physical
/// consequences. Effects are ordered within their committed channel; a console
/// observation stops on the first committed byte when requested by a trigger.
/// IPC adapters validate incarnation/generation and monotonic observations before
/// updating this record. Neither side runs an independent world scheduler.
#[derive(Clone, Debug)]
pub struct MachineState {
    pub name: String,
    pub time: u64,
    pub state: State,
    pub parked: bool,
    pub paused: bool,
    pub down: bool,
    pub incarnation: u32,
    pub booted_at: u64,
    pub fault_effects: Vec<(u32, u64, bool)>,
    pub control_requests: Vec<(u64, Vec<u8>)>,
    pub observe_console: bool,
    pub committed_console: Option<(u64, u8)>,
}
impl MachineState {
    pub fn new(name: String) -> Self {
        Self {
            name,
            time: 0,
            state: State::Running,
            parked: false,
            paused: false,
            down: false,
            incarnation: 0,
            booted_at: 0,
            fault_effects: vec![],
            control_requests: vec![],
            observe_console: false,
            committed_console: None,
        }
    }
}
/// Shared lifecycle policy. Adapters call these at the original commit sites:
/// guards precede fallible physical actions, and metadata commits keep their
/// position relative to timer expiry, CPU completion and witness writes.
impl MachineState {
    pub fn begin_release(&mut self) -> Result<(), String> {
        if !self.parked {
            return Err(format!(
                "{}: released without reaching the launch barrier",
                self.name
            ));
        }
        self.parked = false;
        Ok(())
    }
    pub fn pause(&mut self) -> Result<(), String> {
        if self.paused || self.down || self.parked {
            return Err(format!("{}: cannot pause here", self.name));
        }
        self.paused = true;
        Ok(())
    }
    pub fn resume(&mut self, at: u64) -> Result<(), String> {
        if !self.paused {
            return Err(format!("{}: resumed without being paused", self.name));
        }
        self.paused = false;
        self.time = self.time.max(at);
        Ok(())
    }
    pub fn boot_at(&mut self, at: u64) -> Result<(), String> {
        if !self.down || at < self.time {
            return Err(format!("{}: cannot boot here", self.name));
        }
        self.down = false;
        self.time = at;
        self.booted_at = at;
        Ok(())
    }
    pub fn succeed(&mut self, old: &Self, at: u64) -> Result<(), String> {
        self.incarnation = old
            .incarnation
            .checked_add(1)
            .ok_or("incarnation overflow")?;
        self.down = true;
        self.time = at;
        Ok(())
    }
    /// Check before completing the architecture exit. Mark parked only after
    /// that completion succeeds; a failure must not advertise a fork point.
    pub fn check_park(&self, owes_io: bool) -> Result<(), String> {
        if self.parked || owes_io {
            return Err(format!("{}: cannot park here", self.name));
        }
        Ok(())
    }
    pub fn commit_park(&mut self) {
        self.parked = true;
    }
    pub fn clear_effects(&mut self) {
        self.fault_effects.clear();
        self.control_requests.clear();
        self.committed_console = None;
    }
    pub fn restore_lifecycle(&mut self) {
        self.paused = false;
        self.down = false;
        self.incarnation = 0;
        self.booted_at = 0;
    }
    /// Called after the architecture's idle-time accounting has advanced time.
    pub fn wake(&mut self) {
        if self.state == State::Halted {
            self.state = State::Running;
        }
    }
}
#[derive(Clone, Copy, Debug)]
pub struct SliceRequest {
    pub hard_limit: u64,
    pub early_slack: u64,
    pub observe_console: bool,
}

/// Architecture primitives over the one shared record. Mutable metadata access
/// is restricted to the writer contract above, not independent scheduling. No
/// factory, clock or snapshot type assumes a PMU or a particular host API.
pub trait WorldMachine: Deref<Target = MachineState> + DerefMut + Sized {
    type Factory;
    type Boot;
    type Clock;
    type Snapshot;
    fn boot(
        factory: &Self::Factory,
        name: &str,
        boot: &Self::Boot,
        barrier: bool,
        entropy: [u8; 32],
    ) -> Result<Self, String>;
    fn has_pending_irq(&self) -> bool;
    fn next_deadline(&self) -> u64;
    fn owed_io(&self) -> Option<u64>;
    fn has_control(&self) -> bool;
    fn deliver_control(&mut self, bytes: &[u8]) -> Result<(), String>;
    fn deliver_serial(&mut self, bytes: &[u8]) -> Result<(), String>;
    fn deliver_frame(&mut self, bytes: &[u8]) -> Result<bool, String>;
    fn pause(&mut self) -> Result<(), String>;
    fn resume(&mut self, at: u64) -> Result<(), String>;
    fn boot_at(&mut self, at: u64) -> Result<(), String>;
    fn succeed(&mut self, old: &mut Self, at: u64, seed: u64) -> Result<Option<u64>, String>;
    fn succeed_branch(
        &mut self,
        old: &mut Self,
        at: u64,
        seed: u64,
        identity: Option<&str>,
    ) -> Result<Option<u64>, String> {
        if identity.is_some() {
            return Err("runtime checkpoint succession is unsupported by this backend".into());
        }
        self.succeed(old, at, seed)
    }
    /// Observation fields shared by terminal and crashed incarnations.
    fn crash_observations(&self) -> Value {
        Value::Null
    }

    fn park(&mut self) -> Result<(), String>;
    fn wake(&mut self, at: u64) -> Result<(), String>;
    fn release(&mut self, at: u64, entropy: [u32; 8]) -> Result<Reseed, String>;
    fn abi(&self) -> u64;
    fn seed_timer(&mut self, seed: u64, domain: &str) -> Result<(), String>;
    fn seed_timer_branch(
        &mut self,
        seed: u64,
        identity: Option<&str>,
        domain: &str,
    ) -> Result<(), String> {
        if identity.is_some() {
            return Err("runtime checkpoint timer reseeding is unsupported by this backend".into());
        }
        self.seed_timer(seed, domain)
    }
    fn reseed_guest(&mut self, _: [u32; 8]) -> Result<(), String> {
        Err("runtime checkpoint guest reseeding is unsupported by this backend".into())
    }
    fn checkpoint_boundary(_: &Self::Clock) -> Result<(), String> {
        Err("runtime checkpoints are unsupported by this backend".into())
    }
    fn restore_boundary(_: &mut Self::Clock) -> Result<(), String> {
        Err("runtime checkpoints are unsupported by this backend".into())
    }

    fn supports_speed_reseed(&self) -> bool;
    fn seed_speed(&mut self, seed: u64, domain: &str) -> Result<(), String>;
    fn branches(&self) -> u64;
    fn exits(&self) -> u64;
    fn exit_witness(&mut self) -> Result<String, String>;
    /// Idempotently poison, close and reap owned worker transport without another
    /// guest request. In-process adapters explicitly succeed without a child.
    fn terminate_worker(&mut self) -> Result<(), String>;
    fn snapshot(&mut self) -> Result<Self::Snapshot, String>;
    fn restore(&mut self, snapshot: &Self::Snapshot) -> Result<u64, String>;
    /// Return at the first causal effect. Frames share this stop's committed
    /// timestamp; never batch frames past that stop or advance past hard_limit
    /// including when I/O remains owed. The coordinator chooses any larger
    /// fork limit explicitly, still clamped by every causal boundary.
    fn advance(
        &mut self,
        clock: &mut Self::Clock,
        request: SliceRequest,
        frames: &mut Vec<Vec<u8>>,
    ) -> Result<Stop, String>;
}
/// Immutable slice preconditions, checked before any returned effects enter the
/// world. Channel order remains the reference KVM order, not a merged batch.
struct SliceObservation {
    name: String,
    time: u64,
    incarnation: u32,
    booted_at: u64,
    parked: bool,
    paused: bool,
    down: bool,
}
impl SliceObservation {
    fn begin(m: &MachineState, request: SliceRequest) -> Result<Self, String> {
        if m.time > request.hard_limit {
            return Err(format!("{}: slice limit is in committed past", m.name));
        }
        if !m.fault_effects.is_empty()
            || !m.control_requests.is_empty()
            || m.committed_console.is_some()
        {
            return Err(format!("{}: undrained effects before slice", m.name));
        }
        Ok(Self {
            name: m.name.clone(),
            time: m.time,
            incarnation: m.incarnation,
            booted_at: m.booted_at,
            parked: m.parked,
            paused: m.paused,
            down: m.down,
        })
    }
    fn validate(&self, m: &MachineState, request: SliceRequest, stop: Stop) -> Result<(), String> {
        let error = |reason: &str| format!("{}: invalid stopped observation: {reason}", self.name);
        if m.name != self.name
            || m.incarnation != self.incarnation
            || m.booted_at != self.booted_at
            || m.paused != self.paused
            || m.down != self.down
            || m.observe_console != request.observe_console
            || m.parked != (self.parked || stop == Stop::Barrier)
        {
            return Err(error("identity or lifecycle changed during slice"));
        }
        if m.time < self.time || m.time > request.hard_limit {
            return Err(error("time outside slice hard bounds"));
        }
        let state_ok = match stop {
            Stop::Exited => matches!(m.state, State::Exited(_)),
            Stop::Halted => m.state == State::Halted,
            _ => m.state == State::Running,
        };
        if !state_ok {
            return Err(error("stop and CPU state disagree"));
        }
        let in_slice = |at| at >= self.time && at <= m.time;
        if m.committed_console
            .is_some_and(|(at, _)| !request.observe_console || !in_slice(at))
        {
            return Err(error("console effect outside observed slice"));
        }
        fn ordered(times: impl Iterator<Item = u64>, start: u64, end: u64) -> bool {
            let mut previous = start;
            for at in times {
                if at < previous || at > end {
                    return false;
                }
                previous = at;
            }
            true
        }
        if !ordered(
            m.fault_effects.iter().map(|(_, at, _)| *at),
            self.time,
            m.time,
        ) || !ordered(
            m.control_requests.iter().map(|(at, _)| *at),
            self.time,
            m.time,
        ) {
            return Err(error("effect timestamps outside committed channel order"));
        }
        Ok(())
    }
}

#[derive(Clone)]
pub enum Payload {
    Frame { from: usize, bytes: Vec<u8> },
    Serial(Vec<u8>),
    Partition { from: usize, to: usize, on: bool },
}
const PAUSED_INGRESS_FRAMES: u64 = 256;
pub fn entropy_seed(seed: u64, domain: &str) -> [u8; 32] {
    let mut rng = Rng::new(seed, domain);
    let mut out = [0u8; 32];
    for c in out.chunks_mut(8) {
        c.copy_from_slice(&rng.next().to_le_bytes());
    }
    out
}

pub struct World<M: WorldMachine> {
    /// A failed crash transaction is terminal even if a direct caller retains World.
    pub failure: Option<String>,
    pub machines: Vec<M>,
    pub factory: M::Factory,
    pub boot: Vec<M::Boot>,
    /// The first incarnation parks at its launch barrier.
    pub barrier: bool,
    pub boot_seed: u64,
    /// The seed the world plan was armed with; reboots draw from it.
    pub run_seed: u64,
    pub branch_identity: Option<String>,
    /// Per machine, a summary of each incarnation that crashed.
    pub incarnations: Vec<Vec<Value>>,
    /// Frames queued for each paused machine during its current pause.
    pub paused_ingress: Vec<u64>,
    pub clock: M::Clock,
    pub events: Vec<BTreeMap<(u64, u64), Payload>>,
    pub seq: u64,
    /// No frame can arrive sooner than this after it is sent: the horizon.
    pub lookahead: u64,
    /// Slices may end, and events be delivered, up to this long before their
    /// time. Below `lookahead`, so nothing is ever delivered into the past.
    pub early: u64,
    pub latency: u64,
    pub partitioned: BTreeMap<(usize, usize), bool>,
    pub faults: faults::Faults,
    pub control: control::Control,
    pub strict_events: Vec<BTreeMap<(u64, u64), Vec<u8>>>,
    pub macs: Vec<[u8; 6]>,
    pub dropped: u64,
    pub delivered: u64,
    pub link_counts: BTreeMap<(usize, usize), [u64; 4]>,
    pub semantic_counts: network_counters::NetworkCounters,
    pub slices: u64,
}

impl<M: WorldMachine> World<M> {
    pub fn check_active(&self) -> Result<(), String> {
        match &self.failure {
            Some(reason) => Err(format!("world is terminal after failed crash: {reason}")),
            None => Ok(()),
        }
    }
    /// Common admission for direct and IPC adapters, before any execution.
    pub fn validate(&self) -> Result<(), String> {
        self.check_active()?;
        let n = self.machines.len();
        if n == 0 {
            return Err("world requires at least one machine".into());
        }
        if [
            self.boot.len(),
            self.incarnations.len(),
            self.paused_ingress.len(),
            self.events.len(),
            self.strict_events.len(),
            self.macs.len(),
        ]
        .iter()
        .any(|&len| len != n)
        {
            return Err("world per-machine input lengths differ".into());
        }
        if self.early >= self.lookahead {
            return Err("world early slack must be below lookahead".into());
        }
        let mut names = std::collections::BTreeSet::new();
        let mut macs = std::collections::BTreeSet::new();
        for (m, mac) in self.machines.iter().zip(&self.macs) {
            if m.name.is_empty() || !names.insert(&m.name) {
                return Err("world machine names must be nonempty and unique".into());
            }
            if !macs.insert(mac) {
                return Err("world machine MACs must be unique".into());
            }
        }
        Ok(())
    }

    /// Finalize retained control and trigger evidence at the run's semantic end.
    /// Other machines may have advanced by lookahead past an exiting target.
    pub fn finish(
        &mut self,
        outcome: &str,
        duration: u64,
        end_on: &Option<String>,
    ) -> Result<u64, String> {
        self.check_active()?;
        let cutoff = if outcome == "duration" {
            duration
        } else if outcome == "end_machine_exited" {
            end_on
                .as_ref()
                .and_then(|name| self.machines.iter().find(|m| &m.name == name))
                .map(|m| m.time)
                .unwrap_or(0)
        } else {
            self.machines.iter().map(|m| m.time).max().unwrap_or(0)
        };
        self.control.cut_off(&self.faults, cutoff);
        self.control.triggers.cut_off(cutoff);
        Ok(cutoff)
    }

    fn count_link(&mut self, at: u64, from: usize, to: usize, outcome: usize) {
        self.link_counts.entry((from, to)).or_default()[outcome] += 1;
        self.semantic_counts.record(at, from, to, outcome);
    }
    pub fn network_snapshot(&self) -> Value {
        self.counter_snapshot(&self.link_counts)
    }
    pub fn counter_snapshot(&self, counts: &BTreeMap<(usize, usize), [u64; 4]>) -> Value {
        let snapshot:BTreeMap<_,_>=counts.iter().map(|(&(from,to),counts)|(
            format!("{}->{}",self.machines[from].name,self.machines[to].name),
            {
                // Omitted while zero, so a run that loses nothing keeps main's counters.
                let mut c=json!({"delivered":counts[0],"partition_dropped":counts[1],"receiver_dropped":counts[2]});
                if counts[3]>0 {c["loss_dropped"]=json!(counts[3]);}
                c
            }
        )).collect();
        json!(snapshot)
    }
    pub fn schedule(&mut self, m: usize, t: u64, p: Payload) {
        self.seq += 1;
        self.events[m].insert((t, self.seq), p);
    }

    pub fn next_event(&self, m: usize) -> u64 {
        self.events[m]
            .keys()
            .next()
            .map(|k| k.0)
            .unwrap_or(u64::MAX)
            .min(
                self.strict_events[m]
                    .keys()
                    .next()
                    .map(|k| k.0)
                    .unwrap_or(u64::MAX),
            )
    }

    /// Earliest virtual time at which machine m can next act.
    pub fn key(&self, m: usize) -> u64 {
        let mach = &self.machines[m];
        if mach.parked || mach.paused || mach.down {
            return u64::MAX;
        }
        match mach.state {
            State::Exited(_) => u64::MAX,
            State::Running => mach.time,
            State::Halted => {
                if mach.has_pending_irq() {
                    mach.time
                } else {
                    mach.next_deadline().min(self.next_event(m)).max(mach.time)
                }
            }
        }
    }

    /// Deliver events due by the end of the machine's early window.
    pub fn deliver_due(&mut self, m: usize) -> Result<(), String> {
        self.check_active()?;
        while let Some(bytes) = events::pop_due(&mut self.strict_events[m], self.machines[m].time) {
            if self.machines[m].has_control() {
                self.machines[m].deliver_control(&bytes)?;
            } else {
                self.machines[m].deliver_serial(&bytes)?;
            }
            if bytes.starts_with(b"SBX_REPLY1 ") {
                self.control.mark_delivered(self.machines[m].time)?;
            }
        }
        // Never early across this machine's next lifecycle boundary: what
        // arrives at or after a pause or crash must not reach it before.
        let now = events::early_bound(
            self.machines[m].time,
            self.early,
            self.faults.next_lifecycle(m),
        );
        while let Some((at, p)) = events::pop_due_at(&mut self.events[m], now) {
            let mach = &mut self.machines[m];
            match p {
                Payload::Frame { from, bytes } => {
                    if !mach.deliver_frame(&bytes)? {
                        self.dropped += 1;
                        self.count_link(at, from, m, 2);
                    } else {
                        self.delivered += 1;
                        self.count_link(at, from, m, 0);
                    }
                }
                Payload::Serial(b) => mach.deliver_serial(&b)?,
                Payload::Partition { from, to, on } => {
                    self.partitioned.insert((from, to), on);
                }
            }
        }
        Ok(())
    }

    pub fn route(&mut self, src: usize, send_time: u64, frame: Vec<u8>) {
        if frame.len() < 14 {
            return;
        }
        let dst_mac: [u8; 6] = frame[0..6].try_into().unwrap();
        let targets = links::candidates(src, dst_mac, &self.macs);
        for dst in targets {
            let last = self.faults.links.last.get(&(src, dst)).copied();
            let cut = *self.partitioned.get(&(src, dst)).unwrap_or(&false);
            let t = match self.faults.route(src, dst, send_time, &frame, cut) {
                links::Outcome::Cut => {
                    self.dropped += 1;
                    self.count_link(send_time, src, dst, 1);
                    continue;
                }
                links::Outcome::Lost => {
                    self.dropped += 1;
                    self.count_link(send_time, src, dst, 3);
                    continue;
                }
                links::Outcome::Deliver(t) => t,
            };
            // A frame arriving while its receiver is down meets no NIC; one
            // arriving while it is paused waits, up to a fixed backlog.
            let to = &self.machines[dst];
            match events::arrival(
                t,
                self.faults.next_lifecycle(dst),
                to.down,
                to.paused,
                self.paused_ingress[dst],
                PAUSED_INGRESS_FRAMES,
            ) {
                events::Arrival::Queue => {}
                events::Arrival::Held => self.paused_ingress[dst] += 1,
                events::Arrival::Drop => {
                    // Receiver rejection consumes policy draws but reserves no FIFO arrival.
                    match last {
                        Some(last) => {
                            self.faults.links.last.insert((src, dst), last);
                        }
                        None => {
                            self.faults.links.last.remove(&(src, dst));
                        }
                    }
                    self.dropped += 1;
                    self.count_link(t, src, dst, 2);
                    continue;
                }
            }
            self.schedule(
                dst,
                t,
                Payload::Frame {
                    from: src,
                    bytes: frame.clone(),
                },
            );
        }
    }

    /// Carry out a lifecycle transition at world boundary `t`, before any
    /// machine runs at `t`.
    pub fn lifecycle(&mut self, m: usize, op: faults::Lifecycle, t: u64) -> Result<(), String> {
        self.check_active()?;
        if self.machines[m].time > t {
            return Err("lifecycle boundary is in committed machine past".into());
        }
        match op {
            faults::Lifecycle::Pause => {
                self.paused_ingress[m] = 0;
                self.machines[m].pause()
            }
            faults::Lifecycle::Resume => self.machines[m].resume(t),
            faults::Lifecycle::Crash => self.crash(m, t),
            faults::Lifecycle::Reboot => self.reboot(m, t),
        }
    }

    /// Destroy machine m's VM at `t` and build its next incarnation from the
    /// retained boot inputs, ready to boot when it reboots. Management
    /// commands queued for the dead incarnation go with it; frames in world
    /// transit stay queued for the reboot to sort out.
    pub fn crash(&mut self, m: usize, t: u64) -> Result<(), String> {
        self.check_active()?;
        let mut predecessor = None;
        let result = (|| {
            let name = self.machines[m].name.clone();
            let next = self.machines[m]
                .incarnation
                .checked_add(1)
                .ok_or("incarnation overflow")?;
            let mut rng = Rng::branch(
                self.run_seed,
                self.branch_identity.as_deref(),
                &format!("reboot-entropy/{name}/{next}"),
            );
            let mut seed = [0u8; 32];
            for chunk in seed.chunks_mut(8) {
                chunk.copy_from_slice(&rng.next().to_le_bytes());
            }
            // A failed constructor owns cleanup of any partially booted worker.
            let mut fresh = M::boot(&self.factory, &name, &self.boot[m], false, seed)?;
            if let Err(mut error) = fresh.seed_timer_branch(
                self.run_seed,
                self.branch_identity.as_deref(),
                &format!("timer-jitter/{name}/{next}"),
            ) {
                if let Err(cleanup) = fresh.terminate_worker() {
                    error.push_str(&format!("; fresh worker cleanup: {cleanup}"));
                }
                return Err(error);
            }
            self.control.triggers.reset_source(&name);
            let old = predecessor.insert(std::mem::replace(&mut self.machines[m], fresh));
            let witness = old.exit_witness()?;
            let discarded: usize = self.strict_events[m].values().map(Vec::len).sum();
            self.strict_events[m].clear();
            let mut summary = json!({
                "incarnation": old.incarnation, "booted_at_ns": old.booted_at, "crashed_at_ns": t,
                "time_ns": old.time, "branches": old.branches(), "exits": old.exits(),
                "state": format!("{:?}", old.state), "paused": old.paused,
                "discarded_pending_io": old.owed_io().is_some(), "discarded_management_bytes": discarded,
                "witness": witness,
            });
            if let Some(observations) = old.crash_observations().as_object() {
                for (key, value) in observations {
                    summary[key] = value.clone();
                }
            }
            summary["disk_lost_sectors"] = json!(self.machines[m].succeed_branch(
                old,
                t,
                self.run_seed,
                self.branch_identity.as_deref()
            )?);
            self.incarnations[m].push(summary);
            Ok(())
        })();
        if let Err(mut error) = result {
            if let Some(old) = &mut predecessor {
                if let Err(cleanup) = old.terminate_worker() {
                    error.push_str(&format!("; predecessor worker cleanup: {cleanup}"));
                }
            }
            for machine in &mut self.machines {
                if let Err(cleanup) = machine.terminate_worker() {
                    error.push_str(&format!("; {} worker cleanup: {cleanup}", machine.name));
                }
            }
            self.failure = Some(error.clone());
            return Err(error);
        }
        Ok(())
    }

    /// Boot machine m's next incarnation at `t`. Frames that reached it
    /// before `t` (held while it was paused, or arriving while it was down)
    /// belonged to the dead NIC and are dropped; later ones arrive normally.
    pub fn reboot(&mut self, m: usize, t: u64) -> Result<(), String> {
        self.check_active()?;
        while let Some((at, p)) = events::pop_before(&mut self.events[m], t) {
            match p {
                Payload::Frame { from, .. } => {
                    self.dropped += 1;
                    self.count_link(at, from, m, 2);
                }
                Payload::Serial(_) => {}
                Payload::Partition { from, to, on } => {
                    self.partitioned.insert((from, to), on);
                }
            }
        }
        self.machines[m].boot_at(t)
    }
}

// The same machine may legitimately return at an equal-time delivery. Fail
// only if due processing/replanning produces the identical stalled plan twice.
type SlicePlan = (usize, u64, u64, u64, u64, u64, u64, u64, u64, u64);
pub fn stop_progress(
    previous: &mut Option<SlicePlan>,
    plan: SlicePlan,
    advanced: bool,
) -> Result<(), String> {
    if advanced {
        *previous = None;
    } else if previous.as_ref() == Some(&plan) {
        return Err(format!(
            "scheduler made no progress after equal-time replanning: machine={} time={} limit={}",
            plan.0, plan.1, plan.2
        ));
    } else {
        *previous = Some(plan);
    }
    Ok(())
}

/// Faults while the world boots towards its fork point: no plan yet, but
/// management readiness is recorded, so a plan armed at the release finds the
/// services that became ready before it.
pub fn boot_faults(
    names: &[String],
    readiness: &[Value],
    latency: u64,
    links: links::Links,
) -> Result<faults::Faults, String> {
    let mut faults = faults::Faults::with_links(Vec::new(), names.to_vec(), links)?;
    faults.require_readiness();
    replay_readiness(&mut faults, names, readiness, latency)?;
    Ok(faults)
}

pub fn replay_readiness(
    faults: &mut faults::Faults,
    names: &[String],
    readiness: &[Value],
    latency: u64,
) -> Result<(), String> {
    for r in readiness {
        let m = names
            .iter()
            .position(|n| r["machine"] == n.as_str())
            .ok_or("unknown ready machine")?;
        faults.management_ready(m, r["at_ns"].as_u64().ok_or("readiness at_ns")?, latency)?;
    }
    Ok(())
}

/// Every timed part of the run seed's plan (faults, scenario events and the
/// kill schedule) must start at or after `from`, when every machine of a
/// forked world has been released. Earlier times have already passed.
pub fn check_plan_after(cfg: &Value, from: u64) -> Result<(), String> {
    let mut times = Vec::new();
    for (key, field) in [("faults", "at_ns"), ("scenario", "at_ns")] {
        for (i, ev) in cfg[key].as_array().into_iter().flatten().enumerate() {
            times.push((format!("{key}[{i}].{field}"), ev[field].as_u64()));
        }
    }
    if cfg["kill_restart"].is_object() {
        times.push((
            "kill_restart.start_ns".into(),
            cfg["kill_restart"]["start_ns"].as_u64(),
        ));
    }
    for (what, t) in times {
        let t = t.ok_or(format!("{what} required"))?;
        if t < from {
            return Err(format!(
                "{what} {t} is before the fork point {from}; in a forked world it must be at or after the release (boot.fork_at_ns or the launch barrier, plus boot.release_skew_ns)"
            ));
        }
    }
    Ok(())
}

/// Install everything the run seed decides apart from the machines: link
/// jitter, faults, runtime control, scenario events and the kill schedule.
/// Every timed item must fall at or after `from`. Returns the kill schedule
/// for the report.
pub fn arm_seed<M: WorldMachine>(
    world: &mut World<M>,
    cfg: &Value,
    names: &[String],
    seed: u64,
    from: u64,
) -> Result<Vec<Value>, String> {
    world.check_active()?;
    check_plan_after(cfg, from)?;
    let booted = std::mem::take(&mut world.faults.readiness);
    let connections = world.faults.connections.clone();
    world.faults = faults::Faults::with_links(
        cfg["faults"].as_array().cloned().unwrap_or_default(),
        names.to_vec(),
        world.faults.links.clone(),
    )?;
    world.faults.connections = connections;
    world.control =
        control::Control::new(&cfg["control"], names.to_vec(), world.latency, world.early)?;
    world.faults.require_readiness();
    replay_readiness(&mut world.faults, names, &booted, world.latency)?;
    world.faults.links.reseed(seed, names);
    world.run_seed = seed;
    let index = |name: &str| {
        names
            .iter()
            .position(|x| x == name)
            .ok_or(format!("unknown machine {name}"))
    };
    for ev in cfg["scenario"].as_array().cloned().unwrap_or_default() {
        let t = ev["at_ns"].as_u64().ok_or("scenario at_ns")?;
        if let Some(s) = ev["serial"].as_str() {
            let m = index(ev["machine"].as_str().unwrap())?;
            world.schedule(m, t, Payload::Serial(s.as_bytes().to_vec()));
        }
        if let Some(p) = ev["partition"].as_array() {
            let (a, b) = (
                index(p[0].as_str().unwrap())?,
                index(p[1].as_str().unwrap())?,
            );
            let on = ev["on"].as_bool().unwrap_or(true);
            // Partition state is consulted when the sender routes; apply it at the sender.
            world.schedule(a, t, Payload::Partition { from: a, to: b, on });
        }
    }

    // Seeded unclean stop/restart schedule: one member down at a time.
    let mut kills = Vec::new();
    let g = &cfg["kill_restart"];
    if g.is_object() {
        let members: Vec<usize> = g["members"]
            .as_array()
            .ok_or("kill_restart.members")?
            .iter()
            .map(|v| index(v.as_str().unwrap()))
            .collect::<Result<_, _>>()?;
        let span =
            |k: &str| -> (u64, u64) { (g[k][0].as_u64().unwrap(), g[k][1].as_u64().unwrap()) };
        let (gap, down) = (span("gap_ns"), span("down_ns"));
        let end = g["end_ns"].as_u64().unwrap();
        let mut rng = Rng::new(seed, "scenario/kill_restart");
        let mut t = g["start_ns"].as_u64().unwrap() + gap.0 + rng.below(gap.1 - gap.0 + 1);
        while t < end {
            let m = members[rng.below(members.len() as u64) as usize];
            let d = down.0 + rng.below(down.1 - down.0 + 1);
            world.schedule(m, t, Payload::Serial(b"stop\n".to_vec()));
            world.schedule(m, t + d, Payload::Serial(b"start\n".to_vec()));
            kills.push(json!({"member": names[m], "stop_ns": t, "start_ns": t + d}));
            t += d + gap.0 + rng.below(gap.1 - gap.0 + 1);
        }
    }
    Ok(kills)
}

/// Release a world parked at its launch barrier under `seed`. Every machine
/// resumes at the latest barrier time plus a seeded skew, so which service
/// starts first still varies by seed. Each guest reseeds its kernel CRNG from
/// seeded entropy, read at its launch barrier or, after a time fork, taken
/// from the time page; timer jitter and the seed's world plan start here.
/// Returns the kills and, per machine, how its entropy was delivered.
pub fn release<M: WorldMachine>(
    world: &mut World<M>,
    cfg: &Value,
    names: &[String],
    seed: u64,
    skew: u64,
    diagnostic: &mut impl FnMut(&str, u64),
) -> Result<(Vec<Value>, Value), String> {
    world.check_active()?;
    let at = world.machines.iter().map(|m| m.time).max().unwrap_or(0);
    check_plan_after(cfg, at.saturating_add(skew))?;
    let mut reseed = serde_json::Map::new();
    for (i, mach) in world.machines.iter_mut().enumerate() {
        let mut rng = Rng::new(seed, &format!("fork-entropy/{}", names[i]));
        let entropy = std::array::from_fn(|_| rng.next() as u32);
        let delay = Rng::new(seed, &format!("release/{}", names[i])).below(skew.saturating_add(1));
        let how = mach.release(at.saturating_add(delay), entropy)?;
        if how == Reseed::Unsupported {
            diagnostic(&names[i], mach.abi());
        }
        reseed.insert(names[i].clone(), json!(how.as_str()));
        mach.seed_timer(seed, &format!("timer-jitter/{}", names[i]))?;
        if mach.supports_speed_reseed() {
            mach.seed_speed(seed, &format!("pv-speed/{}", names[i]))?;
        }
    }
    Ok((
        arm_seed(world, cfg, names, seed, at.saturating_add(skew))?,
        Value::Object(reseed),
    ))
}

/// Keep the captured plan and all sampled effects. Only future draws change.
pub fn reseed_checkpoint<M: WorldMachine>(
    world: &mut World<M>,
    names: &[String],
    parent_seed: u64,
    seed: u64,
    identity: &str,
) -> Result<Vec<Value>, String> {
    world.check_active()?;
    world.validate()?;
    if names.len() != world.machines.len() {
        return Err("checkpoint machine names differ from world".into());
    }
    M::checkpoint_boundary(&world.clock)?;
    // Branch P continues every captured stream, including guest entropy.
    if seed == parent_seed {
        return Ok(world
            .machines
            .iter()
            .map(|m| json!({"machine":m.name,"reseed":"continuation"}))
            .collect());
    }
    world.run_seed = seed;
    world.branch_identity = Some(identity.to_string());
    world
        .faults
        .links
        .reseed_branch(seed, Some(identity), names);
    let mut reseed = Vec::new();
    for (mach, name) in world.machines.iter_mut().zip(names) {
        mach.seed_timer_branch(
            seed,
            Some(identity),
            &format!("timer-jitter/{name}/{}", mach.incarnation),
        )?;
        let status = if mach.down || matches!(mach.state, State::Exited(_)) {
            "not_up"
        } else {
            let mut rng = Rng::branch(
                seed,
                Some(identity),
                &format!("guest-entropy/{name}/{}", mach.incarnation),
            );
            mach.reseed_guest(std::array::from_fn(|_| rng.next() as u32))?;
            "time_page"
        };
        reseed.push(json!({"machine":name,"incarnation":mach.incarnation,"reseed":status}));
    }
    Ok(reseed)
}

/// A committed world boundary. Runtime forks retain the active plan and RNG
/// positions; boot forks still replace the plan and streams at release.
pub struct WorldSnapshot<M: WorldMachine> {
    boot_snapshot: bool,
    machines: Vec<M::Snapshot>,
    events: Vec<BTreeMap<(u64, u64), Payload>>,
    strict_events: Vec<BTreeMap<(u64, u64), Vec<u8>>>,
    seq: u64,
    partitioned: BTreeMap<(usize, usize), bool>,
    dropped: u64,
    delivered: u64,
    link_counts: BTreeMap<(usize, usize), [u64; 4]>,
    semantic_counts: network_counters::NetworkCounters,
    slices: u64,
    faults: faults::Faults,
    control: control::Control,
    run_seed: u64,
    branch_identity: Option<String>,
    incarnations: Vec<Vec<Value>>,
    paused_ingress: Vec<u64>,
}

impl<M: WorldMachine> WorldSnapshot<M> {
    pub fn take(world: &mut World<M>) -> Result<Self, String> {
        world.check_active()?;
        world.validate()?;
        if world.machines.iter().any(|m| !m.parked) {
            return Err("boot snapshot requires all machines parked".into());
        }
        Self::capture(world, true)
    }

    pub fn take_checkpoint(world: &mut World<M>) -> Result<Self, String> {
        M::checkpoint_boundary(&world.clock)?;
        Self::capture(world, false)
    }

    fn capture(world: &mut World<M>, boot_snapshot: bool) -> Result<Self, String> {
        world.check_active()?;
        world.validate()?;
        if world.machines.iter().any(|m| {
            !m.fault_effects.is_empty()
                || !m.control_requests.is_empty()
                || m.committed_console.is_some()
        }) {
            return Err("checkpoint requires drained machine messages".into());
        }
        Ok(Self {
            boot_snapshot,
            machines: world
                .machines
                .iter_mut()
                .map(M::snapshot)
                .collect::<Result<_, _>>()?,
            events: world.events.clone(),
            strict_events: world.strict_events.clone(),
            seq: world.seq,
            partitioned: world.partitioned.clone(),
            dropped: world.dropped,
            delivered: world.delivered,
            link_counts: world.link_counts.clone(),
            semantic_counts: world.semantic_counts.clone(),
            slices: world.slices,
            faults: world.faults.clone(),
            control: world.control.clone(),
            run_seed: world.run_seed,
            branch_identity: world.branch_identity.clone(),
            incarnations: world.incarnations.clone(),
            paused_ingress: world.paused_ingress.clone(),
        })
    }

    /// Multiple held snapshots cannot share Undo's single dirty-page baseline.
    /// Fresh VMs force full RAM copies and discard stale incarnation/KVM state.
    pub fn restore_checkpoint(&self, world: &mut World<M>) -> Result<u64, String> {
        world.check_active()?;
        world.validate()?;
        if self.machines.len() != world.machines.len() {
            return Err("snapshot machine count differs from world".into());
        }
        M::restore_boundary(&mut world.clock)?;
        for i in 0..world.machines.len() {
            let name = world.machines[i].name.clone();
            let seed = entropy_seed(world.boot_seed, &format!("boot-entropy/{name}"));
            world.machines[i] =
                M::boot(&world.factory, &name, &world.boot[i], world.barrier, seed)?;
        }
        self.restore(world)
    }

    /// Put the world back to the fork point. Returns RAM pages copied back.
    pub fn restore(&self, world: &mut World<M>) -> Result<u64, String> {
        world.check_active()?;
        world.validate()?;
        if self.machines.len() != world.machines.len() {
            return Err("snapshot machine count differs from world".into());
        }
        let mut pages = 0;
        for (i, snapshot) in self.machines.iter().enumerate() {
            // A machine the last seed crashed is a later incarnation in a new
            // VM: rebuild the first one to restore into.
            if world.machines[i].incarnation > 0 {
                let name = world.machines[i].name.clone();
                let seed = entropy_seed(world.boot_seed, &format!("boot-entropy/{name}"));
                world.machines[i] =
                    M::boot(&world.factory, &name, &world.boot[i], world.barrier, seed)?;
            }
            pages += world.machines[i].restore(snapshot)?;
        }
        if self.boot_snapshot {
            world.incarnations.iter_mut().for_each(Vec::clear);
            world.paused_ingress.fill(0);
        } else {
            world.incarnations = self.incarnations.clone();
            world.paused_ingress = self.paused_ingress.clone();
        }
        world.events = self.events.clone();
        world.strict_events = self.strict_events.clone();
        world.seq = self.seq;
        world.partitioned = self.partitioned.clone();
        world.dropped = self.dropped;
        world.delivered = self.delivered;
        world.link_counts = self.link_counts.clone();
        world.semantic_counts = self.semantic_counts.clone();
        world.slices = self.slices;
        if self.boot_snapshot && self.control.triggers.records.is_empty() {
            let names: Vec<_> = world.machines.iter().map(|m| m.name.clone()).collect();
            world.faults = boot_faults(
                &names,
                &self.faults.readiness,
                world.latency,
                self.faults.links.clone(),
            )?;
            world.faults.connections = self.faults.connections.clone();
            world.control = control::Control::new(&Value::Null, names, world.latency, world.early)?;
        } else {
            world.faults = self.faults.clone();
            world.control = self.control.clone();
        }
        world.run_seed = self.run_seed;
        world.branch_identity = self.branch_identity.clone();
        Ok(pages)
    }
}

/// Run the world until it ends, or with `fork` until every machine is parked
/// at its fork point: the guest's launch barrier, or with `Some(Some(t))` its
/// first deterministic point at or after simulated time `t`. Returns the
/// outcome.
pub fn drain_machine_messages<M: WorldMachine>(
    world: &mut World<M>,
    m: usize,
    until_barrier: bool,
    exited: bool,
) -> Result<(), String> {
    if let Some((at, byte)) = world.machines[m].committed_console.take() {
        let max_time = world.machines.iter().map(|m| m.time).max().unwrap_or(0);
        world.control.triggers.committed(
            &world.machines[m].name,
            at,
            byte,
            world.control.latency,
            max_time,
        )?;
    }
    for (id, at, success) in world.machines[m].fault_effects.drain(..) {
        if id == 0 && success {
            if let Err(error) = world.faults.management_ready(m, at, world.control.latency) {
                world.faults.error = Some(error);
            }
        } else {
            world.faults.effect(m, id, at, success);
        }
    }
    if exited {
        for id in world
            .control
            .triggers
            .records
            .iter()
            .filter_map(|r| r.fault_id)
        {
            world.faults.target_exited(id, m, world.machines[m].time);
        }
    }
    if exited {
        if let Some(id) = world.control.pending.as_ref().and_then(|p| p.fault) {
            world.faults.target_exited(id, m, world.machines[m].time);
        }
    }
    for (at, line) in world.machines[m]
        .control_requests
        .drain(..)
        .collect::<Vec<_>>()
    {
        if until_barrier {
            return Err(format!(
                "{}: runtime control request at {at} ns, before the fork point; fork earlier (boot.fork_at_ns)",
                world.machines[m].name
            ));
        }
        let max_time = world.machines.iter().map(|m| m.time).max().unwrap_or(0);
        if let Err(e) = world.control.request(m, at, &line, max_time) {
            world.control.log.push(json!({"status":"rejected","machine":world.machines[m].name,"at_ns":at,"frame":String::from_utf8_lossy(&line),"error":e}));
            world.control.error = Some(e);
        }
    }
    Ok(())
}

pub fn simulate<M: WorldMachine>(
    world: &mut World<M>,
    cfg: &Value,
    duration: u64,
    end_on: &Option<String>,
    fork: Option<Option<u64>>,
    boundary: &mut impl FnMut() -> Result<(), String>,
) -> Result<&'static str, String> {
    simulate_to(world, cfg, duration, end_on, fork, None, boundary)
}

pub fn simulate_to<M: WorldMachine>(
    world: &mut World<M>,
    cfg: &Value,
    duration: u64,
    end_on: &Option<String>,
    fork: Option<Option<u64>>,
    checkpoint: Option<u64>,
    boundary: &mut impl FnMut() -> Result<(), String>,
) -> Result<&'static str, String> {
    world.validate()?;
    if checkpoint.is_some() {
        M::checkpoint_boundary(&world.clock)?;
    }
    let until_barrier = fork.is_some();
    let fork_at = fork.flatten();
    let n = world.machines.len();
    let mut stalled_plan = None;
    let mut frames = Vec::new();
    let outcome;
    loop {
        if checkpoint.is_some() {
            for m in 0..n {
                drain_machine_messages(world, m, until_barrier, false)?;
            }
        }
        boundary()?;
        let keys: Vec<u64> = (0..n).map(|m| world.key(m)).collect();
        let (m, &key) = keys
            .iter()
            .enumerate()
            .min_by_key(|(i, k)| (**k, *i))
            .unwrap();
        // A checkpoint precedes every action at T, including exact-time faults.
        if checkpoint.is_some_and(|t| {
            key >= t
                && world.control.boundary() >= t
                && world.faults.next_boundary() >= t
                && world.control.triggers.boundary() >= t
        }) {
            return Ok("checkpoint");
        }
        if until_barrier
            && world
                .machines
                .iter()
                .all(|m| m.parked || matches!(m.state, State::Exited(_)))
        {
            if let Some(m) = world.machines.iter().find(|m| !m.parked) {
                return Err(format!("{} exited before the fork point", m.name));
            }
            outcome = "barrier";
            break;
        }
        if fork_at.is_some_and(|t| key >= t) && world.machines[m].owed_io().is_none() {
            world.machines[m].park()?;
            continue;
        }
        let control_boundary = world.control.boundary();
        let trigger_boundary = world.control.triggers.boundary();
        world.semantic_counts.advance(
            key.min(control_boundary)
                .min(trigger_boundary)
                .min(world.faults.next_boundary())
                .min(duration),
        );
        if control_boundary <= key
            && control_boundary <= trigger_boundary
            && control_boundary < duration
        {
            if world.machines.iter().any(|m| m.time > control_boundary) {
                return Err("control boundary is in committed machine past".into());
            }
            let exited = world
                .machines
                .iter()
                .filter(|m| matches!(m.state, State::Exited(_)))
                .map(|m| m.name.clone())
                .collect::<Vec<_>>();
            world.control.reject_unavailable(&exited);
            if world.control.pending.as_ref().is_some_and(|p| {
                !p.admitted_done
                    && matches!(p.operation, Some(seedbox_control::Operation::Inspect {}))
            }) {
                world.control.pending.as_mut().unwrap().result = json!({"services":cfg["control"]["placement"],"machines":world.machines.iter().enumerate().map(|(i,m)|json!({"name":m.name,"state":if matches!(m.state,State::Exited(_)) {format!("{:?}",m.state)} else {world.faults.process_state(i).to_owned()}})).collect::<Vec<_>>(),"active_handles":world.faults.active_handles(),"network_counters":world.counter_snapshot(world.semantic_counts.counts())});
                if let Some(evidence) = world.control.triggers.evidence()? {
                    world.control.pending.as_mut().unwrap().result["triggers"] = evidence;
                }
            }
            if let Some(bytes) = world
                .control
                .apply_boundary(&mut world.faults, control_boundary)?
            {
                let target = world.control.controller.unwrap();
                world.seq += 1;
                world.strict_events[target].insert((control_boundary, world.seq), bytes);
            }
            continue;
        }
        if trigger_boundary <= key
            && trigger_boundary < duration
            && trigger_boundary <= world.faults.next_boundary()
        {
            if world.machines.iter().any(|m| m.time > trigger_boundary) {
                return Err("trigger boundary is in committed machine past".into());
            }
            world
                .control
                .triggers
                .apply_boundary(&mut world.faults, trigger_boundary)?;
            continue;
        }
        let fault_boundary = world.faults.next_boundary();
        if fault_boundary <= key && fault_boundary < duration {
            let commands = world.faults.apply_due(fault_boundary);
            for i in world.faults.take_changed_network() {
                let snapshot = world.counter_snapshot(world.semantic_counts.counts());
                world.faults.log[i]["network_counters"] = snapshot;
            }
            for (target, command) in commands {
                if world.machines[target].state == State::Halted && !world.machines[target].paused {
                    world.machines[target].wake(fault_boundary)?;
                }
                world.seq += 1;
                world.strict_events[target].insert((fault_boundary, world.seq), command);
            }
            // After the commands, so a crash at this boundary discards those
            // its machine was just sent.
            for (target, op) in world.faults.take_lifecycle() {
                world.lifecycle(target, op, fault_boundary)?;
            }
            world
                .control
                .observe_faults(&world.faults, fault_boundary)?;
            if world.faults.error.is_some() && world.control.pending.is_none() {
                outcome = "fault_failed";
                break;
            }
            continue;
        }
        if key == u64::MAX {
            // Paused or down machines with nothing left to resume or reboot
            // them sit out the rest of the run.
            outcome = if world.machines.iter().any(|m| m.paused || m.down) {
                "duration"
            } else {
                "quiescent"
            };
            break;
        }
        if key >= duration {
            outcome = "duration";
            break;
        }
        if world.machines[m].state == State::Halted {
            world.machines[m].wake(key)?;
        }
        if let Err(error) = world.deliver_due(m) {
            world.control.error = Some(error);
            outcome = "fault_failed";
            break;
        }
        let horizon = keys
            .iter()
            .enumerate()
            .filter(|(k, _)| *k != m)
            .map(|(_, &k)| k.saturating_add(world.lookahead))
            .min()
            .unwrap_or(u64::MAX);
        let mut limit = horizon
            .min(world.next_event(m))
            .min(duration)
            .min(fault_boundary)
            .min(control_boundary)
            .min(trigger_boundary)
            .min(checkpoint.unwrap_or(u64::MAX));
        if let Some(t) = fork_at {
            // Stop exactly at the fork time, or as soon after it as owed I/O
            // cost allows.
            let owed = world.machines[m].owed_io().unwrap_or(0);
            limit = limit.min(t.max(world.machines[m].time.saturating_add(owed)));
        }
        world.slices += 1;
        // A slice ending at the horizon or an event may stop a little early
        // (at a cheap deterministic point); the run's end is exact.
        let early = if limit == duration
            || checkpoint == Some(limit)
            || fork_at == Some(limit)
            || limit == fault_boundary
            || limit == control_boundary
            || limit == trigger_boundary
            || world.strict_events[m]
                .keys()
                .next()
                .is_some_and(|k| k.0 == limit)
        {
            0
        } else {
            world.early
        };
        let plan = (
            m,
            world.machines[m].time,
            limit,
            horizon,
            world.next_event(m),
            fault_boundary,
            control_boundary,
            world.machines[m].next_deadline(),
            world.machines[m].owed_io().unwrap_or(0),
            world.seq,
        );
        let observe_console = world.control.triggers.observes(&world.machines[m].name);
        let request = SliceRequest {
            hard_limit: limit,
            early_slack: early,
            observe_console,
        };
        world.machines[m].observe_console = observe_console;
        let before = SliceObservation::begin(&world.machines[m], request)?;
        let stop = world.machines[m].advance(&mut world.clock, request, &mut frames)?;
        before.validate(&world.machines[m], request, stop)?;
        stop_progress(
            &mut stalled_plan,
            plan,
            world.machines[m].time != plan.1 || stop != Stop::Reached,
        )?;
        drain_machine_messages(world, m, until_barrier, stop == Stop::Exited)?;
        world
            .control
            .observe_faults(&world.faults, world.machines[m].time)?;
        if (world.control.error.is_some()
            || (world.faults.error.is_some() && world.control.pending.is_none()))
            && !world.control.has_undelivered_reply()
        {
            outcome = "fault_failed";
            break;
        }
        let t = world.machines[m].time;
        for f in frames.drain(..) {
            world.route(m, t, f);
        }
        if stop == Stop::Exited && end_on.as_deref() == Some(world.machines[m].name.as_str()) {
            outcome = "end_machine_exited";
            break;
        }
    }
    if until_barrier && outcome != "barrier" {
        return Err(format!("launch barrier not reached: {outcome}"));
    }
    Ok(outcome)
}

#[cfg(test)]
#[path = "world_tests.rs"]
mod tests;
