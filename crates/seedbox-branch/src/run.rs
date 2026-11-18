//! A live run with its program and trace, and saved points of it.
use crate::{Error, Input, Program, Record, Trace, What};
use seedbox_driver::driver::{Barrier, Checkpoint, Driver, InputKey, StepOutcome};
use seedbox_engine_protocol::property::Assessment;
use seedbox_machine::machine::{ExitReason, Machine, Timestamp};

/// Why [`Run::advance`] returned.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stop {
    /// The run is at the time asked for.
    Reached,
    /// The caller's watch asked to stop at this barrier.
    Watched,
    /// The oracle finished; the run goes no further.
    Finished(ExitReason),
}

/// A Driver, the program it is given and the trace of what it published.
///
/// The Driver holds no future input: each is scheduled at the barrier it
/// falls due. So a run is defined by its machines, its seed and the inputs
/// delivered so far, and [`Run::save`] captures a point any program can
/// follow.
pub struct Run<M: Machine> {
    driver: Driver<M>,
    now: Timestamp,
    program: Program,
    trace: Trace,
    missed: Vec<InputKey>,
    finished: Option<ExitReason>,
}

/// A run's program and trace, live or saved.
#[derive(Clone, Copy)]
pub struct History<'a> {
    pub program: &'a Program,
    pub trace: &'a Trace,
}

impl<M> Run<M>
where
    M: Machine + Send + 'static,
    M::Context: 'static,
    M::Error: Send + 'static,
{
    /// Take over `driver` at its current barrier. It must hold no scheduled
    /// input of its own; `program` supplies them all from here.
    pub fn new(mut driver: Driver<M>, program: Program) -> Result<Self, Error<M::Error>> {
        let now = driver.with_barrier(|barrier| Ok(barrier.playback().position.at))?;
        let mut trace = Trace::default();
        trace.take(driver.drain_observations().events);
        if program.next_at().is_some_and(|at| at < now) {
            return Err(Error::Stale);
        }
        Ok(Self {
            driver,
            now,
            program,
            trace,
            missed: Vec::new(),
            finished: None,
        })
    }

    pub fn now(&self) -> Timestamp {
        self.now
    }

    pub fn trace(&self) -> &Trace {
        &self.trace
    }

    pub fn program(&self) -> &Program {
        &self.program
    }

    pub fn history(&self) -> History<'_> {
        History {
            program: &self.program,
            trace: &self.trace,
        }
    }

    /// The breakpoint inputs that fell due when no machine had declared
    /// their name, and so did nothing.
    pub fn missed(&self) -> &[InputKey] {
        &self.missed
    }

    /// The oracle's exit, once it has finished.
    pub fn finished(&self) -> Option<ExitReason> {
        self.finished
    }

    /// The property findings so far.
    pub fn properties(&self) -> Option<Assessment> {
        self.driver.properties()
    }

    /// Read the world at the current barrier. Changing it here, other than
    /// through the program, makes the run something its program no longer
    /// describes.
    pub fn barrier<R>(
        &mut self,
        read: impl FnOnce(&mut Barrier<'_, M>) -> Result<R, seedbox_driver::driver::Error<M::Error>>,
    ) -> Result<R, Error<M::Error>> {
        Ok(self.driver.with_barrier(read)?)
    }

    /// Replace everything the program has not yet delivered.
    pub fn replace_pending(&mut self, inputs: Vec<Input>) -> Result<(), Error<M::Error>> {
        Ok(self.program.replace_pending(inputs, self.now)?)
    }

    /// Add inputs to those the program has not yet delivered.
    pub fn extend_pending(
        &mut self,
        inputs: impl IntoIterator<Item = Input>,
    ) -> Result<(), Error<M::Error>> {
        Ok(self.program.extend_pending(inputs, self.now)?)
    }

    /// Run to `until`, delivering each input at the barrier it falls due.
    pub fn advance(&mut self, until: Timestamp) -> Result<Stop, Error<M::Error>> {
        self.advance_watching(until, |_, _| false)
    }

    /// [`Self::advance`], asking `watch` at every barrier whether to stop
    /// there. It gets the trace and the index of the first record the last
    /// step added.
    pub fn advance_watching(
        &mut self,
        until: Timestamp,
        mut watch: impl FnMut(&Trace, usize) -> bool,
    ) -> Result<Stop, Error<M::Error>> {
        let mut stalled = 0;
        loop {
            if let Some(reason) = self.finished {
                return Ok(Stop::Finished(reason));
            }
            let delivered = self.deliver_due()?;
            if self.now >= until {
                return Ok(Stop::Reached);
            }
            let upto = self.program.next_at().map_or(until, |at| at.min(until));
            // Time moves toward the caller's bound also while every machine
            // is idle: the next input, or the caller, is waiting there.
            let outcome = self.driver.step_toward_external(upto)?;
            let first = self.trace.take(self.driver.drain_observations().events);
            let at = match outcome {
                StepOutcome::BarrierReached { at } | StepOutcome::Blocked { at } => at,
                StepOutcome::Finished { at, reason } => {
                    self.now = at;
                    self.finished = Some(reason);
                    return Ok(Stop::Finished(reason));
                }
                other => return Err(Error::Held(format!("{other:?}"))),
            };
            stalled = if at == self.now && !delivered {
                stalled + 1
            } else {
                0
            };
            if stalled > 2 {
                return Err(Error::Held(format!(
                    "no progress at {} ns toward {} ns",
                    at.as_nanos(),
                    upto.as_nanos()
                )));
            }
            self.now = at;
            if watch(&self.trace, first) {
                return Ok(Stop::Watched);
            }
        }
    }

    /// Deliver every input due now; whether there was one.
    fn deliver_due(&mut self) -> Result<bool, Error<M::Error>> {
        let mut delivered = false;
        while let Some(input) = self.program.take_due(self.now) {
            self.deliver(input)?;
            delivered = true;
        }
        Ok(delivered)
    }

    fn deliver(&mut self, input: Input) -> Result<(), Error<M::Error>> {
        if input.at < self.now {
            return Err(Error::Stale);
        }
        let now = self.now;
        let key = input.key.clone();
        let heard = self.driver.with_barrier(|barrier| match input.what {
            What::Action {
                action,
                handle: Some(handle),
            } => barrier
                .schedule_with_handle(now, input.key, action, handle)
                .map(|_| true),
            What::Action {
                action,
                handle: None,
            } => barrier.schedule(now, input.key, action).map(|_| true),
            What::Break(name) => barrier.host_break(&name),
        })?;
        if !heard {
            self.missed.push(key);
        }
        Ok(())
    }

    /// Save this point. The run continues; the branch can be restored any
    /// number of times, in any order.
    pub fn save(&mut self) -> Result<Branch<M::Snapshot>, Error<M::Error>> {
        Ok(Branch {
            at: self.now,
            checkpoint: self.driver.checkpoint()?,
            program: self.program.clone(),
            trace: self.trace.fork(),
            missed: self.missed.clone(),
        })
    }
}

/// Builds a Driver from a checkpoint: the caller's machines, workers and
/// factory. Implemented by any `FnMut(&Checkpoint<S>) -> Result<Driver<M>, _>`.
pub trait Restore<M: Machine> {
    fn restore(
        &mut self,
        checkpoint: &Checkpoint<M::Snapshot>,
    ) -> Result<Driver<M>, seedbox_driver::driver::Error<M::Error>>;
}

impl<M, F> Restore<M> for F
where
    M: Machine,
    F: FnMut(
        &Checkpoint<M::Snapshot>,
    ) -> Result<Driver<M>, seedbox_driver::driver::Error<M::Error>>,
{
    fn restore(
        &mut self,
        checkpoint: &Checkpoint<M::Snapshot>,
    ) -> Result<Driver<M>, seedbox_driver::driver::Error<M::Error>> {
        self(checkpoint)
    }
}

/// A saved point of a run: its checkpoint, the program that led to it and
/// its trace up to it.
pub struct Branch<S> {
    at: Timestamp,
    checkpoint: Checkpoint<S>,
    program: Program,
    trace: Trace,
    missed: Vec<InputKey>,
}

impl<S> Branch<S> {
    pub fn at(&self) -> Timestamp {
        self.at
    }

    pub fn trace(&self) -> &Trace {
        &self.trace
    }

    pub fn program(&self) -> &Program {
        &self.program
    }

    pub fn history(&self) -> History<'_> {
        History {
            program: &self.program,
            trace: &self.trace,
        }
    }

    /// A run continuing from this point with the inputs that were pending
    /// when it was saved.
    pub fn restore<M>(&self, world: &mut impl Restore<M>) -> Result<Run<M>, Error<M::Error>>
    where
        M: Machine<Snapshot = S> + Send + 'static,
        M::Context: 'static,
        M::Error: Send + 'static,
    {
        Ok(Run {
            driver: world.restore(&self.checkpoint)?,
            now: self.at,
            program: self.program.clone(),
            trace: self.trace.continued(),
            missed: self.missed.clone(),
            finished: None,
        })
    }

    /// Go back to just before something `history` saw: restore this branch,
    /// which must be an earlier point of the same run, and replay
    /// `history`'s own inputs up to `until`. The run returned is where
    /// `history` stood at `until`: every input due at or before `until` is
    /// delivered, as [`Run::advance`] delivers them, and nothing its
    /// machines did at `until` or later has happened. No input is pending;
    /// `history`'s later ones are dropped, so what follows is this run's own.
    /// To leave out an input due at `until`, rewind to a nanosecond earlier.
    ///
    /// The replay is checked against `history`: every machine must say and
    /// do the same things at the same times. A difference is
    /// [`Error::Diverged`], which means the two are not one run after all.
    pub fn rewind<M>(
        &self,
        history: History<'_>,
        until: Timestamp,
        world: &mut impl Restore<M>,
    ) -> Result<Run<M>, Error<M::Error>>
    where
        M: Machine<Snapshot = S> + Send + 'static,
        M::Context: 'static,
        M::Error: Send + 'static,
    {
        let delivered = self.program.delivered().len();
        if until < self.at
            || !self.program.begins(history.program)
            || self.trace.len() > history.trace.len()
        {
            return Err(Error::NotOnHistory);
        }
        let between: Vec<Input> = history.program.delivered()[delivered..]
            .iter()
            .chain(history.program.pending())
            .filter(|input| input.at <= until)
            .cloned()
            .collect();
        let mut run = self.restore(world)?;
        run.replace_pending(between)?;
        run.advance(until)?;
        let from = self.trace.len();
        let expected = history.trace.occurrences(from);
        let found = run.trace.occurrences(from);
        for index in 0..expected.len().max(found.len()) {
            let expected = expected.get(index).map_or(&[][..], Vec::as_slice);
            let found = found.get(index).map_or(&[][..], Vec::as_slice);
            let agree = expected
                .iter()
                .zip(found)
                .take_while(|(expected, found)| expected == found)
                .count();
            // What history has beyond the replay must not have happened yet
            // at `until`; a line may have begun before it and ended after,
            // and the replay then holds its beginning.
            let ahead = expected[agree..]
                .iter()
                .find(|record| record.at < until && !run.trace.begins(record));
            if let Some(unexpected) = found.get(agree) {
                return Err(Error::diverged(expected.get(agree), Some(unexpected)));
            }
            if let Some(missing) = ahead {
                return Err(Error::diverged(Some(missing), None));
            }
        }
        Ok(run)
    }
}

impl<E> Error<E> {
    fn diverged(expected: Option<&&Record>, found: Option<&&Record>) -> Self {
        Self::Diverged {
            expected: expected.map(|record| Box::new((*record).clone())),
            found: found.map(|record| Box::new((*record).clone())),
        }
    }
}
