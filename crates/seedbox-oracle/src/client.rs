//! A workload's operations against the system under test.
use crate::{Completion, Random};

/// One concurrent process's view of the system: it generates operations and
/// performs them, one at a time.
///
/// The runner gives each client process number `i` and records every
/// operation in the scenario's history. After an `info` completion the
/// client continues as process `i + clients`, as Jepsen does, so a process
/// never has two operations whose outcomes overlap. A client may reconnect
/// inside [`Client::invoke`], but never retries an operation there: a retry
/// is a new invocation.
pub trait Client: Send {
    type Op: Clone + Send + 'static;
    type Value: Send + 'static;

    /// The next operation to invoke, with every choice drawn from `random`;
    /// `None` ends this client's workload early.
    fn generate(&mut self, random: &mut dyn Random) -> Option<Self::Op>;

    /// Perform `op`. Return `Fail` only when it certainly had no effect;
    /// anything uncertain is `Info`.
    fn invoke(&mut self, op: &Self::Op) -> Completion<Self::Value>;
}
