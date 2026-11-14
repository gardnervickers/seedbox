//! A scenario's phases and its result.
use crate::{Checker, Client, Error, History, Nemesis, Validity, Verdict, World};
use serde_json::Value;
use std::{sync::Arc, time::Duration};

/// The final-read discipline Jepsen's final generator follows. After the
/// nemesis heals every fault, the runner waits for [`FinalRead::ready`];
/// when that fails it restarts every node [`FinalRead::exited`] reports
/// (an OOM kill, say) and waits again. It then polls every node's
/// [`FinalRead::view`] until all agree for [`FinalRead::agree_for`], or
/// [`FinalRead::settle`] passes regardless, and reads each node in order.
pub trait FinalRead {
    type Read;
    /// The nodes to read, in order.
    fn nodes(&self) -> Vec<String>;
    /// Whether every node serves again; it may wait, bounded, in guest time.
    fn ready(&mut self) -> bool;
    /// Whether this node's process exited on its own and needs a restart.
    fn exited(&mut self, node: &str) -> bool;
    /// The node's view of replicated state that must converge before
    /// reading, such as a stream's last sequence.
    fn view(&mut self, node: &str) -> Value;
    fn read(&mut self, node: &str) -> Result<Self::Read, String>;
    fn settle(&self) -> Duration {
        Duration::from_secs(30)
    }
    fn agree_for(&self) -> Duration {
        Duration::from_secs(5)
    }
}

/// A test of one system: the runner calls [`Scenario::setup`], runs the
/// clients on their own threads while the nemesis runs on the oracle's,
/// stops the clients once the nemesis returns, heals, runs the final reads,
/// then evaluates each decided checker's verdict as its `always` property
/// and flushes.
pub trait Scenario {
    type Op: Clone + Send + 'static;
    type Value: Send + 'static;
    type Read;
    type Client: Client<Op = Self::Op, Value = Self::Value>;

    fn name(&self) -> &str;
    /// Wait for the system and prepare it (create a stream, load accounts).
    /// A failure ends the run with every verdict unknown.
    fn setup(&mut self, world: &mut dyn World) -> Result<(), Error>;
    /// The history every client and the final reads record into. A
    /// scenario whose operations happen elsewhere, such as on a client
    /// machine that streams its invocations to the oracle, records them here
    /// itself.
    fn history(&self) -> Arc<dyn History<Self::Op, Self::Value>>;
    /// One client per concurrent process; empty when the history comes from
    /// elsewhere.
    fn clients(&mut self) -> Vec<Self::Client>;
    fn nemesis(&mut self) -> Box<dyn Nemesis>;
    fn final_read(&mut self) -> Box<dyn FinalRead<Read = Self::Read>>;
    fn checkers(&self) -> Checkers<Self::Op, Self::Value, Self::Read>;
}

/// A scenario's checkers, each judging the same history and final reads.
pub type Checkers<O, V, R> = Vec<Box<dyn Checker<O, V, R>>>;

/// What a scenario run concluded: one verdict per checker in order, and
/// how the run's own steps went. A failed step does not end the run (the
/// runner still heals, reads and checks), but it leaves the run incomplete:
/// the faults the scenario describes did not all happen or did not all end,
/// a client stopped early, a node the final reads needed was not restarted,
/// breakpoint serving stopped, or the verdicts may not have reached the
/// run's results.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Report {
    pub verdicts: Vec<(String, Verdict)>,
    /// [`Nemesis::run`]'s error, if it returned one.
    pub run_error: Option<Error>,
    /// [`Nemesis::heal`]'s error, if it returned one.
    pub heal_error: Option<Error>,
    /// Each client thread that panicked, by index.
    pub panics: Vec<ClientPanic>,
    /// Each restart the [`crate::final_reads`] asked for and the world
    /// refused.
    pub restart_errors: Vec<Error>,
    /// The error that stopped the world serving breakpoints, if one did:
    /// [`World::break_error`] after the final reads.
    pub break_error: Option<Error>,
    /// The final property flush's error, if it failed: the verdicts, and
    /// any properties the scenario evaluated itself, may not count.
    pub flush_error: Option<Error>,
}

/// A client thread that panicked.
#[derive(Clone, Debug, PartialEq)]
pub struct ClientPanic {
    /// The client's index in [`Scenario::clients`].
    pub client: usize,
    /// The panic's message, or `(no message)`.
    pub message: String,
}

impl Report {
    /// Every check held and every step ran.
    pub const OK: u8 = 0;
    /// Some check was violated.
    pub const INVALID: u8 = 1;
    /// No check was violated, but a step failed or some check could not
    /// decide: the run is incomplete, neither a pass nor a bug.
    pub const UNKNOWN: u8 = 3;

    /// One line for each step that failed, in the order the steps run: the
    /// nemesis run, the clients by index, the heal, the final reads'
    /// restarts, breakpoint serving, then the property flush.
    pub fn failures(&self) -> Vec<String> {
        let run = self.run_error.iter().map(|e| format!("nemesis run: {e}"));
        let panics = self.panics.iter();
        let panics = panics.map(|p| format!("client {} panicked: {}", p.client, p.message));
        let heal = self.heal_error.iter().map(|e| format!("nemesis heal: {e}"));
        let restarts = self.restart_errors.iter();
        let restarts = restarts.map(|e| format!("final reads: {e}"));
        let breaks = self.break_error.iter().map(|e| format!("breakpoints: {e}"));
        let flush = self
            .flush_error
            .iter()
            .map(|e| format!("property flush: {e}"));
        run.chain(panics)
            .chain(heal)
            .chain(restarts)
            .chain(breaks)
            .chain(flush)
            .collect()
    }

    /// The oracle's exit status for this report: [`Report::INVALID`] when a
    /// check was violated; otherwise [`Report::UNKNOWN`] when a step failed,
    /// a check could not decide or there were no checks; otherwise
    /// [`Report::OK`].
    pub fn exit_code(&self) -> u8 {
        let has = |v: Validity| self.verdicts.iter().any(|(_, d)| d.validity == v);
        if has(Validity::Invalid) {
            Self::INVALID
        } else if !self.failures().is_empty() || has(Validity::Unknown) || self.verdicts.is_empty()
        {
            Self::UNKNOWN
        } else {
            Self::OK
        }
    }
}
