//! Sequential specifications for linearizability checks with the
//! Wing–Gong–Lowe checker (`wgl`). An operation that never returned is
//! `Pending` (it may or may not have taken effect); a definite rejection that
//! had no side effect is `Failed`.
use std::hash::Hash;
use wgl::{SequentialModel, Verdict};

/// A step budget generous enough for Jepsen-sized histories; exhaustion is
/// reported as such, never as a pass.
pub const STEP_BUDGET: u64 = 50_000_000;

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

    /// Print every operation as an `H` line tagged with `key`: its index,
    /// operation, outcome and times.
    pub fn print(&self, key: &str) {
        for (i, (operation, (start, end))) in self
            .history
            .operations()
            .iter()
            .zip(&self.times)
            .enumerate()
        {
            let outcome = match &operation.outcome {
                wgl::OpOutcome::Completed { ret, .. } => format!("{ret:?}"),
                wgl::OpOutcome::Failed { .. } => "failed".into(),
                wgl::OpOutcome::Pending => "pending".into(),
            };
            crate::kit::history_line(&serde_json::json!({
                "key": key, "i": i, "op": format!("{:?}", operation.op),
                "outcome": outcome, "t0": start, "t1": end,
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
