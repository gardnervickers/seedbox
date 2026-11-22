//! The inputs of a run, kept outside the Driver.
//!
//! The Driver cannot retract a scheduled input, and a checkpoint carries
//! whatever is pending in it. So the host keeps the program and hands the
//! Driver each input at the barrier it falls due, as the service's own worker
//! does. A checkpoint then holds no future input, and what follows a saved
//! point is whatever program its branch is given.
use seedbox_driver::driver::{Action, FaultHandle, InputKey};
use seedbox_machine::machine::Timestamp;

/// What an input does when it falls due.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum What {
    /// A fault, admitted like any host input; `handle` names the handle it
    /// installs (see `Barrier::schedule_with_handle`).
    Action {
        action: Action,
        handle: Option<FaultHandle>,
    },
    /// A breakpoint hit from the host (see `Barrier::host_break`). A name no
    /// machine has declared does nothing; the trace does not record it, and
    /// `Run::missed` lists the input.
    Break(String),
    /// Nothing but a stop: the Driver's round ends at this time. A run
    /// otherwise stops only where it would anyway (see `Run::advance`),
    /// because cutting a round short changes what follows; a stop in the
    /// program makes that cut part of the run, reproduced with it.
    Stop,
}

/// One timed input. Keys are unique within a program.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Input {
    pub at: Timestamp,
    pub key: InputKey,
    pub what: What,
}

impl Input {
    pub fn action(at: Timestamp, key: InputKey, action: Action) -> Self {
        Self {
            at,
            key,
            what: What::Action {
                action,
                handle: None,
            },
        }
    }

    pub fn host_break(at: Timestamp, key: InputKey, name: &str) -> Self {
        Self {
            at,
            key,
            what: What::Break(name.to_owned()),
        }
    }

    pub fn stop(at: Timestamp, key: InputKey) -> Self {
        Self {
            at,
            key,
            what: What::Stop,
        }
    }
}

/// Why a program was refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InvalidProgram {
    /// Two inputs share this key.
    DuplicateKey(InputKey),
    /// This input falls due before the position it would be delivered from.
    Stale { key: InputKey, at: Timestamp },
}

impl std::fmt::Display for InvalidProgram {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::DuplicateKey(key) => write!(f, "input key {} is used twice", key.as_str()),
            Self::Stale { key, at } => write!(
                f,
                "input {} at {} ns falls due before the run's position",
                key.as_str(),
                at.as_nanos()
            ),
        }
    }
}

/// The ordered inputs of one run and how many it has delivered. The
/// delivered prefix is history and never changes; the rest can be replaced.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Program {
    inputs: Vec<Input>,
    next: usize,
}

impl Program {
    /// `inputs` in time order; inputs due together keep their given order.
    pub fn new(inputs: Vec<Input>) -> Result<Self, InvalidProgram> {
        let mut program = Self::default();
        program.replace_pending(inputs, Timestamp::from_nanos(0))?;
        Ok(program)
    }

    /// The inputs already delivered, in delivery order.
    pub fn delivered(&self) -> &[Input] {
        &self.inputs[..self.next]
    }

    /// The inputs still to come, in time order.
    pub fn pending(&self) -> &[Input] {
        &self.inputs[self.next..]
    }

    /// Replace every pending input. None may fall due before `now` or reuse
    /// a delivered input's key.
    pub fn replace_pending(
        &mut self,
        mut inputs: Vec<Input>,
        now: Timestamp,
    ) -> Result<(), InvalidProgram> {
        inputs.sort_by_key(|input| input.at);
        let mut keys: std::collections::BTreeSet<&str> = self
            .delivered()
            .iter()
            .map(|input| input.key.as_str())
            .collect();
        for input in &inputs {
            if input.at < now {
                return Err(InvalidProgram::Stale {
                    key: input.key.clone(),
                    at: input.at,
                });
            }
            if !keys.insert(input.key.as_str()) {
                return Err(InvalidProgram::DuplicateKey(input.key.clone()));
            }
        }
        self.inputs.truncate(self.next);
        self.inputs.extend(inputs);
        Ok(())
    }

    /// Add pending inputs to those already pending.
    pub fn extend_pending(
        &mut self,
        inputs: impl IntoIterator<Item = Input>,
        now: Timestamp,
    ) -> Result<(), InvalidProgram> {
        let mut pending = self.pending().to_vec();
        pending.extend(inputs);
        self.replace_pending(pending, now)
    }

    /// The next pending input's time.
    pub(crate) fn next_at(&self) -> Option<Timestamp> {
        self.inputs.get(self.next).map(|input| input.at)
    }

    /// Take the next pending input if it is due at `now`.
    pub(crate) fn take_due(&mut self, now: Timestamp) -> Option<Input> {
        let input = self.inputs.get(self.next).filter(|input| input.at <= now)?;
        self.next += 1;
        Some(input.clone())
    }

    /// Whether `self`'s delivered inputs begin `other`'s: the two runs took
    /// the same inputs up to `self`'s position.
    pub(crate) fn begins(&self, other: &Program) -> bool {
        other.delivered().starts_with(self.delivered())
    }
}
