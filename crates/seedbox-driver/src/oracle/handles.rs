//! Shared fault-handle allocation and execution-fault state, without scheduling.
//! The dispatcher validates operation arguments and resolves targets before
//! reserving a handle. Targets are canonical machine names, so service aliases
//! cannot bypass the per-machine reservation. Planning allocates pN before checking live
//! fault state, so a later state rejection consumes that number. Other fault
//! categories must use this same owner/allocator when their records are added.
use crate::{clock::ClockRate, execution::ExecutionConfig};
use std::{collections::BTreeMap, fmt};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct Handle(u32);
impl fmt::Display for Handle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "p{}", self.0)
    }
}
impl Handle {
    fn parse(text: &str) -> Option<Self> {
        let value = text.strip_prefix('p')?.parse::<u32>().ok()?;
        let handle = Self(value);
        (value != 0 && handle.to_string() == text).then_some(handle)
    }
}

/// Private monotonic record identity, independent of text and guest numbering.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct RecordId(pub(crate) u64);
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Text {
    Allocated(Handle),
    Named(String),
}
impl fmt::Display for Text {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Allocated(n) => n.fmt(f),
            Self::Named(n) => f.write_str(n),
        }
    }
}
impl Text {
    fn matches(&self, text: &str) -> bool {
        match self {
            Self::Allocated(n) => Handle::parse(text) == Some(*n),
            Self::Named(n) => n == text,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Error {
    RecordsExhausted,
    DuplicateActive,
    Poisoned,
    HandlesExhausted,
    AttemptsExhausted,
    AlreadyThrottled,
    AlreadyDrifting,
    InactiveHandle,
    Unsettled,
    UnexpectedEffect,
    /// A realtime step outside the 365-day bound, including i64::MIN.
    StepOutOfRange,
}
#[derive(Clone, Default)]
struct Allocator {
    last: u32,
}
impl Allocator {
    fn reserve(&mut self) -> Result<Handle, Error> {
        let next = self.last.checked_add(1).ok_or(Error::HandlesExhausted)?;
        self.last = next;
        Ok(Handle(next))
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Pending,
    Active,
    Healing,
    /// A realtime step whose guest kernel ended; healing it changes nothing.
    Lost,
    /// A realtime step not yet acknowledged, with its heal queued behind it:
    /// the guest carries out commands in order, so the undo follows the step.
    PendingHealing,
}
impl Phase {
    /// The installing change has not taken effect yet.
    fn unapplied(self) -> bool {
        matches!(self, Self::Pending | Self::PendingHealing)
    }
}
/// The change a handle installed. Execution and drift are Driver-owned
/// dependencies that survive crash and reboot; a realtime step is guest kernel
/// state that ends with the machine's boot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Change {
    Execution(ExecutionConfig),
    Drift(ClockRate),
    Step(i64),
    /// Cut the record's links; network state that outlives every boot.
    Partition,
    /// The inverse of a partition: restore only that handle's links.
    Unpartition,
    Shape(crate::driver::NetworkPolicy, crate::driver::NetworkFaults),
    Unshape,
    Select,
    Unselect,
}
/// What a handle's change acts on: one machine's canonical name, or both
/// directions of every link between two disjoint, sorted sets of names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum HandleTarget {
    Machine(String),
    Links {
        left: Vec<String>,
        right: Vec<String>,
    },
    DirectedLinks {
        from: Vec<String>,
        to: Vec<String>,
    },
    Connection(crate::driver::Connection),
}
impl Change {
    /// Whether this change installs a handle rather than restoring one.
    pub(crate) fn installs(self) -> bool {
        !matches!(self, Self::Unpartition | Self::Unshape | Self::Unselect)
    }
    fn same_kind(self, other: Self) -> bool {
        std::mem::discriminant(&self) == std::mem::discriminant(&other)
    }
}
#[derive(Clone)]
struct Record {
    text: Text,
    target: HandleTarget,
    change: Change,
    phase: Phase,
    revision: u64,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Transition {
    Apply,
    Heal,
}

/// A proposed handle change. This is neither a queue entry nor evidence of
/// application. The owner confirms only after the effect succeeds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ExecutionPlan {
    pub(crate) record_id: RecordId,
    pub(crate) handle: Text,
    pub(crate) target: HandleTarget,
    /// The change to make now: the installed change, or its inverse when healing.
    pub(crate) change: Change,
    /// Healing a lost realtime step: it settles with no guest effect.
    pub(crate) lost: bool,
    transition: Transition,
    revision: u64,
}
impl ExecutionPlan {
    pub(crate) fn healing(&self) -> bool {
        self.transition == Transition::Heal
    }
}

#[derive(Clone, Default)]
pub(crate) struct Handles {
    allocator: Allocator,
    last_record: u64,
    records: BTreeMap<RecordId, Record>,
    poisoned: bool,
}
impl Handles {
    fn valid(&self) -> Result<(), Error> {
        if self.poisoned {
            Err(Error::Poisoned)
        } else {
            Ok(())
        }
    }
    /// Shared reservation seam for all categories; reserving alone activates
    /// nothing. Validation upstream must precede this call.
    pub(crate) fn reserve(&mut self) -> Result<Handle, Error> {
        self.valid()?;
        self.allocator.reserve()
    }
    /// Target membership and configuration are already validated. Reserve the
    /// handle before checking fault state, matching planning/admission.
    pub(crate) fn throttle(
        &mut self,
        target: String,
        config: ExecutionConfig,
    ) -> Result<ExecutionPlan, Error> {
        self.install(HandleTarget::Machine(target), Change::Execution(config))
    }
    /// A machine runs at one rate: drifts do not stack, including pending ones.
    pub(crate) fn drift(
        &mut self,
        target: String,
        rate: ClockRate,
    ) -> Result<ExecutionPlan, Error> {
        self.install(HandleTarget::Machine(target), Change::Drift(rate))
    }
    /// Realtime steps stack; each has its own handle.
    pub(crate) fn step(
        &mut self,
        target: String,
        nanoseconds: i64,
    ) -> Result<ExecutionPlan, Error> {
        // Every entrance (guest call or host action) must keep the 365-day
        // bound, which also excludes i64::MIN, so a heal's inverse exists.
        if nanoseconds.unsigned_abs() > super::MAX_CLOCK_OFFSET_NS {
            return Err(Error::StepOutOfRange);
        }
        self.install(HandleTarget::Machine(target), Change::Step(nanoseconds))
    }
    /// Partitions stack: overlapping links are cut until each handle heals.
    /// Sides are already validated nonempty, disjoint and sorted.
    pub(crate) fn partition(
        &mut self,
        left: Vec<String>,
        right: Vec<String>,
    ) -> Result<ExecutionPlan, Error> {
        self.install(HandleTarget::Links { left, right }, Change::Partition)
    }
    /// Whether `target` already has a drift record in any phase.
    pub(crate) fn drifting(&self, target: &str) -> bool {
        self.exclusive(target, Change::Drift(ClockRate::NOMINAL))
    }
    fn exclusive(&self, target: &str, change: Change) -> bool {
        matches!(change, Change::Execution(_) | Change::Drift(_))
            && self.records.values().any(|record| {
                record.target == HandleTarget::Machine(target.into())
                    && record.change.same_kind(change)
            })
    }
    pub(crate) fn network(
        &mut self,
        target: HandleTarget,
        change: Change,
    ) -> Result<ExecutionPlan, Error> {
        self.install(target, change)
    }
    fn install(&mut self, target: HandleTarget, change: Change) -> Result<ExecutionPlan, Error> {
        self.install_as(target, change, None)
    }
    pub(crate) fn named(
        &mut self,
        target: HandleTarget,
        change: Change,
        name: String,
    ) -> Result<ExecutionPlan, Error> {
        self.check_name(&name)?;
        self.install_as(target, change, Some(name))
    }
    pub(crate) fn check_name(&self, text: &str) -> Result<(), Error> {
        self.valid()?;
        if text.is_empty() {
            return Err(Error::InactiveHandle);
        }
        if self
            .records
            .values()
            .any(|r| !r.phase.unapplied() && r.text.matches(text))
        {
            return Err(Error::DuplicateActive);
        }
        Ok(())
    }
    fn install_as(
        &mut self,
        target: HandleTarget,
        change: Change,
        name: Option<String>,
    ) -> Result<ExecutionPlan, Error> {
        self.valid()?;
        if matches!(change,Change::Step(n) if n.unsigned_abs()>super::MAX_CLOCK_OFFSET_NS) {
            return Err(Error::StepOutOfRange);
        }
        let text = match name {
            Some(text) => Text::Named(text),
            None => Text::Allocated(self.reserve()?),
        };
        if matches!(&target,HandleTarget::Machine(name) if self.exclusive(name,change)) {
            return Err(match change {
                Change::Drift(_) => Error::AlreadyDrifting,
                _ => Error::AlreadyThrottled,
            });
        }
        let next = self
            .last_record
            .checked_add(1)
            .ok_or(Error::RecordsExhausted)?;
        let record_id = RecordId(next);
        self.last_record = next;
        self.records.insert(
            record_id,
            Record {
                text: text.clone(),
                target: target.clone(),
                change,
                phase: Phase::Pending,
                revision: 0,
            },
        );
        Ok(ExecutionPlan {
            record_id,
            handle: text,
            target,
            change,
            lost: false,
            transition: Transition::Apply,
            revision: 0,
        })
    }
    pub(crate) fn collision(&self, plan: &ExecutionPlan) -> bool {
        let text = plan.handle.to_string();
        plan.transition == Transition::Apply
            && self.records.iter().any(|(&id, r)| {
                id != plan.record_id && !r.phase.unapplied() && r.text.matches(&text)
            })
    }
    pub(crate) fn record_highwater(&self) -> u64 {
        self.last_record
    }
    pub(crate) fn unsettled_records(&self) -> impl Iterator<Item = RecordId> + '_ {
        self.records
            .iter()
            .filter(|(_, r)| r.phase.unapplied() || r.phase == Phase::Healing)
            .map(|(&id, _)| id)
    }
    pub(crate) fn retained_network(&self) -> impl Iterator<Item = (String, Change, &HandleTarget)> {
        self.records
            .values()
            .filter(|r| {
                !r.phase.unapplied()
                    && matches!(
                        r.change,
                        Change::Partition | Change::Shape(..) | Change::Select
                    )
            })
            .map(|r| (r.text.to_string(), r.change, &r.target))
    }
    pub(crate) fn active_record(&self, id: RecordId) -> bool {
        self.records
            .get(&id)
            .is_some_and(|r| matches!(r.phase, Phase::Active | Phase::Lost))
    }
    /// Whether a heal of `id` may be admitted: an active handle, or a
    /// realtime step not yet acknowledged, whose undo then follows it.
    pub(crate) fn healable_record(&self, id: RecordId) -> bool {
        self.active_record(id)
            || self
                .records
                .get(&id)
                .is_some_and(|r| r.phase == Phase::Pending && matches!(r.change, Change::Step(_)))
    }
    /// Whether `id` is a step whose heal waits for the step to take effect.
    pub(crate) fn awaiting_start(&self, id: RecordId) -> bool {
        self.records
            .get(&id)
            .is_some_and(|r| r.phase == Phase::PendingHealing)
    }
    /// None only for a step outside its installation bound (a corrupt record).
    fn inverse(change: Change) -> Option<Change> {
        Some(match change {
            // Nominal cost is the inverse, not a captured previous rate.
            Change::Execution(_) => Change::Execution(ExecutionConfig::default()),
            // Nominal rate keeps the offset accrued so far.
            Change::Drift(_) => Change::Drift(ClockRate::NOMINAL),
            Change::Step(nanoseconds) => Change::Step(nanoseconds.checked_neg()?),
            Change::Partition => Change::Unpartition,
            Change::Shape(..) => Change::Unshape,
            Change::Select => Change::Unselect,
            Change::Unpartition | Change::Unshape | Change::Unselect => return None,
        })
    }
    /// Healing is a distinct operation on an existing active handle; it does
    /// not allocate a new pN. No clock, idle time or partially charged work is
    /// changed by healing a throttle.
    pub(crate) fn heal(&mut self, text: &str) -> Result<ExecutionPlan, Error> {
        self.valid()?;
        let id = self
            .records
            .iter()
            .find(|(_, r)| !r.phase.unapplied() && r.text.matches(text))
            .or_else(|| self.records.iter().find(|(_, r)| r.text.matches(text)))
            .map(|(&id, _)| id)
            .ok_or(Error::InactiveHandle)?;
        self.heal_record(id)
    }
    pub(crate) fn heal_record(&mut self, id: RecordId) -> Result<ExecutionPlan, Error> {
        self.valid()?;
        let Some(record) = self.records.get_mut(&id) else {
            return Err(Error::InactiveHandle);
        };
        // A step not yet acknowledged may be healed: the guest carries out
        // commands in order, and the undo is sent once the step lands.
        let pending_step =
            record.phase == Phase::Pending && matches!(record.change, Change::Step(_));
        if !pending_step && !matches!(record.phase, Phase::Active | Phase::Lost) {
            return Err(Error::Unsettled);
        }
        let revision = record
            .revision
            .checked_add(1)
            .ok_or(Error::AttemptsExhausted)?;
        let lost = record.phase == Phase::Lost;
        let change = if lost {
            Change::Step(0)
        } else {
            Self::inverse(record.change).ok_or(Error::StepOutOfRange)?
        };
        record.revision = revision;
        record.phase = if pending_step {
            Phase::PendingHealing
        } else {
            Phase::Healing
        };
        Ok(ExecutionPlan {
            record_id: id,
            handle: record.text.clone(),
            target: record.target.clone(),
            change,
            lost,
            transition: Transition::Heal,
            revision,
        })
    }
    fn matching(&mut self, plan: &ExecutionPlan) -> Result<(), Error> {
        self.valid()?;
        let matches = self.records.get(&plan.record_id).is_some_and(|record| {
            record.target == plan.target
                // Heal attempts revise the record; its one Apply plan is
                // unchanged by a heal queued behind it.
                && match plan.transition {
                    Transition::Apply => record.phase.unapplied(),
                    Transition::Heal => {
                        record.revision == plan.revision
                            && matches!(record.phase, Phase::Healing | Phase::PendingHealing)
                    }
                }
                && Some(plan.change)
                    == match (plan.transition, plan.lost) {
                        (Transition::Apply, _) => Some(record.change),
                        (Transition::Heal, false) => Self::inverse(record.change),
                        (Transition::Heal, true) => Some(Change::Step(0)),
                    }
        });
        if !matches {
            self.poisoned = true;
            return Err(Error::UnexpectedEffect);
        }
        Ok(())
    }
    /// Confirm successful application, distinct from enqueue/plan acceptance.
    pub(crate) fn applied(&mut self, plan: &ExecutionPlan) -> Result<(), Error> {
        self.matching(plan)?;
        let record = self.records.get_mut(&plan.record_id).unwrap();
        match (plan.transition, record.phase) {
            (Transition::Apply, Phase::PendingHealing) => record.phase = Phase::Healing,
            (Transition::Apply, _) => record.phase = Phase::Active,
            // A queued heal is sent only after its step lands.
            (Transition::Heal, Phase::PendingHealing) => {
                self.poisoned = true;
                return Err(Error::UnexpectedEffect);
            }
            (Transition::Heal, _) => {
                self.records.remove(&plan.record_id);
            }
        }
        Ok(())
    }
    /// Admission was rejected with a guarantee that no setting was applied.
    /// This bookkeeping success is not an operation success response.
    pub(crate) fn rejected(&mut self, plan: &ExecutionPlan) -> Result<(), Error> {
        self.matching(plan)?;
        let record = self.records.get_mut(&plan.record_id).unwrap();
        match (plan.transition, record.phase) {
            // The heal queued behind a step settles before the step does.
            (Transition::Apply, Phase::PendingHealing) => {
                self.poisoned = true;
                return Err(Error::UnexpectedEffect);
            }
            (Transition::Apply, _) => {
                self.records.remove(&plan.record_id);
            }
            // A heal that never reached the guest leaves its step pending.
            (Transition::Heal, Phase::PendingHealing) => record.phase = Phase::Pending,
            (Transition::Heal, _) => {
                record.phase = if plan.lost {
                    Phase::Lost
                } else {
                    Phase::Active
                };
            }
        }
        Ok(())
    }
    /// The target's guest kernel ended (crash, reboot or exit). Its settled
    /// realtime steps ended with it; throttles and drifts are Driver-owned and
    /// persist. Pending and healing steps settle through their own faults first.
    pub(crate) fn machine_ended(&mut self, target: &str) {
        for record in self.records.values_mut() {
            if record.target == HandleTarget::Machine(target.into())
                && matches!(record.change, Change::Step(_))
                && record.phase == Phase::Active
            {
                record.phase = Phase::Lost;
            }
        }
    }
    /// The Driver/execution failed without a rollback guarantee. Neither the
    /// prior nor requested configuration is authoritative; continuation is fatal.
    pub(crate) fn execution_failed(&mut self) {
        self.poisoned = true;
    }

    pub(crate) fn active_handles(&self) -> Result<Vec<String>, Error> {
        self.valid()?;
        let mut active: Vec<_> = self
            .records
            .iter()
            .filter(|(_, record)| !record.phase.unapplied())
            .map(|(_, record)| record.text.to_string())
            .collect();
        active.sort(); // Inspection uses lexical handle order.
        Ok(active)
    }
}

#[cfg(test)]
mod named_tests;
#[cfg(test)]
mod tests;

mod persist;
