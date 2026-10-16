//! History checkers for guest oracles. An oracle records each operation's
//! invocation and response at its client API, in one logical order, and
//! checks the history against a sequential model before reporting a
//! property. An operation that timed out has an unknown outcome: it may have
//! taken effect at any point after its invocation, or never. A checker that
//! runs out of work says so instead of guessing.
use std::collections::{BTreeMap, HashSet};
use std::sync::Mutex;

/// A register operation as its client invoked it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Call {
    Read,
    Write(i64),
    /// Compare-and-set: write `new` if the value is `expected`.
    Cas {
        expected: i64,
        new: i64,
    },
}

/// What the client saw come back.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Ret {
    Read(Option<i64>),
    Write,
    Cas(bool),
}

/// One operation: logical invocation and response positions, from one
/// counter shared by every client; no response means an unknown outcome.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Op {
    pub client: u32,
    pub call: Call,
    pub invoke: u64,
    pub response: Option<(u64, Ret)>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Verdict {
    Linearizable,
    /// No sequential order explains the history. `stuck` lists the
    /// completed operations, by index, that no explanation could place.
    Violation {
        stuck: Vec<usize>,
    },
    /// The search hit its step budget.
    Inconclusive {
        steps: u64,
    },
}

/// Records a history from concurrent clients in one logical order.
#[derive(Default)]
pub struct History {
    inner: Mutex<(u64, Vec<Op>)>,
}

impl History {
    pub fn new() -> Self {
        Self::default()
    }
    /// Note an invocation; returns its token for [`History::respond`].
    pub fn invoke(&self, client: u32, call: Call) -> usize {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.0 += 1;
        let invoke = inner.0;
        inner.1.push(Op {
            client,
            call,
            invoke,
            response: None,
        });
        inner.1.len() - 1
    }
    /// Note a response. An operation never answered stays unknown.
    pub fn respond(&self, token: usize, ret: Ret) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.0 += 1;
        let at = inner.0;
        if let Some(op) = inner.1.get_mut(token) {
            op.response = Some((at, ret));
        }
    }
    pub fn ops(&self) -> Vec<Op> {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .1
            .clone()
    }
}

/// Apply `call` to `state`; the sequential register model.
fn step(state: Option<i64>, call: Call) -> (Option<i64>, Ret) {
    match call {
        Call::Read => (state, Ret::Read(state)),
        Call::Write(v) => (Some(v), Ret::Write),
        Call::Cas { expected, new } if state == Some(expected) => (Some(new), Ret::Cas(true)),
        Call::Cas { .. } => (state, Ret::Cas(false)),
    }
}

/// Wing–Gong–Lowe search with memoization: linearize, one at a time, an
/// operation invoked before every unlinearized completed operation
/// responded, until every completed operation is placed. Unknown operations
/// may be placed whenever they are eligible, or never.
pub fn check_register(initial: Option<i64>, ops: &[Op], max_steps: u64) -> Verdict {
    let completed: Vec<usize> = (0..ops.len())
        .filter(|&i| ops[i].response.is_some())
        .collect();
    let words = ops.len().div_ceil(64);
    let mut seen: HashSet<(Vec<u64>, Option<i64>)> = HashSet::new();
    // Depth-first over (linearized set, state), most recent first.
    let mut stack = vec![(vec![0u64; words], initial)];
    let mut steps = 0u64;
    let mut furthest: Vec<u64> = vec![0; words];
    let mut best = 0;
    let done = |set: &[u64], i: usize| set[i / 64] & (1 << (i % 64)) != 0;
    while let Some((set, state)) = stack.pop() {
        steps += 1;
        if steps > max_steps {
            return Verdict::Inconclusive { steps: max_steps };
        }
        let placed = completed.iter().filter(|&&i| done(&set, i)).count();
        if placed == completed.len() {
            return Verdict::Linearizable;
        }
        if placed > best {
            best = placed;
            furthest = set.clone();
        }
        // The earliest response among completed, unplaced operations bounds
        // which operations may come next.
        let horizon = completed
            .iter()
            .filter(|&&i| !done(&set, i))
            .map(|&i| ops[i].response.expect("completed").0)
            .min()
            .unwrap_or(u64::MAX);
        for (i, op) in ops.iter().enumerate() {
            if done(&set, i) || op.invoke >= horizon {
                continue;
            }
            let (next, ret) = step(state, op.call);
            if op.response.is_some_and(|(_, seen_ret)| seen_ret != ret) {
                continue;
            }
            let mut extended = set.clone();
            extended[i / 64] |= 1 << (i % 64);
            if seen.insert((extended.clone(), next)) {
                stack.push((extended, next));
            }
        }
    }
    Verdict::Violation {
        stuck: completed
            .iter()
            .copied()
            .filter(|&i| !done(&furthest, i))
            .collect(),
    }
}

/// Operation identities applied more than once, with their counts: a
/// non-idempotent retry's duplicate effects.
pub fn duplicates<T: Ord + Clone>(applied: &[T]) -> Vec<(T, usize)> {
    let mut counts: BTreeMap<T, usize> = BTreeMap::new();
    for id in applied {
        *counts.entry(id.clone()).or_default() += 1;
    }
    counts.into_iter().filter(|(_, n)| *n > 1).collect()
}

#[cfg(test)]
mod tests;
