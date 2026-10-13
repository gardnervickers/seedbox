//! Shared fault-handle allocation and execution-fault state, without scheduling.
//! The dispatcher validates operation arguments and resolves targets before
//! reserving a handle. Targets are canonical machine names, so service aliases
//! cannot bypass the per-machine reservation. Legacy planning allocates pN before checking live
//! fault state, so a later state rejection consumes that number. Other fault
//! categories must use this same owner/allocator when their records are added.
use crate::execution::ExecutionConfig;
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
    InactiveHandle,
    Unsettled,
    UnexpectedEffect,
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
}
#[derive(Clone)]
struct Throttle {
    target: String,
    config: ExecutionConfig,
    phase: Phase,
    revision: u64,
}
#[derive(Clone)]
enum Record {
    Throttle(Throttle),
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Transition {
    Apply,
    Heal,
}

/// A proposed execution setting. This is neither a queue entry nor evidence
/// of application. The owner confirms only after the Driver effect succeeds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ExecutionPlan {
    pub(crate) handle: Handle,
    pub(crate) target: String,
    pub(crate) config: ExecutionConfig,
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
        let handle = self.reserve()?;
        // A pending operation reserves its target until admission resolves.
        // This preserves sequential single-active admission with multiple callers.
        if self.records.values().any(|record| match record {
            Record::Throttle(t) => t.target == target,
        }) {
            return Err(Error::AlreadyThrottled);
        }
        self.records.insert(
            handle,
            Record::Throttle(Throttle {
                target: target.clone(),
                config,
                phase: Phase::Pending,
                revision: 0,
            }),
        );
        Ok(ExecutionPlan {
            handle,
            target,
            config,
            transition: Transition::Apply,
            revision: 0,
        })
    }
    /// Healing is a distinct operation on an existing active handle; it does
    /// not allocate a new pN. Nominal cost is the legacy inverse, not a captured
    /// previous rate. No clock, idle time or partially charged work is changed.
    pub(crate) fn heal(&mut self, text: &str) -> Result<ExecutionPlan, Error> {
        self.valid()?;
        let handle = Handle::parse(text).ok_or(Error::InactiveHandle)?;
        let Some(Record::Throttle(t)) = self.records.get_mut(&handle) else {
            return Err(Error::InactiveHandle);
        };
        if t.phase != Phase::Active {
            return Err(Error::Unsettled);
        }
        let revision = t.revision.checked_add(1).ok_or(Error::AttemptsExhausted)?;
        t.revision = revision;
        t.phase = Phase::Healing;
        Ok(ExecutionPlan {
            handle,
            target: t.target.clone(),
            config: ExecutionConfig::default(),
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
        let matches = self
            .records
            .get(&plan.handle)
            .is_some_and(|record| match record {
                Record::Throttle(t) => {
                    t.target == plan.target
                        && t.revision == plan.revision
                        && t.phase == expected_phase
                        && plan.config
                            == match plan.transition {
                                Transition::Apply => t.config,
                                Transition::Heal => ExecutionConfig::default(),
                            }
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
                let Some(Record::Throttle(t)) = self.records.get_mut(&plan.handle) else {
                    unreachable!()
                };
                t.phase = Phase::Active;
            }
            Transition::Heal => {
                self.records.remove(&plan.handle);
            }
        }
        Ok(())
    }
    /// Admission was rejected with a guarantee that no execution setting was
    /// applied. This bookkeeping success is not an operation success response.
    pub(crate) fn rejected(&mut self, plan: &ExecutionPlan) -> Result<(), Error> {
        self.matching(plan)?;
        match plan.transition {
            Transition::Apply => {
                self.records.remove(&plan.handle);
            }
            Transition::Heal => {
                let Some(Record::Throttle(t)) = self.records.get_mut(&plan.handle) else {
                    unreachable!()
                };
                t.phase = Phase::Active;
            }
        }
        Ok(())
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
            .filter_map(|(handle, record)| match record {
                Record::Throttle(t) => (t.phase != Phase::Pending).then(|| handle.to_string()),
            })
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
        assert_eq!(heal.config, ExecutionConfig::default());
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
            (heal.target.as_str(), heal.config.factor_permille()),
            ("a", 1000)
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
            whole_service_copy.heal("p1").unwrap().config,
            ExecutionConfig::default()
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
        let Some(Record::Throttle(record)) = handles.records.get_mut(&Handle(1)) else {
            panic!()
        };
        record.revision = u64::MAX;
        assert_eq!(handles.heal("p1"), Err(Error::AttemptsExhausted));
        let Some(Record::Throttle(record)) = handles.records.get(&Handle(1)) else {
            panic!()
        };
        assert_eq!(record.phase, Phase::Active);
        assert_eq!(record.revision, u64::MAX);
        assert_eq!(handles.active_handles().unwrap().len(), 10);
    }
}
