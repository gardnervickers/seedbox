//! Shared fault-handle allocation and execution-fault state, without scheduling.
//! The dispatcher validates operation arguments and resolves targets before
//! reserving a handle. Targets are canonical machine names, so service aliases
//! cannot bypass the per-machine reservation. Legacy planning allocates pN before checking live
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
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Error {
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
}
/// The change a handle installed. Execution and drift are Driver-owned
/// dependencies that survive crash and reboot; a realtime step is guest kernel
/// state that ends with the machine's boot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Change {
    Execution(ExecutionConfig),
    Drift(ClockRate),
    Step(i64),
}
impl Change {
    fn same_kind(self, other: Self) -> bool {
        std::mem::discriminant(&self) == std::mem::discriminant(&other)
    }
}
#[derive(Clone)]
struct Record {
    target: String,
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
    pub(crate) handle: Handle,
    pub(crate) target: String,
    /// The change to make now: the installed change, or its inverse when healing.
    pub(crate) change: Change,
    /// Healing a lost realtime step: it settles with no guest effect.
    pub(crate) lost: bool,
    transition: Transition,
    revision: u64,
}
#[derive(Clone, Default)]
pub(crate) struct Handles {
    allocator: Allocator,
    records: BTreeMap<Handle, Record>,
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
    /// handle before checking fault state, matching legacy planning/admission.
    pub(crate) fn throttle(
        &mut self,
        target: String,
        config: ExecutionConfig,
    ) -> Result<ExecutionPlan, Error> {
        self.install(target, Change::Execution(config))
    }
    /// A machine runs at one rate: drifts do not stack, including pending ones.
    pub(crate) fn drift(
        &mut self,
        target: String,
        rate: ClockRate,
    ) -> Result<ExecutionPlan, Error> {
        self.install(target, Change::Drift(rate))
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
        self.install(target, Change::Step(nanoseconds))
    }
    /// Whether `target` already has a drift record in any phase.
    pub(crate) fn drifting(&self, target: &str) -> bool {
        self.exclusive(target, Change::Drift(ClockRate::NOMINAL))
    }
    fn exclusive(&self, target: &str, change: Change) -> bool {
        !matches!(change, Change::Step(_))
            && self
                .records
                .values()
                .any(|record| record.target == target && record.change.same_kind(change))
    }
    fn install(&mut self, target: String, change: Change) -> Result<ExecutionPlan, Error> {
        let handle = self.reserve()?;
        // A pending operation reserves its target until admission resolves.
        // This preserves sequential single-active admission with multiple callers.
        if self.exclusive(&target, change) {
            return Err(match change {
                Change::Drift(_) => Error::AlreadyDrifting,
                _ => Error::AlreadyThrottled,
            });
        }
        self.records.insert(
            handle,
            Record {
                target: target.clone(),
                change,
                phase: Phase::Pending,
                revision: 0,
            },
        );
        Ok(ExecutionPlan {
            handle,
            target,
            change,
            lost: false,
            transition: Transition::Apply,
            revision: 0,
        })
    }
    /// None only for a step outside its installation bound (a corrupt record).
    fn inverse(change: Change) -> Option<Change> {
        Some(match change {
            // Nominal cost is the legacy inverse, not a captured previous rate.
            Change::Execution(_) => Change::Execution(ExecutionConfig::default()),
            // Nominal rate keeps the offset accrued so far.
            Change::Drift(_) => Change::Drift(ClockRate::NOMINAL),
            Change::Step(nanoseconds) => Change::Step(nanoseconds.checked_neg()?),
        })
    }
    /// Healing is a distinct operation on an existing active handle; it does
    /// not allocate a new pN. No clock, idle time or partially charged work is
    /// changed by healing a throttle.
    pub(crate) fn heal(&mut self, text: &str) -> Result<ExecutionPlan, Error> {
        self.valid()?;
        let handle = Handle::parse(text).ok_or(Error::InactiveHandle)?;
        let Some(record) = self.records.get_mut(&handle) else {
            return Err(Error::InactiveHandle);
        };
        if !matches!(record.phase, Phase::Active | Phase::Lost) {
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
        record.phase = Phase::Healing;
        Ok(ExecutionPlan {
            handle,
            target: record.target.clone(),
            change,
            lost,
            transition: Transition::Heal,
            revision,
        })
    }
    fn matching(&mut self, plan: &ExecutionPlan) -> Result<(), Error> {
        self.valid()?;
        let expected_phase = match plan.transition {
            Transition::Apply => Phase::Pending,
            Transition::Heal => Phase::Healing,
        };
        let matches = self.records.get(&plan.handle).is_some_and(|record| {
            record.target == plan.target
                && record.revision == plan.revision
                && record.phase == expected_phase
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
        match plan.transition {
            Transition::Apply => {
                self.records.get_mut(&plan.handle).unwrap().phase = Phase::Active;
            }
            Transition::Heal => {
                self.records.remove(&plan.handle);
            }
        }
        Ok(())
    }
    /// Admission was rejected with a guarantee that no setting was applied.
    /// This bookkeeping success is not an operation success response.
    pub(crate) fn rejected(&mut self, plan: &ExecutionPlan) -> Result<(), Error> {
        self.matching(plan)?;
        match plan.transition {
            Transition::Apply => {
                self.records.remove(&plan.handle);
            }
            Transition::Heal => {
                self.records.get_mut(&plan.handle).unwrap().phase = if plan.lost {
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
            if record.target == target
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
            .filter(|(_, record)| record.phase != Phase::Pending)
            .map(|(handle, _)| handle.to_string())
            .collect();
        active.sort(); // Legacy inspection uses lexical handle order.
        Ok(active)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn config(n: u32) -> ExecutionConfig {
        ExecutionConfig::new(n).unwrap()
    }
    #[test]
    fn shared_numbering_and_later_state_rejection_consume_handles() {
        let mut handles = Handles::default();
        assert_eq!(handles.reserve().unwrap().to_string(), "p1");
        let first = handles.throttle("a".into(), config(2000)).unwrap();
        assert_eq!(first.handle.to_string(), "p2");
        assert!(handles.active_handles().unwrap().is_empty());
        assert_eq!(
            handles.throttle("a".into(), config(3000)),
            Err(Error::AlreadyThrottled)
        );
        handles.applied(&first).unwrap();
        assert_eq!(
            handles.throttle("a".into(), config(3000)),
            Err(Error::AlreadyThrottled)
        );
        let independent = handles.throttle("b".into(), config(1001)).unwrap();
        assert_eq!(independent.handle.to_string(), "p5");
        handles.applied(&independent).unwrap();
        assert_eq!(handles.active_handles().unwrap(), ["p2", "p5"]);
    }
    #[test]
    fn checked_exhaustion_never_wraps_or_mutates_prior_state() {
        let mut handles = Handles {
            allocator: Allocator { last: u32::MAX - 1 },
            ..Default::default()
        };
        let last = handles.throttle("a".into(), config(1001)).unwrap();
        assert_eq!(last.handle.to_string(), "p4294967295");
        handles.applied(&last).unwrap();
        assert_eq!(handles.reserve(), Err(Error::HandlesExhausted));
        assert_eq!(handles.allocator.last, u32::MAX);
        assert_eq!(handles.active_handles().unwrap(), ["p4294967295"]);
        let heal = handles.heal("p4294967295").unwrap();
        assert_eq!(heal.change, Change::Execution(ExecutionConfig::default()));
        handles.applied(&heal).unwrap();
        assert!(handles.active_handles().unwrap().is_empty());
    }
    #[test]
    fn heal_removes_only_matching_handle_and_restores_nominal() {
        let mut handles = Handles::default();
        let a = handles.throttle("a".into(), config(4000)).unwrap();
        let b = handles.throttle("b".into(), config(2000)).unwrap();
        assert_eq!(handles.heal("p1"), Err(Error::Unsettled));
        handles.applied(&a).unwrap();
        handles.applied(&b).unwrap();
        for inactive in ["", "p0", "p01", "p3", "unknown"] {
            assert_eq!(handles.heal(inactive), Err(Error::InactiveHandle));
        }
        let heal = handles.heal("p1").unwrap();
        assert_eq!(
            (heal.target.as_str(), heal.change),
            ("a", Change::Execution(ExecutionConfig::default()))
        );
        assert_eq!(handles.heal("p1"), Err(Error::Unsettled));
        handles.applied(&heal).unwrap();
        assert_eq!(handles.active_handles().unwrap(), ["p2"]);
        assert_eq!(handles.heal("p1"), Err(Error::InactiveHandle));
        assert_eq!(handles.reserve().unwrap().to_string(), "p3");
    }
    #[test]
    fn rejected_admission_is_distinct_from_uncertain_execution_failure() {
        let mut handles = Handles::default();
        let rejected = handles.throttle("a".into(), config(2000)).unwrap();
        handles.rejected(&rejected).unwrap();
        assert!(handles.active_handles().unwrap().is_empty());
        let active = handles.throttle("a".into(), config(3000)).unwrap();
        assert_eq!(active.handle.to_string(), "p2");
        handles.applied(&active).unwrap();
        let rejected_heal = handles.heal("p2").unwrap();
        handles.rejected(&rejected_heal).unwrap();
        assert_eq!(handles.active_handles().unwrap(), ["p2"]);
        let uncertain_heal = handles.heal("p2").unwrap();
        handles.execution_failed();
        assert_eq!(handles.applied(&uncertain_heal), Err(Error::Poisoned));
        assert_eq!(handles.rejected(&uncertain_heal), Err(Error::Poisoned));
        assert_eq!(handles.active_handles(), Err(Error::Poisoned));
        assert_eq!(handles.reserve(), Err(Error::Poisoned));
    }
    #[test]
    fn stale_confirmation_cannot_complete_a_later_heal_attempt() {
        let mut handles = Handles::default();
        let active = handles.throttle("a".into(), config(3000)).unwrap();
        handles.applied(&active).unwrap();
        let old = handles.heal("p1").unwrap();
        handles.rejected(&old).unwrap();
        let current = handles.heal("p1").unwrap();
        assert_ne!(old.revision, current.revision);
        assert_eq!(handles.applied(&old), Err(Error::UnexpectedEffect));
        assert_eq!(handles.applied(&current), Err(Error::Poisoned));
    }
    #[test]
    fn service_state_copy_keeps_active_records_independent() {
        // Machine-only resets have no ledger callback: the service owns these
        // records independently of the driver-owned execution configuration. This
        // clone test is not whole-simulation snapshot/native acceptance.
        let mut handles = Handles::default();
        let plan = handles.throttle("a".into(), config(2000)).unwrap();
        handles.applied(&plan).unwrap();
        let mut whole_service_copy = handles.clone();
        let heal = handles.heal("p1").unwrap();
        handles.applied(&heal).unwrap();
        assert!(handles.active_handles().unwrap().is_empty());
        assert_eq!(whole_service_copy.active_handles().unwrap(), ["p1"]);
        assert_eq!(
            whole_service_copy.heal("p1").unwrap().change,
            Change::Execution(ExecutionConfig::default())
        );
    }
    #[test]
    fn inspection_order_and_heal_attempt_exhaustion_preserve_active_state() {
        let mut handles = Handles::default();
        for index in 1..=10 {
            let plan = handles.throttle(format!("m{index}"), config(2000)).unwrap();
            handles.applied(&plan).unwrap();
        }
        assert_eq!(
            handles.active_handles().unwrap(),
            ["p1", "p10", "p2", "p3", "p4", "p5", "p6", "p7", "p8", "p9"]
        );
        let record = handles.records.get_mut(&Handle(1)).unwrap();
        record.revision = u64::MAX;
        assert_eq!(handles.heal("p1"), Err(Error::AttemptsExhausted));
        let record = &handles.records[&Handle(1)];
        assert_eq!(record.phase, Phase::Active);
        assert_eq!(record.revision, u64::MAX);
        assert_eq!(handles.active_handles().unwrap().len(), 10);
    }
    #[test]
    fn drifts_do_not_stack_and_heal_to_nominal_keeping_kind() {
        let rate = |ppb| ClockRate::new(ppb).unwrap();
        let mut handles = Handles::default();
        let throttle = handles.throttle("a".into(), config(2000)).unwrap();
        handles.applied(&throttle).unwrap();
        // Throttle and drift are independent categories on one machine.
        let drift = handles.drift("a".into(), rate(5)).unwrap();
        assert!(handles.drifting("a") && !handles.drifting("b"));
        assert_eq!(
            handles.drift("a".into(), rate(6)),
            Err(Error::AlreadyDrifting)
        );
        handles.applied(&drift).unwrap();
        assert_eq!(
            handles.drift("a".into(), rate(6)),
            Err(Error::AlreadyDrifting)
        );
        // The machine ending leaves a drift active: it is a host dependency.
        handles.machine_ended("a");
        let heal = handles.heal(&drift.handle.to_string()).unwrap();
        assert_eq!(heal.change, Change::Drift(ClockRate::NOMINAL));
        assert!(!heal.lost);
        handles.applied(&heal).unwrap();
        assert!(!handles.drifting("a"));
        assert_eq!(handles.active_handles().unwrap(), ["p1"]);
    }
    #[test]
    fn steps_stack_heal_inversely_and_become_lost_with_their_boot() {
        let mut handles = Handles::default();
        let first = handles.step("a".into(), -7).unwrap();
        let second = handles.step("a".into(), 9).unwrap();
        // A pending step is unaffected by the machine ending: its fault settles.
        handles.machine_ended("a");
        handles.applied(&first).unwrap();
        handles.applied(&second).unwrap();
        let heal = handles.heal("p1").unwrap();
        assert_eq!((heal.change, heal.lost), (Change::Step(7), false));
        handles.rejected(&heal).unwrap();
        handles.machine_ended("a");
        assert_eq!(handles.active_handles().unwrap(), ["p1", "p2"]);
        let heal = handles.heal("p1").unwrap();
        assert_eq!((heal.change, heal.lost), (Change::Step(0), true));
        // A refused lost heal remains lost and healable.
        handles.rejected(&heal).unwrap();
        let heal = handles.heal("p1").unwrap();
        assert!(heal.lost);
        handles.applied(&heal).unwrap();
        assert_eq!(handles.active_handles().unwrap(), ["p2"]);
        assert_eq!(handles.heal("p1"), Err(Error::InactiveHandle));
    }
    #[test]
    fn steps_keep_the_bound_that_makes_their_inverse_representable() {
        let mut handles = Handles::default();
        let bound = super::super::MAX_CLOCK_OFFSET_NS as i64;
        for outside in [i64::MIN, -bound - 1, bound + 1, i64::MAX] {
            assert_eq!(
                handles.step("a".into(), outside),
                Err(Error::StepOutOfRange)
            );
        }
        assert!(handles.active_handles().unwrap().is_empty());
        for edge in [-bound, bound] {
            let plan = handles.step("a".into(), edge).unwrap();
            handles.applied(&plan).unwrap();
            let heal = handles.heal(&plan.handle.to_string()).unwrap();
            assert_eq!(heal.change, Change::Step(-edge));
            handles.applied(&heal).unwrap();
        }
        // A corrupt out-of-bound record cannot produce a wrapped inverse.
        let plan = handles.step("a".into(), 1).unwrap();
        handles.applied(&plan).unwrap();
        handles.records.get_mut(&plan.handle).unwrap().change = Change::Step(i64::MIN);
        assert_eq!(
            handles.heal(&plan.handle.to_string()),
            Err(Error::StepOutOfRange)
        );
        assert_eq!(handles.records[&plan.handle].phase, Phase::Active);
    }
}
