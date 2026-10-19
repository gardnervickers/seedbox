//! Sequential specifications for linearizability checks with the
//! Wing–Gong–Lowe checker (`wgl`). An operation that never returned is
//! `Pending` (it may or may not have taken effect); a definite rejection that
//! had no side effect is `Failed`.
use std::hash::Hash;
use wgl::{SequentialModel, Verdict};

/// A step budget generous enough for Jepsen-sized histories; exhaustion is
/// reported as such, never as a pass.
pub const STEP_BUDGET: u64 = 50_000_000;

/// Memory the search's configuration cache may use per check. The cache
/// holds a bitset over the history plus a model state per configuration,
/// so a long history with many unknown outcomes would otherwise fill a
/// guest's memory long before [`STEP_BUDGET`] ran out.
pub const SEARCH_MEMORY: u64 = 640 << 20;

/// Search steps per cached configuration assumed when bounding memory.
/// Register searches measured 9-12, deciding and exhausting alike; a search
/// that caches faster than this may exceed [`SEARCH_MEMORY`].
pub const STEPS_PER_CONFIGURATION: u64 = 4;

/// The step budget for a history of `operations`: [`STEP_BUDGET`], or less
/// when the configurations that many steps cache would exceed
/// [`SEARCH_MEMORY`], so the search reports budget exhaustion instead of
/// the oracle being killed.
pub fn step_budget(operations: usize) -> u64 {
    let per_configuration = operations as u64 / 8 + 64;
    (SEARCH_MEMORY / per_configuration * STEPS_PER_CONFIGURATION).min(STEP_BUDGET)
}

/// Check one key's register history with [`step_budget`], first recording
/// which key is being checked and how large its history is.
///
/// Failed operations are dropped first. The search already excludes them,
/// but sizes its configurations by the whole history, so a history that is
/// mostly requests never sent (clients retrying a stopped node) would get
/// a budget too small to decide what is left. Evidence indices refer to
/// `operations`.
pub fn check_register<V: Clone + Eq + std::hash::Hash>(
    key: &str,
    operations: &[wgl::Operation<RegisterOp<V>, RegisterRet<V>>],
) -> Verdict {
    let kept: Vec<usize> = (0..operations.len())
        .filter(|&i| !matches!(operations[i].outcome, wgl::OpOutcome::Failed { .. }))
        .collect();
    let checked: Vec<_> = kept.iter().map(|&i| operations[i].clone()).collect();
    let pending = checked
        .iter()
        .filter(|o| matches!(o.outcome, wgl::OpOutcome::Pending))
        .count();
    let budget = step_budget(checked.len());
    crate::kit::evidence(
        "checking",
        serde_json::json!({"key": key, "ops": checked.len(), "failed": operations.len() - checked.len(),
                           "pending": pending, "budget": budget}),
    );
    let original = |e: wgl::Evidence| wgl::Evidence {
        longest_prefix: e.longest_prefix.iter().map(|&i| kept[i]).collect(),
        blocked_on: e.blocked_on.map(|i| kept[i]),
        steps: e.steps,
    };
    match wgl::check_history(&RegisterOf::<V>::default(), &checked, budget) {
        Verdict::Linearizable => Verdict::Linearizable,
        Verdict::NotLinearizable(e) => Verdict::NotLinearizable(original(e)),
        // Many unknown outcomes can make refuting a history far costlier
        // than accepting one; an overwritten read refutes it outright.
        Verdict::BudgetExhausted(e) => match overwritten_read(&checked) {
            Some((read, overwrite)) => {
                crate::kit::evidence(
                    "overwritten-read",
                    serde_json::json!({"key": key, "read": kept[read], "overwrite": kept[overwrite]}),
                );
                Verdict::NotLinearizable(original(wgl::Evidence {
                    blocked_on: Some(read),
                    ..e
                }))
            }
            None => Verdict::BudgetExhausted(original(e)),
        },
    }
}

/// A completed read that no linearization explains because the value it
/// returned was certainly overwritten: every operation that may have
/// written that value finished before a write or successful compare-and-set
/// began, which itself finished before the read began. Returns the read and
/// that overwrite. Cheap, and sound for any history, but it finds only
/// violations of this shape: a value no unknown operation could restore.
fn overwritten_read<V: Eq>(
    operations: &[wgl::Operation<RegisterOp<V>, RegisterRet<V>>],
) -> Option<(usize, usize)> {
    use wgl::OpOutcome::{Completed, Failed, Pending};
    // Operations that certainly changed the register, by invocation, and
    // the earliest completion among those invoked at or after each.
    let mut changes: Vec<(u64, u64, usize)> = operations
        .iter()
        .enumerate()
        .filter_map(|(i, o)| match (&o.op, &o.outcome) {
            (RegisterOp::Write(_), Completed { at, .. })
            | (
                RegisterOp::Cas(..),
                Completed {
                    at,
                    ret: RegisterRet::Cas(true),
                },
            ) => Some((o.invoked_at, *at, i)),
            _ => None,
        })
        .collect();
    changes.sort_unstable();
    let mut earliest = vec![(u64::MAX, usize::MAX); changes.len() + 1];
    for k in (0..changes.len()).rev() {
        earliest[k] = earliest[k + 1].min((changes[k].1, changes[k].2));
    }
    'reads: for (r, read) in operations.iter().enumerate() {
        let (
            RegisterOp::Read,
            Completed {
                at: end,
                ret: RegisterRet::Value(Some(value)),
            },
        ) = (&read.op, &read.outcome)
        else {
            continue;
        };
        // When the last operation that may have written `value` finished.
        let mut latest = 0;
        for o in operations.iter().filter(|o| o.invoked_at < *end) {
            let writes = match &o.op {
                RegisterOp::Write(v) | RegisterOp::Cas(_, v) => v == value,
                RegisterOp::Read => false,
            };
            match &o.outcome {
                _ if !writes => {}
                Pending => continue 'reads,
                Failed { .. }
                | Completed {
                    ret: RegisterRet::Cas(false),
                    ..
                } => {}
                Completed { at, .. } => latest = latest.max(*at),
            }
        }
        let first_after = changes.partition_point(|c| c.0 <= latest);
        let (finished, overwrite) = earliest[first_after];
        if finished < read.invoked_at {
            return Some((r, overwrite));
        }
    }
    None
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum RegisterOp<V> {
    Read,
    Write(V),
    /// Set to the second value if the register holds the first.
    Cas(V, V),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RegisterRet<V> {
    Value(Option<V>),
    Wrote,
    Cas(bool),
}

/// A read/write register with compare-and-set, initially `None`.
pub struct RegisterOf<V>(std::marker::PhantomData<V>);

impl<V> Default for RegisterOf<V> {
    fn default() -> Self {
        Self(std::marker::PhantomData)
    }
}

impl<V: Clone + Eq + Hash> SequentialModel for RegisterOf<V> {
    type Op = RegisterOp<V>;
    type Ret = RegisterRet<V>;
    type State = Option<V>;

    fn initial_state(&self) -> Option<V> {
        None
    }

    fn step(
        &self,
        state: &Option<V>,
        op: &RegisterOp<V>,
        observed: Option<&RegisterRet<V>>,
    ) -> Option<Option<V>> {
        match (op, observed) {
            (RegisterOp::Read, Some(RegisterRet::Value(seen))) => {
                (seen == state).then(|| state.clone())
            }
            (RegisterOp::Read, None) => Some(state.clone()),
            (RegisterOp::Write(value), Some(RegisterRet::Wrote) | None) => {
                Some(Some(value.clone()))
            }
            (RegisterOp::Cas(from, to), observed) => {
                let applies = state.as_ref() == Some(from);
                match observed {
                    Some(RegisterRet::Cas(true)) | None if applies => Some(Some(to.clone())),
                    Some(RegisterRet::Cas(false)) if !applies => Some(state.clone()),
                    _ => None,
                }
            }
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum MutexOp<O> {
    Acquire(O),
    Release(O),
}

/// A mutex: at most one owner at a time; only the owner releases.
pub struct MutexOf<O>(std::marker::PhantomData<O>);

impl<O> Default for MutexOf<O> {
    fn default() -> Self {
        Self(std::marker::PhantomData)
    }
}

impl<O: Clone + Eq + Hash> SequentialModel for MutexOf<O> {
    type Op = MutexOp<O>;
    type Ret = ();
    type State = Option<O>;

    fn initial_state(&self) -> Option<O> {
        None
    }

    fn step(&self, state: &Option<O>, op: &MutexOp<O>, _: Option<&()>) -> Option<Option<O>> {
        match (op, state) {
            (MutexOp::Acquire(owner), None) => Some(Some(owner.clone())),
            (MutexOp::Release(owner), Some(holder)) if holder == owner => Some(None),
            _ => None,
        }
    }
}

/// A FIFO queue; a dequeue of an empty queue returns `None`.
pub struct QueueOf<V>(std::marker::PhantomData<V>);

impl<V> Default for QueueOf<V> {
    fn default() -> Self {
        Self(std::marker::PhantomData)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum QueueOp<V> {
    Enqueue(V),
    Dequeue,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum QueueRet<V> {
    Enqueued,
    Dequeued(Option<V>),
}

impl<V: Clone + Eq + Hash> SequentialModel for QueueOf<V> {
    type Op = QueueOp<V>;
    type Ret = QueueRet<V>;
    type State = Vec<V>;

    fn initial_state(&self) -> Vec<V> {
        Vec::new()
    }

    fn step(
        &self,
        state: &Vec<V>,
        op: &QueueOp<V>,
        observed: Option<&QueueRet<V>>,
    ) -> Option<Vec<V>> {
        match (op, observed) {
            (QueueOp::Enqueue(value), Some(QueueRet::Enqueued) | None) => {
                let mut next = state.clone();
                next.push(value.clone());
                Some(next)
            }
            (QueueOp::Dequeue, Some(QueueRet::Dequeued(seen))) => match (state.first(), seen) {
                (None, None) => Some(Vec::new()),
                (Some(head), Some(value)) if head == value => Some(state[1..].to_vec()),
                _ => None,
            },
            (QueueOp::Dequeue, None) => Some(state.get(1..).map(<[V]>::to_vec).unwrap_or_default()),
            _ => None,
        }
    }
}

/// A `wgl` history that also keeps each operation's invocation and
/// completion times (seconds since the oracle started), so a key that fails
/// the check can be printed against the run's faults.
pub struct Timed<Op, Ret> {
    history: wgl::History<Op, Ret>,
    times: Vec<(f64, Option<f64>)>,
}

impl<Op: Clone, Ret: Clone> Default for Timed<Op, Ret> {
    fn default() -> Self {
        Self {
            history: wgl::History::new(),
            times: Vec::new(),
        }
    }
}

impl<Op: Clone + std::fmt::Debug, Ret: Clone + std::fmt::Debug> Timed<Op, Ret> {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn invoke(&mut self, op: Op) -> wgl::OpId {
        self.times.push((crate::kit::now(), None));
        self.history.invoke(op)
    }

    pub fn complete(&mut self, id: wgl::OpId, ret: Ret) {
        self.times[id.index()].1 = Some(crate::kit::now());
        self.history.complete(id, ret);
    }

    pub fn fail(&mut self, id: wgl::OpId) {
        self.times[id.index()].1 = Some(crate::kit::now());
        self.history.fail(id);
    }

    pub fn operations(&self) -> &[wgl::Operation<Op, Ret>] {
        self.history.operations()
    }

    /// Print every operation the checker sees as an `H` line tagged with
    /// `key`: its index, operation, outcome, times and the checker's stamps.
    /// Failed operations, which constrain nothing, are left out.
    pub fn print(&self, key: &str) {
        for (i, (operation, (start, end))) in self
            .history
            .operations()
            .iter()
            .zip(&self.times)
            .enumerate()
        {
            let (outcome, at) = match &operation.outcome {
                wgl::OpOutcome::Completed { ret, at } => (format!("{ret:?}"), Some(*at)),
                wgl::OpOutcome::Failed { .. } => continue,
                wgl::OpOutcome::Pending => ("pending".into(), None),
            };
            crate::kit::history_line(&serde_json::json!({
                "key": key, "i": i, "op": format!("{:?}", operation.op),
                "outcome": outcome, "t0": start, "t1": end,
                "s0": operation.invoked_at, "s1": at,
            }));
        }
    }
}

/// A verdict as a short word for evidence: linearizable, not-linearizable
/// or budget-exhausted.
pub fn verdict_word(verdict: &Verdict) -> &'static str {
    match verdict {
        Verdict::Linearizable => "linearizable",
        Verdict::NotLinearizable(_) => "not-linearizable",
        Verdict::BudgetExhausted(_) => "budget-exhausted",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wgl::{check_history, History};

    fn verdict<M: SequentialModel>(model: &M, history: &History<M::Op, M::Ret>) -> &'static str {
        verdict_word(&check_history(model, history.operations(), 100_000))
    }

    #[test]
    fn register_checks_skip_failed_operations_and_report_original_indices() {
        // A stale read buried among many requests that were never sent.
        let mut h = History::new();
        let w = h.invoke(RegisterOp::Write(1u8));
        h.complete(w, RegisterRet::Wrote);
        for _ in 0..50_000 {
            let r = h.invoke(RegisterOp::Read);
            h.fail(r);
        }
        let w = h.invoke(RegisterOp::Write(2));
        h.complete(w, RegisterRet::Wrote);
        for _ in 0..50_000 {
            let r = h.invoke(RegisterOp::Cas(2, 3));
            h.fail(r);
        }
        let stale = h.invoke(RegisterOp::Read);
        h.complete(stale, RegisterRet::Value(Some(1)));
        let operations = h.operations();
        assert!(step_budget(operations.len()) < 3 * operations.len() as u64);
        match check_register("k", operations) {
            Verdict::NotLinearizable(e) => {
                assert_eq!(e.blocked_on, Some(stale.index()));
                assert_eq!(e.longest_prefix, vec![0, 50_001]);
            }
            other => panic!("{other:?}"),
        }
        // Without the stale read the same padding passes.
        let passing = &operations[..operations.len() - 1];
        assert!(check_register("k", passing).is_linearizable());
    }

    #[test]
    fn overwritten_reads_refute_histories_too_costly_to_search() {
        // The register is reset to its initial value after a write, among
        // many writes of unknown outcome, none of which writes that value.
        let mut h = History::new();
        let w = h.invoke(RegisterOp::Write(0u32));
        h.complete(w, RegisterRet::Wrote);
        let w = h.invoke(RegisterOp::Write(1));
        h.complete(w, RegisterRet::Wrote);
        for v in 0..40 {
            let _ = h.invoke(RegisterOp::Cas(1 + v % 5, 1 + (v + 1) % 5));
        }
        let r = h.invoke(RegisterOp::Read);
        h.complete(r, RegisterRet::Value(Some(0)));
        assert_eq!(overwritten_read(h.operations()), Some((r.index(), 1)));
        // An unknown write of that value could explain the read...
        let mut explained = History::new();
        let w = explained.invoke(RegisterOp::Write(0u32));
        explained.complete(w, RegisterRet::Wrote);
        let w = explained.invoke(RegisterOp::Write(1));
        explained.complete(w, RegisterRet::Wrote);
        let _ = explained.invoke(RegisterOp::Write(0));
        let r = explained.invoke(RegisterOp::Read);
        explained.complete(r, RegisterRet::Value(Some(0)));
        assert_eq!(overwritten_read(explained.operations()), None);
        // ...as could a read concurrent with the overwrite.
        let mut concurrent = History::new();
        let w = concurrent.invoke(RegisterOp::Write(0u32));
        concurrent.complete(w, RegisterRet::Wrote);
        let w = concurrent.invoke(RegisterOp::Write(1));
        let r = concurrent.invoke(RegisterOp::Read);
        concurrent.complete(w, RegisterRet::Wrote);
        concurrent.complete(r, RegisterRet::Value(Some(0)));
        assert_eq!(overwritten_read(concurrent.operations()), None);
    }

    #[test]
    fn register_reads_must_see_a_linearizable_write() {
        let model = RegisterOf::<u8>::default();
        // Concurrent write and read: the read may see either value.
        let mut h = History::new();
        let w = h.invoke(RegisterOp::Write(1));
        let r = h.invoke(RegisterOp::Read);
        h.complete(r, RegisterRet::Value(Some(1)));
        h.complete(w, RegisterRet::Wrote);
        assert_eq!(verdict(&model, &h), "linearizable");
        // A read after a completed write must see it (stale read).
        let mut h = History::new();
        let w = h.invoke(RegisterOp::Write(1));
        h.complete(w, RegisterRet::Wrote);
        let r = h.invoke(RegisterOp::Read);
        h.complete(r, RegisterRet::Value(None));
        assert_eq!(verdict(&model, &h), "not-linearizable");
        // A write that never returned may have landed...
        let mut h = History::new();
        let _ = h.invoke(RegisterOp::Write(2));
        let r = h.invoke(RegisterOp::Read);
        h.complete(r, RegisterRet::Value(Some(2)));
        assert_eq!(verdict(&model, &h), "linearizable");
        // ...but a failed one did not.
        let mut h = History::new();
        let w = h.invoke(RegisterOp::Write(2));
        h.fail(w);
        let r = h.invoke(RegisterOp::Read);
        h.complete(r, RegisterRet::Value(Some(2)));
        assert_eq!(verdict(&model, &h), "not-linearizable");
    }

    #[test]
    fn compare_and_set_succeeds_once() {
        let model = RegisterOf::<u8>::default();
        let mut h = History::new();
        let w = h.invoke(RegisterOp::Write(0));
        h.complete(w, RegisterRet::Wrote);
        let a = h.invoke(RegisterOp::Cas(0, 1));
        let b = h.invoke(RegisterOp::Cas(0, 2));
        h.complete(a, RegisterRet::Cas(true));
        h.complete(b, RegisterRet::Cas(false));
        assert_eq!(verdict(&model, &h), "linearizable");
        let mut h = History::new();
        let w = h.invoke(RegisterOp::Write(0));
        h.complete(w, RegisterRet::Wrote);
        let a = h.invoke(RegisterOp::Cas(0, 1));
        let b = h.invoke(RegisterOp::Cas(0, 2));
        h.complete(a, RegisterRet::Cas(true));
        h.complete(b, RegisterRet::Cas(true));
        assert_eq!(verdict(&model, &h), "not-linearizable");
    }

    #[test]
    fn compare_and_set_sees_absent_keys_and_pending_writes() {
        let model = RegisterOf::<u8>::default();
        // A key never written holds no value: a CAS from any value fails.
        let mut h = History::new();
        let a = h.invoke(RegisterOp::Cas(0, 1));
        h.complete(a, RegisterRet::Cas(true));
        assert_eq!(verdict(&model, &h), "not-linearizable");
        // A pending write can explain a later successful CAS...
        let mut h = History::new();
        let _ = h.invoke(RegisterOp::Write(3));
        let a = h.invoke(RegisterOp::Cas(3, 4));
        h.complete(a, RegisterRet::Cas(true));
        let r = h.invoke(RegisterOp::Read);
        h.complete(r, RegisterRet::Value(Some(4)));
        assert_eq!(verdict(&model, &h), "linearizable");
        // ...but a read after a completed successful CAS must see its value
        // or a later one (etcd's stale-read shape).
        let mut h = History::new();
        let w = h.invoke(RegisterOp::Write(3));
        h.complete(w, RegisterRet::Wrote);
        let a = h.invoke(RegisterOp::Cas(3, 4));
        h.complete(a, RegisterRet::Cas(true));
        let r = h.invoke(RegisterOp::Read);
        h.complete(r, RegisterRet::Value(Some(3)));
        assert_eq!(verdict(&model, &h), "not-linearizable");
    }

    #[test]
    fn long_histories_get_a_budget_that_fits_in_memory() {
        let budget = step_budget(4_000);
        assert!(budget < step_budget(100) && step_budget(100) <= STEP_BUDGET);
        assert!(budget / STEPS_PER_CONFIGURATION * (4_000 / 8 + 64) <= SEARCH_MEMORY);
        // A wedged server's shape: many distinct unknown writes, then a read
        // of the value before them. The search must stop on its budget.
        let mut h = History::new();
        let w = h.invoke(RegisterOp::Write(0u32));
        h.complete(w, RegisterRet::Wrote);
        for value in 1..=40 {
            let _ = h.invoke(RegisterOp::Write(value));
        }
        let r = h.invoke(RegisterOp::Read);
        h.complete(r, RegisterRet::Value(Some(1000)));
        let verdict = check_history(&RegisterOf::<u32>::default(), h.operations(), 200_000);
        assert!(matches!(
            verdict,
            Verdict::BudgetExhausted(_) | Verdict::NotLinearizable(_)
        ));
    }

    #[test]
    fn mutex_grants_never_overlap() {
        let model = MutexOf::<&str>::default();
        // Sequential holds.
        let mut h = History::new();
        let a = h.invoke(MutexOp::Acquire("c1"));
        h.complete(a, ());
        let _release = h.invoke(MutexOp::Release("c1"));
        let b = h.invoke(MutexOp::Acquire("c2"));
        h.complete(b, ());
        assert_eq!(
            verdict(&model, &h),
            "linearizable",
            "a pending release may land first"
        );
        // c2 is granted while c1 still holds: the lease expired under c1.
        let mut h = History::new();
        let a = h.invoke(MutexOp::Acquire("c1"));
        h.complete(a, ());
        let b = h.invoke(MutexOp::Acquire("c2"));
        h.complete(b, ());
        let _ = h.invoke(MutexOp::Release("c1"));
        assert_eq!(verdict(&model, &h), "not-linearizable");
        // A failed acquire never held the lock.
        let mut h = History::new();
        let a = h.invoke(MutexOp::Acquire("c1"));
        h.fail(a);
        let b = h.invoke(MutexOp::Acquire("c2"));
        h.complete(b, ());
        assert_eq!(verdict(&model, &h), "linearizable");
    }

    #[test]
    fn queue_dequeues_in_enqueue_order() {
        let model = QueueOf::<u8>::default();
        let mut h = History::new();
        let a = h.invoke(QueueOp::Enqueue(1));
        h.complete(a, QueueRet::Enqueued);
        let b = h.invoke(QueueOp::Enqueue(2));
        h.complete(b, QueueRet::Enqueued);
        let d = h.invoke(QueueOp::Dequeue);
        h.complete(d, QueueRet::Dequeued(Some(1)));
        assert_eq!(verdict(&model, &h), "linearizable");
        let mut h = History::new();
        let a = h.invoke(QueueOp::Enqueue(1));
        h.complete(a, QueueRet::Enqueued);
        let b = h.invoke(QueueOp::Enqueue(2));
        h.complete(b, QueueRet::Enqueued);
        let d = h.invoke(QueueOp::Dequeue);
        h.complete(d, QueueRet::Dequeued(Some(2)));
        assert_eq!(verdict(&model, &h), "not-linearizable");
        // Concurrent enqueues may land in either order.
        let mut h = History::new();
        let a = h.invoke(QueueOp::Enqueue(1));
        let b = h.invoke(QueueOp::Enqueue(2));
        h.complete(a, QueueRet::Enqueued);
        h.complete(b, QueueRet::Enqueued);
        let d = h.invoke(QueueOp::Dequeue);
        h.complete(d, QueueRet::Dequeued(Some(2)));
        assert_eq!(verdict(&model, &h), "linearizable");
        // A dequeue of an empty queue after a completed enqueue is a lost write.
        let mut h = History::new();
        let a = h.invoke(QueueOp::Enqueue(1));
        h.complete(a, QueueRet::Enqueued);
        let d = h.invoke(QueueOp::Dequeue);
        h.complete(d, QueueRet::Dequeued(None));
        assert_eq!(verdict(&model, &h), "not-linearizable");
    }
}
