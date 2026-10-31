//! Private control policy. Plans are executed by the owner of the single Driver
//! timeline; returning a plan never confirms its effect. Facts, command IDs and
//! first terminal-transition times must come from that owner, including changes
//! made outside this service. Machine snapshots alone do not snapshot this state.
pub(crate) mod targets;

use super::{
    handles::{Change, ExecutionPlan, HandleTarget, Handles},
    triggers::{TriggerAction, Triggers},
    *,
};
use crate::{
    clock::ClockRate,
    execution::ExecutionConfig,
    pv::{ApplicationCommand, CommandResult, SimulationControlId},
};

/// Retained control-policy lifecycle, independent of physical Machine exit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Vm {
    Up,
    Paused,
    Down,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Process {
    Running,
    Stopped,
    Frozen,
}
#[derive(Clone, Debug)]
pub(crate) struct Facts {
    pub name: String,
    pub boot: u64,
    pub vm: Vm,
    /// Process eligibility: a natural application exit permits another Start.
    /// The observation projection separately retains its actual exit outcome.
    pub process: Process,
    pub physically_ended: bool,
    pub ready: bool,
}
/// Authoritative stopped-state view, not an output-derived mirror. Inspection
/// must supply the complete projection, or explicitly return Unsupported.
/// Lifecycle/process policy follows confirmed control admissions/completions;
/// physical exit is tracked separately. Natural application exit keeps the
/// machine Up and makes its process eligible for Start, without a commanded Stop. Physical exit preflight nevertheless rejects Pause/Resume and all
/// process controls before policy validation. Logical Down excludes this
/// preflight because a crashed Machine may still retain an Exited reason.
/// Down process faults therefore still admit and settle Unavailable regardless
/// of physical state. Crash/Reboot validate retained lifecycle independently;
/// a confirmed physical crash (`ExitReason::Crashed`) makes that lifecycle Down.
#[derive(Clone, Copy, Debug)]
pub(crate) enum CommandOwner {
    External,
    RetiredBoot,
    RetiredApplication,
    Service { fault: u32, action: CommandKind },
}
/// A correlated guest command: an application operation or a realtime step.
/// Both share one per-boot ID space and acknowledge through CommandAcknowledged.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CommandKind {
    Application(ApplicationCommand),
    StepRealtime,
}
pub(crate) trait Adapter {
    /// Draw 64 seeded bits from `target`'s clock-fault stream at admission.
    fn draw(&mut self, _target: &str) -> Result<u64, OperationError> {
        Err(OperationError::unsupported())
    }
    /// Reserve a declared machine once, without building it or drawing entropy.
    fn reserve_spawn(&mut self, _: &str) -> Result<(), OperationError> {
        Err(OperationError::unsupported())
    }
    fn resolve(&self, target: &Target) -> Result<Facts, OperationError>;
    /// Refuse `name` unless the engine running these machines provides
    /// `capability`.
    fn require_capability(
        &self,
        name: &str,
        capability: crate::driver::faults::Capability,
    ) -> Result<(), OperationError> {
        Err(error(
            ErrorCode::Unsupported,
            crate::driver::faults::require_capability(None, name, capability)
                .err()
                .unwrap_or_default(),
        ))
    }
    /// Check indirect heal targets too. SDK adapters retain their unrestricted
    /// authority; the Driver's guest view can exclude its controller.
    fn check_control_target(&self, _target: &Target) -> Result<(), OperationError> {
        Ok(())
    }
    /// Canonical name of an admitted machine with a network interface, in any
    /// lifecycle state. A declared but unspawned machine is TargetUnavailable;
    /// an unknown or unattached machine is InvalidRequest.
    fn network_member(&self, _target: &Target) -> Result<String, OperationError> {
        Err(OperationError::unsupported())
    }
    /// Reserve from the shared boot allocator, excluding even future scheduled
    /// external IDs. Reservation is not machine acceptance: command_owner must
    /// reject a receipt until the owner actually admits this plan.
    fn network_floor(&self) -> Result<u64, OperationError> {
        Err(OperationError::unsupported())
    }
    fn connections(
        &self,
        _from: &str,
        _to: &str,
        _after: u64,
    ) -> Result<Vec<Connection>, OperationError> {
        Err(OperationError::unsupported())
    }
    fn selection_available(&self, _connection: &Connection) -> Result<bool, OperationError> {
        Err(OperationError::unsupported())
    }
    fn selection_counts(
        &self,
        _handle: &str,
    ) -> Result<Option<crate::driver::NetworkSettlement>, OperationError> {
        Err(OperationError::unsupported())
    }
    fn command_id(
        &mut self,
        target: &str,
        boot: u64,
        fault: u32,
        action: CommandKind,
    ) -> Result<NonZeroU32, String>;
    /// Authoritative admitted-command table, including external commands and
    /// retired boot policy. Unknown/current miscorrelation is an error.
    fn command_owner(
        &self,
        target: &str,
        boot: u64,
        id: NonZeroU32,
    ) -> Result<CommandOwner, String>;
    fn inspect(
        &self,
        handles: &[String],
        triggers: &[super::triggers::Record],
    ) -> Result<Value, OperationError>;
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Effect {
    Spawn,
    Pause,
    Resume,
    Crash,
    Reboot,
    /// Latch graceful shutdown of the current boot (host inputs only).
    Shutdown,
    Execution(ExecutionConfig),
    /// Driver-applied rate of the target's time base.
    Drift(ClockRate),
    /// Guest-performed realtime step; zero settles without a guest command.
    StepRealtime(i64),
    Process(ApplicationCommand),
    /// Cut the fault's links (Driver-applied network state).
    Partition,
    /// Restore the links a partition handle cut; never a guest effect.
    Unpartition,
    Shape(crate::driver::NetworkPolicy, NetworkFaults),
    Unshape,
    Select,
    Unselect,
    /// Install a machine-side fault under its handle record's identity.
    Fault {
        record: u64,
        fault: crate::pv::DeviceFault,
    },
    /// Heal the machine-side fault of that handle record.
    Unfault {
        record: u64,
    },
}
/// A validated admission subject before its fault ID is allocated.
enum Subject {
    Network(Scope),
    Machine(Facts),
    Links {
        left: Vec<String>,
        right: Vec<String>,
    },
}
/// What a fault acts on: one machine boot, or links between machines.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Scope {
    DirectedLinks {
        from: Vec<String>,
        to: Vec<String>,
    },
    Connection {
        connection: Connection,
    },
    Machine {
        name: String,
        boot: u64,
    },
    /// Both directions of every left × right pair; bound to no boot.
    Links {
        left: Vec<String>,
        right: Vec<String>,
    },
}
impl Scope {
    pub(crate) fn machine(&self) -> Option<(&str, u64)> {
        match self {
            Self::Machine { name, boot } => Some((name, *boot)),
            Self::Links { .. } | Self::DirectedLinks { .. } | Self::Connection { .. } => None,
        }
    }
    fn is_machine(&self, target: &str, boot: u64) -> bool {
        self.machine() == Some((target, boot))
    }
}
impl Effect {
    /// The effect that makes a handle plan's change on `record`.
    pub(crate) fn of(change: Change, record: super::handles::RecordId) -> Self {
        match change {
            Change::Execution(c) => Self::Execution(c),
            Change::Drift(c) => Self::Drift(c),
            Change::Step(n) => Self::StepRealtime(n),
            Change::Partition => Self::Partition,
            Change::Unpartition => Self::Unpartition,
            Change::Shape(p, f) => Self::Shape(p, f),
            Change::Unshape => Self::Unshape,
            Change::Select => Self::Select,
            Change::Unselect => Self::Unselect,
            Change::Fault(fault) => Self::Fault {
                record: record.0,
                fault,
            },
            Change::Unfault => Self::Unfault { record: record.0 },
        }
    }
}
impl Effect {
    /// The handle change this effect installs or restores; the inverse of
    /// [`Effect::of`].
    pub(crate) fn handle_change(&self) -> Option<Change> {
        Some(match *self {
            Self::Execution(c) => Change::Execution(c),
            Self::Drift(c) => Change::Drift(c),
            Self::StepRealtime(n) => Change::Step(n),
            Self::Partition => Change::Partition,
            Self::Unpartition => Change::Unpartition,
            Self::Shape(p, f) => Change::Shape(p, f),
            Self::Unshape => Change::Unshape,
            Self::Select => Change::Select,
            Self::Unselect => Change::Unselect,
            Self::Fault { fault, .. } => Change::Fault(fault),
            Self::Unfault { .. } => Change::Unfault,
            Self::Spawn
            | Self::Pause
            | Self::Resume
            | Self::Crash
            | Self::Reboot
            | Self::Shutdown
            | Self::Process(_) => return None,
        })
    }
    /// Effects carried out by the guest and completed by its acknowledgment.
    pub(crate) fn guest_command(&self) -> bool {
        self.command_kind().is_some()
    }
    fn command_kind(&self) -> Option<CommandKind> {
        match *self {
            Self::Process(action) => Some(CommandKind::Application(action)),
            Self::StepRealtime(_) => Some(CommandKind::StepRealtime),
            _ => None,
        }
    }
}
/// Terminal observations are handed to the timeline/history owner once. No
/// duplicate unbounded history is kept here. Outcome time is not applied time:
/// only Applied proves a mutation; Unavailable and Failed never do.
pub(crate) use crate::driver::SettlementStatus;
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum FaultAction {
    Effect(Effect),
    Heal,
}
/// Who requested an admitted fault: a guest control call, a matched trigger
/// or a host input identified by its validated key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Requester {
    Call {
        source: String,
        id: SimulationControlId,
    },
    Trigger(u32),
    Input(String),
}
/// A fault request after operation parsing, independent of its requester.
/// Guest operations convert through `from_operation`; host inputs construct
/// one directly, so host-only effects (Shutdown) never appear on the wire.
#[derive(Clone, Debug)]
pub(crate) enum Command {
    Named {
        handle: String,
        command: Box<Command>,
    },
    HealRecord {
        record_id: super::handles::RecordId,
    },
    Shape {
        from: Vec<Target>,
        to: Vec<Target>,
        policy: crate::driver::NetworkPolicy,
        network_faults: NetworkFaults,
    },
    DropConnection {
        connection: Connection,
    },
    /// A disk or scheduler fault inside the target machine, under a handle.
    DeviceFault {
        target: Target,
        fault: crate::pv::DeviceFault,
    },
    Effect {
        target: Target,
        effect: Effect,
    },
    Throttle {
        target: Target,
        config: ExecutionConfig,
    },
    Heal {
        handle: String,
    },
    /// Reserve and later build a declared machine by name.
    Spawn {
        machine: String,
    },
    /// Step the target's guest realtime (`clock_skew`).
    StepRealtime {
        target: Target,
        nanoseconds: Amount,
    },
    /// Set the rate of the target's time base in ppb (`clock_drift`).
    Drift {
        target: Target,
        ppb: Amount,
    },
    /// Cut both directions between every left and right member (`partition`).
    Partition {
        left: Vec<Target>,
        right: Vec<Target>,
    },
}
/// A clock fault value: exact, or drawn uniformly from [-max, max] by the
/// simulation RNG at admission.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Amount {
    Exact(i64),
    Seeded(u64),
}
impl Command {
    /// Exactly one of the exact or seeded value; the schema has bounded both.
    fn amount(exact: Option<i64>, max: Option<u64>) -> Result<Amount, OperationError> {
        match (exact, max) {
            (Some(exact), None) => Ok(Amount::Exact(exact)),
            (None, Some(max)) => Ok(Amount::Seeded(max)),
            _ => Err(OperationError::invalid("clock fault needs one value")),
        }
    }
    fn from_operation(op: Operation) -> Result<Self, OperationError> {
        let effect = |target, effect| Ok(Self::Effect { target, effect });
        match op {
            Operation::Stop { target } => effect(target, Effect::Process(ApplicationCommand::Stop)),
            Operation::Restart { target } => {
                effect(target, Effect::Process(ApplicationCommand::Start))
            }
            Operation::PauseProcess { target } => {
                effect(target, Effect::Process(ApplicationCommand::Pause))
            }
            Operation::ResumeProcess { target } => {
                effect(target, Effect::Process(ApplicationCommand::Resume))
            }
            Operation::Pause { target } => effect(target, Effect::Pause),
            Operation::Resume { target } => effect(target, Effect::Resume),
            Operation::Crash { target } => effect(target, Effect::Crash),
            Operation::Reboot { target } => effect(target, Effect::Reboot),
            Operation::Throttle {
                target,
                factor_permille,
            } => Ok(Self::Throttle {
                target,
                config: u32::try_from(factor_permille)
                    .ok()
                    .and_then(|factor| ExecutionConfig::new(factor).ok())
                    .ok_or_else(|| OperationError::invalid("invalid execution factor"))?,
            }),
            Operation::Heal { handle } => Ok(Self::Heal { handle }),
            Operation::Spawn { machine } => Ok(Self::Spawn { machine }),
            Operation::ClockSkew {
                target,
                offset_ns,
                max_offset_ns,
            } => Ok(Self::StepRealtime {
                target,
                nanoseconds: Self::amount(offset_ns, max_offset_ns)?,
            }),
            Operation::ClockDrift {
                target,
                rate_ppb,
                max_rate_ppb,
            } => Ok(Self::Drift {
                target,
                ppb: Self::amount(rate_ppb, max_rate_ppb)?,
            }),
            Operation::Partition { left, right } => Ok(Self::Partition { left, right }),
            Operation::Shape {
                from,
                to,
                latency_ns,
                jitter_ns,
                loss_ppm,
                bandwidth_bps,
                queue_bound_ns,
                network_faults,
            } => Ok(Self::Shape {
                from,
                to,
                policy: crate::driver::NetworkPolicy {
                    latency_ns,
                    jitter_ns,
                    loss_ppm,
                    bandwidth_bps,
                    queue_bound_ns,
                },
                network_faults,
            }),
            Operation::DropConnection { connection } => Ok(Self::DropConnection { connection }),
            _ => Err(OperationError::unsupported()),
        }
    }
}
/// Policy admission of a fault. Admission is not completion; a later
/// Settlement reports the outcome.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Admitted {
    pub record_id: Option<super::handles::RecordId>,
    pub fault: u32,
    pub requester: Requester,
    pub scope: Scope,
    pub action: FaultAction,
    pub handle: Option<String>,
    pub requested_at: u64,
    pub admitted_at: u64,
}
/// Incremental control facts in the order this service confirmed them. The
/// timeline owner takes them after every call, including failed calls, and
/// publishes them; this service keeps no history of drained notices.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Notice {
    Admitted(Admitted),
    Settled(Settlement),
    TriggerArmed {
        trigger: u32,
        source: String,
        /// The Stop target; None for a partition, named by its admission.
        target: Option<String>,
        at: u64,
        deadline: u64,
    },
    /// `status` is a final trigger status, never Armed or Matched.
    TriggerResolved {
        trigger: u32,
        at: u64,
        status: super::triggers::Status,
        fault: Option<u32>,
    },
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Settlement {
    pub record_id: Option<super::handles::RecordId>,
    pub network: Option<crate::driver::NetworkSettlement>,
    /// What a healed device fault matched and did, over every boot.
    pub counts: Option<crate::pv::FaultCounts>,
    pub fault: u32,
    pub requester: Requester,
    pub scope: Scope,
    pub action: FaultAction,
    pub requested_at: u64,
    pub admitted_at: u64,
    pub outcome_at: Option<u64>,
    pub committed_at: u64,
    pub status: SettlementStatus,
    pub handle: Option<String>,
    pub command: Option<NonZeroU32>,
    pub detail: String,
}
/// Fatal paths must return already-known settlements as well as the error.
/// The caller publishes these immutable facts and terminates; no rollback or
/// success reply is fabricated after an uncertain effect failure.
#[derive(Debug)]
pub(crate) struct Failure {
    pub message: String,
    pub settlements: Vec<Settlement>,
}
impl From<String> for Failure {
    fn from(message: String) -> Self {
        Self {
            message,
            settlements: vec![],
        }
    }
}
impl From<&str> for Failure {
    fn from(message: &str) -> Self {
        message.to_owned().into()
    }
}
impl Failure {
    fn retain_before(&mut self, plans: Vec<Plan>) {
        let mut earlier: Vec<_> = plans
            .into_iter()
            .filter_map(|p| {
                if let Plan::Settled(s) = p {
                    Some(*s)
                } else {
                    None
                }
            })
            .collect();
        earlier.append(&mut self.settlements);
        self.settlements = earlier;
    }
}
/// Admission policy is independent of whether work has an RPC reply destination.
/// Ordinary controls apply the API availability preflight; bare trigger faults
/// use the underlying fault admission path and can settle Unavailable.
#[derive(Clone, Copy)]
enum AdmissionMode {
    Control,
    Trigger,
}

type HandleAdmission = (Subject, Effect, Option<ExecutionPlan>, bool);
enum PreparationError {
    Rejected(OperationError),
    Fatal(Failure),
}
impl From<OperationError> for PreparationError {
    fn from(e: OperationError) -> Self {
        Self::Rejected(e)
    }
}
impl From<Failure> for PreparationError {
    fn from(e: Failure) -> Self {
        Self::Fatal(e)
    }
}
#[derive(Clone, Debug)]
pub(crate) enum Plan {
    // Immediate receipts never enter the timeline; box only this private arm
    // instead of inflating every queued control/input with optional counts.
    Settled(Box<Settlement>),
    Effect {
        fault: u32,
        scope: Scope,
        at: u64,
        effect: Effect,
        command: Option<NonZeroU32>,
        /// The handle a partition installs or a heal restores.
        handle: Option<String>,
    },
    /// Owner schedules this token on its timeline, even if the caller disappears.
    Wake {
        token: u32,
        at: u64,
    },
    Deadline {
        fault: u32,
        at: u64,
    },
    Reply {
        boot: Option<u64>,
        source: String,
        id: SimulationControlId,
        at: u64,
        payload: Vec<u8>,
    },
    /// Ask `source`'s machine, now, to hold its `nth` disk request of kind
    /// `op` for `block_request` trigger `trigger`, if that request arrives
    /// before the match deadline `until`. The owner sends it at once; it is
    /// never queued.
    Hold {
        source: String,
        trigger: u32,
        op: seedbox_engine_protocol::control::DiskOp,
        nth: u64,
        until: u64,
    },
    /// Ask `source`'s machine, now, to stop counting for `trigger`'s hold.
    Release {
        source: String,
        trigger: u32,
    },
}
#[derive(Clone)]
struct Call {
    boot: Option<u64>,
    source: String,
    id: SimulationControlId,
    requested: u64,
    admitted: u64,
}
#[derive(Clone)]
struct Pending {
    network: Option<crate::driver::NetworkSettlement>,
    call: Option<Call>,
    requester: Requester,
    scope: Scope,
    effect: Effect,
    command: Option<NonZeroU32>,
    dispatched: Option<u64>,
    deadline: u64,
    admitted: u64,
    handle: Option<ExecutionPlan>,
    result: Value,
    healing: bool,
    /// Settlement detail of an effect applied without a guest command.
    note: Option<&'static str>,
}
#[derive(Clone, Default)]
pub(crate) struct Service {
    pub(crate) reply_latency_ns: u64,
    handles: Handles,
    triggers: Triggers,
    last: u32,
    last_wake: u32,
    pending: BTreeMap<u32, Pending>,
    sleepers: BTreeMap<u32, (Call, u64)>,
    poisoned: bool,
    notices: Vec<Notice>,
}
fn error(code: ErrorCode, message: impl Into<String>) -> OperationError {
    OperationError {
        code,
        message: message.into(),
        handle: None,
    }
}
impl Service {
    /// Transfer confirmed facts once, in confirmation order.
    pub(crate) fn take_notices(&mut self) -> Vec<Notice> {
        std::mem::take(&mut self.notices)
    }
    /// Describe a pending fault as it was admitted, for a pre-effect hold.
    pub(crate) fn pending_admitted(&self, fault: u32) -> Option<Admitted> {
        let p = self.pending.get(&fault)?;
        Some(Admitted {
            record_id: p.handle.as_ref().map(|h| h.record_id),
            fault,
            requester: p.requester.clone(),
            scope: p.scope.clone(),
            action: Self::action(p),
            handle: Self::handle(p),
            requested_at: Self::requested_at(p),
            admitted_at: p.admitted,
        })
    }
    fn valid(&self) -> Result<(), Failure> {
        if self.poisoned {
            Err("control service poisoned".into())
        } else {
            Ok(())
        }
    }
    fn token(&mut self) -> Result<u32, Failure> {
        self.last = self
            .last
            .checked_add(1)
            .ok_or("control operation IDs exhausted")?;
        Ok(self.last)
    }
    fn reply(
        &self,
        call: Call,
        now: u64,
        effect: Option<u64>,
        result: Result<Value, OperationError>,
    ) -> Result<Plan, Failure> {
        if now < call.admitted {
            return Err("reply precedes admission".into());
        }
        let reply_base = if self.reply_latency_ns == 0 {
            now
        } else {
            effect.unwrap_or(now)
        };
        let replied = reply_base
            .checked_add(self.reply_latency_ns)
            .ok_or("control reply time overflow")?;
        let payload = encode(
            call.id.request,
            ReplyMetadata {
                requested_at_ns: call.requested,
                admitted_at_ns: call.admitted,
                effect_at_ns: effect,
                replied_at_ns: replied,
            },
            result.as_ref().map_err(Clone::clone),
        )
        .map_err(|e| format!("reply encoding: {e:?}"))?;
        Ok(Plan::Reply {
            boot: call.boot,
            source: call.source,
            id: call.id,
            at: replied,
            payload,
        })
    }
    pub(crate) fn input_record_active(&self, id: super::handles::RecordId) -> bool {
        self.handles.active_record(id)
    }
    pub(crate) fn handle_collision(&self, fault: u32) -> bool {
        self.pending
            .get(&fault)
            .and_then(|p| p.handle.as_ref())
            .is_some_and(|p| self.handles.collision(p))
    }
}

mod call;
mod install;
mod prepare;
mod settle;
mod trigger;

#[cfg(test)]
mod tests;

mod persist;
