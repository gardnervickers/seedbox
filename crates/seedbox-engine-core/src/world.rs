//! One deterministic world coordinator over stopped architecture primitives.
//! MachineState is stored once in the real machine/proxy; it is never refreshed
//! from an independently mutable scheduler mirror.
use crate::network_counters::{
    self, DELIVERED, LOSS_DROPPED, PARTITION_DROPPED, QUEUE_DROPPED, RECEIVER_DROPPED,
};
use crate::report::Kill;
use crate::world_config::{SeedPlan, SeedPlanConfig};
use crate::world_events as events;
use crate::{control, faults, links, rng::Rng};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::ops::{Deref, DerefMut};

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum State {
    Running,
    Halted,
    Exited(u8),
}
/// The text inspection and crash summaries report: `Running`, `Halted` or
/// `Exited(code)`. Report validators parse `Exited(code)` back.
impl std::fmt::Display for State {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Running => f.write_str("Running"),
            Self::Halted => f.write_str("Halted"),
            Self::Exited(code) => write!(f, "Exited({code})"),
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
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
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct MachineState {
    pub name: String,
    pub time: u64,
    pub state: State,
    pub(crate) parked: bool,
    pub(crate) paused: bool,
    pub(crate) down: bool,
    pub(crate) incarnation: u32,
    pub(crate) booted_at: u64,
    pub fault_effects: Vec<(u32, u64, bool)>,
    pub control_requests: Vec<(u64, Vec<u8>)>,
    pub observe_console: bool,
    pub committed_console: Option<(u64, u8)>,
    /// Complete application frames, committed at their terminating transport byte.
    pub committed_output: Vec<(u64, Vec<u8>)>,
    #[serde(with = "crate::observation::empty_checkpoint")]
    pub(crate) observations: crate::observation::Buffer,
}
impl MachineState {
    pub fn is_parked(&self) -> bool {
        self.parked
    }
    pub fn is_paused(&self) -> bool {
        self.paused
    }
    pub fn is_down(&self) -> bool {
        self.down
    }
    pub fn incarnation(&self) -> u32 {
        self.incarnation
    }
    pub fn booted_at(&self) -> u64 {
        self.booted_at
    }
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
            committed_output: Vec::new(),
            observations: crate::observation::Buffer::default(),
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
        self.committed_output.clear();
        self.observations.clear();
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
    /// Restart the stream the guest kernel draws progress jitter from, at a
    /// time fork (`seed_progress_jitter`) or in a checkpoint branch and at a
    /// reboot (`seed_progress_jitter_branch`), next to the timer stream.
    fn seed_progress_jitter(&mut self, seed: u64, domain: &str) -> Result<(), String>;
    fn seed_progress_jitter_branch(
        &mut self,
        seed: u64,
        identity: Option<&str>,
        domain: &str,
    ) -> Result<(), String>;
    /// Whether `throttle` can scale this machine's CPU cost.
    fn supports_throttle(&self) -> bool {
        false
    }
    /// Charge CPU work at `factor_permille / 1000` times the seeded speed;
    /// 1000 restores it. Takes effect from the machine's next slice.
    fn throttle(&mut self, _factor_permille: u64) -> Result<(), String> {
        Err("throttle is unsupported by this backend".into())
    }
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
pub(crate) struct SliceObservation {
    name: String,
    time: u64,
    incarnation: u32,
    booted_at: u64,
    parked: bool,
    paused: bool,
    down: bool,
}
impl SliceObservation {
    pub(crate) fn begin(m: &MachineState, request: SliceRequest) -> Result<Self, String> {
        if m.time > request.hard_limit {
            return Err(format!("{}: slice limit is in committed past", m.name));
        }
        if !m.fault_effects.is_empty()
            || !m.control_requests.is_empty()
            || m.committed_console.is_some()
            || !m.committed_output.is_empty()
            || !m.observations.is_empty()
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
    pub(crate) fn validate(
        &self,
        m: &MachineState,
        request: SliceRequest,
        stop: Stop,
    ) -> Result<(), String> {
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
        if !m.committed_output.is_empty() && !request.observe_console {
            return Err(error("application output outside observed slice"));
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
            m.committed_output.iter().map(|(at, _)| *at),
            self.time,
            m.time,
        ) || !ordered(
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

#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub enum Payload {
    Frame { from: usize, bytes: Vec<u8> },
    Serial(Vec<u8>),
    Partition { from: usize, to: usize, on: bool },
}
pub fn entropy_seed(seed: u64, domain: &str) -> [u8; 32] {
    entropy_seed_branch(seed, None, domain)
}
/// As [`entropy_seed`], on a checkpoint branch's stream when `identity` is set.
pub fn entropy_seed_branch(seed: u64, identity: Option<&str>, domain: &str) -> [u8; 32] {
    let mut rng = Rng::branch(seed, identity, domain);
    let mut out = [0u8; 32];
    for c in out.chunks_mut(8) {
        c.copy_from_slice(&rng.next().to_le_bytes());
    }
    out
}

/// Physical machines and explicit initial policy supplied by a backend.
/// Queue, counter and crash bookkeeping are initialized by the coordinator.
pub struct WorldInputs<M: WorldMachine> {
    pub machines: Vec<M>,
    pub factory: M::Factory,
    pub boot: Vec<M::Boot>,
    pub clock: M::Clock,
    pub barrier: bool,
    pub boot_seed: u64,
    pub run_seed: u64,
    pub lookahead: u64,
    pub early: u64,
    pub latency: u64,
    pub faults: faults::Faults,
    pub control: control::Control,
    pub macs: Vec<[u8; 6]>,
}

pub struct World<M: WorldMachine> {
    /// An interactive capture suspends this exact plan, including queue keys
    /// and source counters. Rebuilding it would renumber in-flight effects.
    pub(crate) scheduler_continuation: Option<crate::window::World<crate::world_windowed::Msg>>,
    pub(crate) observations: Option<std::sync::Arc<crate::observation::Publisher>>,
    /// A failed crash transaction is terminal even if a direct caller retains World.
    pub(crate) failure: Option<String>,
    pub(crate) machines: Vec<M>,
    pub(crate) factory: M::Factory,
    pub(crate) boot: Vec<M::Boot>,
    /// The first incarnation parks at its launch barrier.
    pub(crate) barrier: bool,
    pub(crate) boot_seed: u64,
    /// The seed the world plan was armed with; reboots draw from it.
    pub(crate) run_seed: u64,
    pub(crate) branch_identity: Option<String>,
    /// Per machine, a summary of each incarnation that crashed.
    pub(crate) incarnations: Vec<Vec<Value>>,
    /// Frames queued for each paused machine during its current pause.
    pub(crate) paused_ingress: Vec<u64>,
    pub(crate) clock: M::Clock,
    pub(crate) events: Vec<BTreeMap<(u64, u64), Payload>>,
    pub(crate) seq: u64,
    /// No frame can arrive sooner than this after it is sent: the horizon.
    pub(crate) lookahead: u64,
    /// Slices may end, and events be delivered, up to this long before their
    /// time. Below `lookahead`, so nothing is ever delivered into the past.
    pub(crate) early: u64,
    pub(crate) latency: u64,
    pub(crate) partitioned: BTreeMap<(usize, usize), bool>,
    pub(crate) faults: faults::Faults,
    pub(crate) input_program: crate::input_program::Program,
    pub(crate) control: control::Control,
    pub(crate) strict_events: Vec<BTreeMap<(u64, u64), Vec<u8>>>,
    pub(crate) macs: Vec<[u8; 6]>,
    pub(crate) dropped: u64,
    pub(crate) delivered: u64,
    pub(crate) link_counts: network_counters::LinkCounts,
    pub(crate) semantic_counts: network_counters::NetworkCounters,
    pub(crate) slices: u64,
    pub(crate) boundaries: u64,
}

impl<M: WorldMachine> World<M> {
    /// Create an active world with empty queues and counters. Validation runs
    /// before the caller can execute any machine through the coordinator.
    pub fn new(inputs: WorldInputs<M>) -> Result<Self, String> {
        let WorldInputs {
            machines,
            factory,
            boot,
            clock,
            barrier,
            boot_seed,
            run_seed,
            lookahead,
            early,
            latency,
            faults,
            control,
            macs,
        } = inputs;
        let n = machines.len();
        let world = Self {
            observations: None,
            machines,
            factory,
            boot,
            clock,
            barrier,
            boot_seed,
            run_seed,
            lookahead,
            early,
            latency,
            faults,
            control,
            macs,
            failure: None,
            input_program: Default::default(),
            scheduler_continuation: None,
            branch_identity: None,
            incarnations: vec![Vec::new(); n],
            paused_ingress: vec![0; n],
            events: vec![BTreeMap::new(); n],
            strict_events: vec![BTreeMap::new(); n],
            seq: 0,
            partitioned: BTreeMap::new(),
            dropped: 0,
            delivered: 0,
            link_counts: BTreeMap::new(),
            semantic_counts: network_counters::NetworkCounters::default(),
            slices: 0,
            boundaries: 0,
        };
        world.validate()?;
        Ok(world)
    }

    /// Arm external inputs before execution or at a seed release.
    pub fn arm_input_program(
        &mut self,
        inputs: Vec<crate::input_program::TimedInput>,
        duration_ns: u64,
        from: u64,
    ) -> Result<(), String> {
        self.check_active()?;
        self.input_program = crate::input_program::Program::new(inputs, duration_ns, from)?;
        Ok(())
    }
    pub fn merge_input_program(
        &mut self,
        inputs: Vec<crate::input_program::TimedInput>,
    ) -> Result<(), String> {
        self.check_active()?;
        self.input_program.replace_pending(inputs)
    }
    pub fn input_rejections(&self) -> &[crate::input_program::InputRejection] {
        self.input_program.rejections()
    }
    pub fn input_admissions(&self) -> &[crate::input_program::InputAdmission] {
        self.input_program.admissions()
    }

    /// Attach a runtime-only sink; observations publish after a round commits.
    pub fn with_observations(
        mut self,
        observations: Option<std::sync::Arc<crate::observation::Publisher>>,
    ) -> Self {
        self.observations = observations;
        self
    }

    /// Fixed machine membership; adapters may inspect or update physical state.
    pub fn machines(&self) -> &[M] {
        &self.machines
    }
    pub fn machines_mut(&mut self) -> &mut [M] {
        &mut self.machines
    }

    /// Immutable policy/evidence access for platform-specific report projection.
    pub fn faults(&self) -> &faults::Faults {
        &self.faults
    }
    pub fn control(&self) -> &control::Control {
        &self.control
    }
    pub fn boot_inputs(&self) -> &[M::Boot] {
        &self.boot
    }
    pub fn events(&self) -> &[BTreeMap<(u64, u64), Payload>] {
        &self.events
    }
    pub fn strict_events(&self) -> &[BTreeMap<(u64, u64), Vec<u8>>] {
        &self.strict_events
    }
    pub fn partitions(&self) -> &BTreeMap<(usize, usize), bool> {
        &self.partitioned
    }
    pub fn link_counts(&self) -> &network_counters::LinkCounts {
        &self.link_counts
    }
    pub fn delivered(&self) -> u64 {
        self.delivered
    }
    pub fn dropped(&self) -> u64 {
        self.dropped
    }
    pub fn slices(&self) -> u64 {
        self.slices
    }
    /// Split physical report access from the immutable retired-incarnation evidence.
    pub fn machine_with_history(&mut self, index: usize) -> (&mut M, &[Value]) {
        (&mut self.machines[index], &self.incarnations[index])
    }

    pub fn check_active(&self) -> Result<(), String> {
        match &self.failure {
            Some(reason) => Err(format!("world is terminal after failed mutation: {reason}")),
            None => Ok(()),
        }
    }
    /// Common admission for direct and IPC adapters, before any execution.
    pub fn validate(&self) -> Result<(), String> {
        self.input_program.validate()?;
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

    pub(crate) fn count_link(&mut self, at: u64, from: usize, to: usize, outcome: usize) {
        self.link_counts.entry((from, to)).or_default()[outcome] += 1;
        self.semantic_counts.record(at, from, to, outcome);
    }
    /// Physical link counts, for the report and the world witness.
    pub fn network_snapshot(&self) -> network_counters::Snapshot {
        network_counters::snapshot(|m| &self.machines[m].name, &self.link_counts)
    }
    pub fn counter_snapshot(&self, counts: &network_counters::LinkCounts) -> Value {
        network_counters::snapshot_value(|m| &self.machines[m].name, counts)
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
                        self.count_link(at, from, m, RECEIVER_DROPPED);
                    } else {
                        self.delivered += 1;
                        self.count_link(at, from, m, DELIVERED);
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
                    self.count_link(send_time, src, dst, PARTITION_DROPPED);
                    continue;
                }
                links::Outcome::Lost => {
                    self.dropped += 1;
                    self.count_link(send_time, src, dst, LOSS_DROPPED);
                    continue;
                }
                links::Outcome::QueueDropped => {
                    self.dropped += 1;
                    self.count_link(send_time, src, dst, QUEUE_DROPPED);
                    continue;
                }
                links::Outcome::Deliver(t) => t,
            };
            let mut accepted = false;
            for (t, bytes) in self.faults.deliveries(src, dst, t, &frame) {
                // A frame arriving while its receiver is down meets no NIC; one
                // arriving while it is paused waits, up to a fixed backlog.
                let to = &self.machines[dst];
                match events::arrival(
                    t,
                    self.faults.next_lifecycle(dst),
                    to.down,
                    to.paused,
                    self.paused_ingress[dst],
                    events::PAUSED_INGRESS_FRAMES,
                ) {
                    events::Arrival::Queue => {}
                    events::Arrival::Held => self.paused_ingress[dst] += 1,
                    events::Arrival::Drop => {
                        self.dropped += 1;
                        self.count_link(t, src, dst, RECEIVER_DROPPED);
                        continue;
                    }
                }
                self.schedule(dst, t, Payload::Frame { from: src, bytes });
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
            faults::Lifecycle::Throttle(factor) => self.machines[m].throttle(factor),
        }
    }

    /// Destroy machine m's VM at `t` and build its next incarnation from the
    /// retained boot inputs, ready to boot when it reboots. Management
    /// commands queued for the dead incarnation go with it; frames in world
    /// transit stay queued for the reboot to sort out.
    pub fn crash(&mut self, m: usize, t: u64) -> Result<(), String> {
        self.check_active()?;
        let discarded: usize = self.strict_events[m].values().map(Vec::len).sum();
        self.strict_events[m].clear();
        let result = crash_machine(
            &mut self.machines[m],
            Crash {
                factory: &self.factory,
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
            terminate_workers(&mut self.machines, &mut error);
            self.failure = Some(error.clone());
            return Err(error);
        }
        if let Some(factor) = self.faults.throttle(m) {
            self.machines[m].throttle(factor)?;
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
                    self.count_link(at, from, m, RECEIVER_DROPPED);
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

/// Append a failed worker cleanup to `error`, naming whose worker it was.
pub(crate) fn note_cleanup(error: &mut String, whose: &str, cleanup: Result<(), String>) {
    if let Err(cleanup) = cleanup {
        error.push_str(&format!("; {whose} worker cleanup: {cleanup}"));
    }
}

/// After a failed crash transaction: terminate every machine's worker,
/// appending each cleanup failure to `error`.
pub(crate) fn terminate_workers<'m, M: WorldMachine + 'm>(
    machines: impl IntoIterator<Item = &'m mut M>,
    error: &mut String,
) {
    for machine in machines {
        let cleanup = machine.terminate_worker();
        note_cleanup(error, &machine.name, cleanup);
    }
}

/// One machine as `inspect` reports it: its exit, else its fault-model state.
pub(crate) fn inspect_machine(
    name: &str,
    state: State,
    faults: &faults::Faults,
    m: usize,
) -> Value {
    let state = match state {
        State::Exited(_) => state.to_string(),
        State::Running | State::Halted => faults.process_state(m).to_owned(),
    };
    json!({ "name": name, "state": state })
}

/// The error for a runtime control request made while booting to a fork point.
pub(crate) fn control_before_fork(machine: &str, at: u64) -> String {
    format!(
        "{machine}: runtime control request at {at} ns, before the fork point; fork earlier (boot.fork_at_ns)"
    )
}

/// What [`crash_machine`] needs besides the machine's slot.
pub(crate) struct Crash<'a, M: WorldMachine> {
    pub factory: &'a M::Factory,
    pub boot: &'a M::Boot,
    pub run_seed: u64,
    pub branch_identity: Option<&'a str>,
    pub triggers: &'a mut crate::triggers::Triggers,
    /// Management bytes the caller dropped with the dead incarnation.
    pub discarded: usize,
    /// Where the crashed incarnation's summary goes.
    pub incarnations: &'a mut Vec<Value>,
    /// The crash time.
    pub at: u64,
}

/// Replace `slot` with its next incarnation at `crash.at`, summarizing the
/// one that crashed. On failure the predecessor's worker is terminated; the
/// caller terminates the rest and marks the world.
pub(crate) fn crash_machine<M: WorldMachine>(
    slot: &mut M,
    crash: Crash<'_, M>,
) -> Result<(), String> {
    let Crash {
        factory,
        boot,
        run_seed,
        branch_identity,
        triggers,
        discarded,
        incarnations,
        at: t,
    } = crash;
    let mut predecessor = None;
    let result = (|| {
        let name = slot.name.clone();
        let next = slot
            .incarnation
            .checked_add(1)
            .ok_or("incarnation overflow")?;
        let seed = entropy_seed_branch(
            run_seed,
            branch_identity,
            &format!("reboot-entropy/{name}/{next}"),
        );
        // A failed constructor owns cleanup of any partially booted worker.
        let mut fresh = M::boot(factory, &name, boot, false, seed)?;
        if let Err(mut error) = fresh
            .seed_timer_branch(
                run_seed,
                branch_identity,
                &format!("timer-jitter/{name}/{next}"),
            )
            .and_then(|()| {
                fresh.seed_progress_jitter_branch(
                    run_seed,
                    branch_identity,
                    &format!("progress-jitter/{name}/{next}"),
                )
            })
        {
            note_cleanup(&mut error, "fresh", fresh.terminate_worker());
            return Err(error);
        }
        triggers.reset_source(&name);
        let old = predecessor.insert(std::mem::replace(slot, fresh));
        let witness = old.exit_witness()?;
        let mut summary = json!({
            "incarnation": old.incarnation,
            "booted_at_ns": old.booted_at,
            "crashed_at_ns": t,
            "time_ns": old.time,
            "exits": old.exits(),
            "state": old.state.to_string(),
            "paused": old.paused,
            "discarded_pending_io": old.owed_io().is_some(),
            "discarded_management_bytes": discarded,
            "witness": witness,
        });
        if let Some(observations) = old.crash_observations().as_object() {
            for (key, value) in observations {
                summary[key] = value.clone();
            }
        }
        summary["disk_lost_sectors"] =
            json!(slot.succeed_branch(old, t, run_seed, branch_identity)?);
        incarnations.push(summary);
        Ok(())
    })();
    if let Err(mut error) = result {
        if let Some(old) = &mut predecessor {
            note_cleanup(&mut error, "predecessor", old.terminate_worker());
        }
        return Err(error);
    }
    Ok(())
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

/// [`SeedPlan::check_after`] for a world document held as JSON.
/// The ARM worker checks its raw world document before decoding the plan, so
/// this reads only the timed fields and names any missing one.
pub fn check_plan_after(cfg: &Value, from: u64) -> Result<(), String> {
    let mut times = Vec::new();
    for key in ["faults", "scenario"] {
        for (i, ev) in cfg[key].as_array().into_iter().flatten().enumerate() {
            times.push((format!("{key}[{i}].at_ns"), ev["at_ns"].as_u64()));
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
    plan: SeedPlan<'_>,
    names: &[String],
    seed: u64,
    from: u64,
) -> Result<Vec<Kill>, String> {
    world.check_active()?;
    plan.check_after(from)?;
    world.arm_input_program(plan.input_program.to_vec(), u64::MAX, from)?;
    let booted = std::mem::take(&mut world.faults.readiness);
    let connections = world.faults.connections.clone();
    world.faults = faults::Faults::with_plans(
        plan.faults.to_vec(),
        names.to_vec(),
        world.faults.links.clone(),
    )?;
    world.faults.connections = connections;
    world.control =
        control::Control::from_config(plan.control, names.to_vec(), world.latency, world.early)?;
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
    for ev in plan.scenario {
        let t = ev.at_ns;
        if let Some(s) = &ev.serial {
            let m = index(ev.machine.as_deref().ok_or("scenario machine")?)?;
            world.schedule(m, t, Payload::Serial(s.as_bytes().to_vec()));
        }
        if let Some(p) = &ev.partition {
            let side = |i: usize| p.get(i).ok_or("scenario partition");
            let (a, b) = (index(side(0)?)?, index(side(1)?)?);
            // Partition state is consulted when the sender routes; apply it at the sender.
            world.schedule(
                a,
                t,
                Payload::Partition {
                    from: a,
                    to: b,
                    on: ev.on,
                },
            );
        }
    }

    // Seeded unclean stop/restart schedule: one member down at a time.
    let mut kills = Vec::new();
    if let Some(g) = plan.kill_restart {
        let members: Vec<usize> = g
            .members
            .iter()
            .map(|name| index(name))
            .collect::<Result<_, _>>()?;
        // An inclusive [min, max] range, drawn from as min + below(max - min + 1).
        let span = |k: &str, [min, max]: [u64; 2]| {
            if min <= max {
                Ok((min, max))
            } else {
                Err(format!("kill_restart.{k}"))
            }
        };
        let (gap, down) = (span("gap_ns", g.gap_ns)?, span("down_ns", g.down_ns)?);
        let end = g.end_ns;
        let mut rng = Rng::new(seed, "scenario/kill_restart");
        let mut t = g.start_ns + gap.0 + rng.below(gap.1 - gap.0 + 1);
        while t < end {
            let m = *members
                .get(rng.below(members.len() as u64) as usize)
                .ok_or("kill_restart.members")?;
            let d = down.0 + rng.below(down.1 - down.0 + 1);
            world.schedule(m, t, Payload::Serial(b"stop\n".to_vec()));
            world.schedule(m, t + d, Payload::Serial(b"start\n".to_vec()));
            kills.push(Kill {
                member: names[m].clone(),
                stop_ns: t,
                start_ns: t + d,
            });
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
) -> Result<(Vec<Kill>, Value), String> {
    let plan = SeedPlanConfig::from_value(cfg)?;
    release_plan(world, plan.plan(), names, seed, skew, diagnostic)
}

/// [`release`] with a typed seed plan.
pub fn release_plan<M: WorldMachine>(
    world: &mut World<M>,
    plan: SeedPlan<'_>,
    names: &[String],
    seed: u64,
    skew: u64,
    diagnostic: &mut impl FnMut(&str, u64),
) -> Result<(Vec<Kill>, Value), String> {
    world.check_active()?;
    let at = world.machines.iter().map(|m| m.time).max().unwrap_or(0);
    plan.check_after(at.saturating_add(skew))?;
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
        mach.seed_progress_jitter(seed, &format!("progress-jitter/{}", names[i]))?;
        if mach.supports_speed_reseed() {
            mach.seed_speed(seed, &format!("pv-speed/{}", names[i]))?;
        }
    }
    Ok((
        arm_seed(world, plan, names, seed, at.saturating_add(skew))?,
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
        mach.seed_progress_jitter_branch(
            seed,
            Some(identity),
            &format!("progress-jitter/{name}/{}", mach.incarnation),
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
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(
    deny_unknown_fields,
    bound(
        serialize = "M::Snapshot: serde::Serialize",
        deserialize = "M::Snapshot: serde::Deserialize<'de>"
    )
)]
pub struct WorldSnapshot<M: WorldMachine> {
    version: u32,
    scheduler_continuation: Option<crate::window::Checkpoint<crate::world_windowed::Msg>>,
    boot_snapshot: bool,
    machines: Vec<M::Snapshot>,
    #[serde(with = "crate::checkpoint::ordered_maps")]
    events: Vec<BTreeMap<(u64, u64), Payload>>,
    #[serde(with = "crate::checkpoint::ordered_maps")]
    strict_events: Vec<BTreeMap<(u64, u64), Vec<u8>>>,
    seq: u64,
    #[serde(with = "crate::checkpoint::ordered_map")]
    partitioned: BTreeMap<(usize, usize), bool>,
    dropped: u64,
    delivered: u64,
    #[serde(with = "crate::checkpoint::ordered_map")]
    link_counts: network_counters::LinkCounts,
    semantic_counts: network_counters::NetworkCounters,
    slices: u64,
    boundaries: u64,
    faults: faults::Faults,
    #[serde(default)]
    input_program: crate::input_program::Program,
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
                || !m.committed_output.is_empty()
                || !m.observations.is_empty()
        }) {
            return Err("checkpoint requires drained machine messages".into());
        }
        Ok(Self {
            boot_snapshot,
            version: 1,
            scheduler_continuation: world
                .scheduler_continuation
                .as_ref()
                .map(crate::window::World::checkpoint),
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
            boundaries: world.boundaries,
            faults: world.faults.clone(),
            input_program: world.input_program.clone(),
            control: world.control.clone(),
            run_seed: world.run_seed,
            branch_identity: world.branch_identity.clone(),
            incarnations: world.incarnations.clone(),
            paused_ingress: world.paused_ingress.clone(),
        })
    }

    /// Backend owners persist machine artifacts before publishing coordinator metadata.
    pub fn machine_snapshots(&self) -> &[M::Snapshot] {
        &self.machines
    }

    /// Multiple held snapshots cannot share Undo's single dirty-page baseline.
    /// Fresh VMs force full RAM copies and discard stale incarnation/KVM state.
    pub fn restore_checkpoint(&self, world: &mut World<M>) -> Result<u64, String> {
        self.restore_transaction(world, true)
    }

    /// Put the world back to the fork point. Returns RAM pages copied back.
    pub fn restore(&self, world: &mut World<M>) -> Result<u64, String> {
        self.restore_transaction(world, false)
    }

    fn restore_transaction(&self, world: &mut World<M>, fresh: bool) -> Result<u64, String> {
        world.validate()?;
        if self.version != 1 {
            return Err("unsupported world checkpoint version".into());
        }
        if self.machines.len() != world.machines.len() {
            return Err("snapshot machine count differs from world".into());
        }
        let count = self.machines.len();
        if [
            self.events.len(),
            self.strict_events.len(),
            self.incarnations.len(),
            self.paused_ingress.len(),
        ]
        .into_iter()
        .any(|len| len != count)
        {
            return Err("snapshot per-machine state has inconsistent lengths".into());
        }
        // Reject malformed scheduler metadata before touching any machine.
        let continuation = self
            .scheduler_continuation
            .clone()
            .map(crate::window::World::from_checkpoint)
            .transpose()
            .map_err(|error| error.to_string())?;
        if continuation
            .as_ref()
            .is_some_and(|scheduler| scheduler.machines() != self.machines.len())
        {
            return Err("snapshot scheduler machine count differs from world".into());
        }
        // Once physical restoration starts, failure may leave a mixture of
        // timelines. Never allow a retry or guest entry against that world.
        let result = if fresh {
            self.restore_checkpoint_inner(world)
        } else {
            self.restore_inner(world)
        };
        let pages = result.map_err(|mut error| {
            terminate_workers(&mut world.machines, &mut error);
            world.failure = Some(error.clone());
            error
        })?;
        world.scheduler_continuation = continuation;
        Ok(pages)
    }

    fn restore_checkpoint_inner(&self, world: &mut World<M>) -> Result<u64, String> {
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
        self.restore_inner(world)
    }

    fn restore_inner(&self, world: &mut World<M>) -> Result<u64, String> {
        world.check_active()?;
        world.validate()?;
        if self.machines.len() != world.machines.len() {
            return Err("snapshot machine count differs from world".into());
        }
        if self.boot_snapshot {
            M::restore_boundary(&mut world.clock)?;
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
        world.boundaries = self.boundaries;
        if self.boot_snapshot && self.control.triggers.records.is_empty() {
            let names: Vec<_> = world.machines.iter().map(|m| m.name.clone()).collect();
            world.input_program = Default::default();
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
            world.input_program = self.input_program.clone();
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
    // The legacy test-only scheduler has no live observation contract.
    world.machines[m].observations.clear();
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
    for (at, bytes) in std::mem::take(&mut world.machines[m].committed_output) {
        let max_time = world.machines.iter().map(|m| m.time).max().unwrap_or(0);
        for byte in bytes {
            world.control.triggers.committed(
                &world.machines[m].name,
                at,
                byte,
                world.control.latency,
                max_time,
            )?;
        }
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
        world.faults.machine_exited(m, world.machines[m].time);
    }
    for (at, line) in std::mem::take(&mut world.machines[m].control_requests) {
        if until_barrier {
            return Err(control_before_fork(&world.machines[m].name, at));
        }
        let max_time = world.machines.iter().map(|m| m.time).max().unwrap_or(0);
        if let Err(e) = world.control.request(m, at, &line, max_time) {
            world
                .control
                .reject_request(&world.machines[m].name, at, &line, e);
        }
    }
    Ok(())
}

/// `_cfg` is unused: inspection answers from the control placement. The
/// parameter keeps the arm64 caller's signature.
pub fn simulate<M: WorldMachine>(
    world: &mut World<M>,
    _cfg: &Value,
    duration: u64,
    end_on: &Option<String>,
    fork: Option<Option<u64>>,
    boundary: &mut impl FnMut() -> Result<(), String>,
) -> Result<&'static str, String> {
    simulate_to(world, _cfg, duration, end_on, fork, None, boundary)
}

pub fn simulate_to<M: WorldMachine>(
    world: &mut World<M>,
    _cfg: &Value,
    duration: u64,
    end_on: &Option<String>,
    fork: Option<Option<u64>>,
    checkpoint: Option<u64>,
    boundary: &mut impl FnMut() -> Result<(), String>,
) -> Result<&'static str, String> {
    simulate_stopped(
        world,
        duration,
        end_on,
        SimulationStop {
            fork,
            checkpoint,
            require_barrier: true,
        },
        boundary,
    )
}

/// Boot initial guests to their launch barriers without converting an ordinary
/// duration or end-machine exit into a failed fork. No snapshot is required.
pub fn simulate_launch<M: WorldMachine>(
    world: &mut World<M>,
    _cfg: &Value,
    duration: u64,
    end_on: &Option<String>,
    boundary: &mut impl FnMut() -> Result<(), String>,
) -> Result<&'static str, String> {
    simulate_stopped(
        world,
        duration,
        end_on,
        SimulationStop {
            fork: Some(None),
            checkpoint: None,
            require_barrier: false,
        },
        boundary,
    )
}

struct SimulationStop {
    fork: Option<Option<u64>>,
    checkpoint: Option<u64>,
    require_barrier: bool,
}

fn simulate_stopped<M: WorldMachine>(
    world: &mut World<M>,
    duration: u64,
    end_on: &Option<String>,
    stop: SimulationStop,
    boundary: &mut impl FnMut() -> Result<(), String>,
) -> Result<&'static str, String> {
    let SimulationStop {
        fork,
        checkpoint,
        require_barrier,
    } = stop;
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
            let unthrottleable: Vec<String> = world
                .machines
                .iter()
                .filter(|m| !m.supports_throttle())
                .map(|m| m.name.clone())
                .collect();
            world.control.reject_unthrottleable(&unthrottleable);
            if world.control.wants_inspect() {
                let machines = (world.machines.iter().enumerate())
                    .map(|(i, m)| inspect_machine(&m.name, m.state, &world.faults, i))
                    .collect();
                let counters = world.counter_snapshot(world.semantic_counts.counts());
                (world.control).answer_inspect(machines, &world.faults, counters)?;
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
                world.faults.set_network_counters(i, snapshot);
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
    if require_barrier && until_barrier && outcome != "barrier" {
        return Err(format!("launch barrier not reached: {outcome}"));
    }
    Ok(outcome)
}

#[cfg(test)]
#[path = "world_tests.rs"]
mod tests;

#[cfg(test)]
mod progress_tests {
    use super::*;
    /// A Compose guest reports management readiness while it boots; with a
    /// time fork that is before the release. The boot world accepts it, and
    /// the seed's plan starts with that machine already ready.
    #[test]
    fn readiness_signalled_before_the_fork_carries_over_to_the_seed() {
        let names = vec!["a".to_string(), "b".to_string()];
        let links = links::Links::from_config(0, &names, &json!({"latency_ns":1000})).unwrap();
        let mut boot = boot_faults(&names, &[], 1000, links).unwrap();
        boot.management_ready(1, 700, 1000).unwrap();
        let restored = boot_faults(&names, &boot.readiness, 1000, boot.links.clone()).unwrap();
        assert_eq!(restored.readiness, boot.readiness);
        let mut seed = faults::Faults::new(Vec::new(), names.clone()).unwrap();
        seed.require_readiness();
        replay_readiness(&mut seed, &names, &restored.readiness, 1000).unwrap();
        assert_eq!(seed.readiness, vec![json!({"machine":"b","at_ns":700})]);
        assert!(
            seed.management_ready(1, 2000, 1000).is_err(),
            "b is already ready"
        );
        assert!(seed.management_ready(0, 2000, 1000).is_ok());
    }
    #[test]
    fn equal_time_delivery_can_replan_but_an_unchanged_plan_cannot_loop() {
        let mut previous = None;
        let plan = (0, 1000, 1000, 1000, 1000, 1000, 1000, 2000, 500, 1);
        assert!(stop_progress(&mut previous, plan, false).is_ok());
        let delivered = (0, 1000, 1000, 1000, 2000, 2000, 2000, 2000, 500, 2);
        assert!(stop_progress(&mut previous, delivered, false).is_ok());
        assert!(stop_progress(&mut previous, delivered, false).is_err());
        assert!(stop_progress(&mut previous, delivered, true).is_ok());
        assert_eq!(previous, None);
        assert!(stop_progress(&mut previous, delivered, false).is_ok());
    }
}
