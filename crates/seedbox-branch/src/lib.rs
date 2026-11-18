//! Branch a Driver world from the host.
//!
//! A [`Run`] is a Driver with the [`Program`] of inputs it is given and the
//! [`Trace`] of what it published. [`Run::save`] captures a [`Branch`], which
//! can be restored any number of times; each restored run takes whatever
//! program its caller gives it from there. Three things follow from keeping
//! the program outside the Driver and delivering each input at the barrier it
//! falls due:
//!
//! - A saved point carries no future input, so what follows it is the
//!   caller's choice alone.
//! - A run is its machines, its seed and its delivered inputs. Any restored
//!   run, however many saves deep, is reproduced by its program from boot.
//! - What one run saw at time `t` recurs at `t` on every run that takes the
//!   same inputs up to `t`. [`Branch::rewind`] uses that to go back to just
//!   before something a run has already seen, and checks it each time.
//!
//! [`Run::recover`] and [`Run::judge`] bring a run to a verdict: end what is
//! in force, tell the oracle to finish, and read the outcome. Everything
//! they do is an input of the program, so the verdict is reproduced with the
//! run. They spend the run, so they are for a branch restored for the
//! purpose.
//!
//! Which points to save, where to go back to and what to try from there are
//! the caller's. The crate's one policy is how a verdict ends a run: what
//! counts as in force and the input that undoes each, in [`recovery`].
mod program;
mod run;
mod trace;
mod verdict;

pub use program::{Input, InvalidProgram, Program, What};
pub use run::{Branch, History, Restore, Run, Stop};
pub use trace::{Entry, Record, Trace};
pub use verdict::{recovery, Judgement, Recovered, JUDGE_ROUNDS};

/// A run could not continue, or was asked for something it cannot do.
#[derive(Debug)]
pub enum Error<E> {
    Driver(seedbox_driver::driver::Error<E>),
    Program(InvalidProgram),
    /// An input fell due before the run's position.
    Stale,
    /// The Driver stopped somewhere a run cannot continue from by itself: a
    /// spawn waiting for its definition, a held fault, paused playback.
    Held(String),
    /// The branch is not an earlier point of the history it was asked to
    /// replay, or the time asked for precedes it.
    NotOnHistory,
    /// A replay differed from its history. `expected` is what the history
    /// has next for one machine and `found` what the replay has there.
    Diverged {
        expected: Option<Box<Record>>,
        found: Option<Box<Record>>,
    },
    /// [`Run::recover`] or [`Run::judge`] was given no time to settle, so
    /// simulated time could not move between its looks.
    NoSettle,
    /// [`Run::recover`] was given a label that makes an invalid input key.
    Label(seedbox_driver::driver::InvalidInputKey),
}

impl<E> From<seedbox_driver::driver::Error<E>> for Error<E> {
    fn from(error: seedbox_driver::driver::Error<E>) -> Self {
        Self::Driver(error)
    }
}

impl<E> From<InvalidProgram> for Error<E> {
    fn from(error: InvalidProgram) -> Self {
        Self::Program(error)
    }
}

impl<E: std::fmt::Display> std::fmt::Display for Error<E> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Driver(error) => write!(f, "{error}"),
            Self::Program(error) => write!(f, "{error}"),
            Self::Stale => f.write_str("an input falls due before the run's position"),
            Self::Held(what) => write!(f, "the run cannot continue: {what}"),
            Self::NotOnHistory => {
                f.write_str("the branch is not an earlier point of the history to replay")
            }
            Self::Diverged { expected, found } => write!(
                f,
                "the replay differs from its history: expected {expected:?}, found {found:?}"
            ),
            Self::NoSettle => f.write_str("settle must be longer than zero"),
            Self::Label(error) => write!(f, "invalid recovery label: {error}"),
        }
    }
}

impl<E: std::fmt::Debug + std::fmt::Display> std::error::Error for Error<E> {}
