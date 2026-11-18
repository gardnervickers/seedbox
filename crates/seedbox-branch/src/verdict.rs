//! Bringing a run to a verdict: end what is in force, tell the oracle to
//! finish, read the outcome. Written over [`Run`]'s public surface.
use crate::{Error, Input, Run, Stop};
use seedbox_driver::driver::{Action, HealTarget, InputKey, MachineState, Target, WorldView};
use seedbox_engine_protocol::property::Assessment;
use seedbox_machine::machine::{ExitReason, Machine, Timestamp};
use std::time::Duration;

impl<M> Run<M>
where
    M: Machine + Send + 'static,
    M::Context: 'static,
    M::Error: Send + 'static,
{
    /// End everything in force: drop the inputs still to come, then heal
    /// every active handle and bring each machine back to running, looking
    /// again every `settle` until nothing is left to undo. A machine can hide
    /// one condition behind another (a frozen application in a paused
    /// machine), so this takes as many looks as there are layers, up to
    /// `rounds`. Returns whether the world came back clean, and the inputs
    /// it dropped.
    ///
    /// A machine that went down is rebooted whether a fault or the guest
    /// itself crashed it, so `clean` says the world runs again, not that
    /// nothing went wrong: an unallowed crash stays a violation in
    /// [`Run::properties`].
    ///
    /// Each undo is an input of the program, keyed under `label`. A zero
    /// `settle` is refused, since time could not move between looks.
    pub fn recover(
        &mut self,
        label: &str,
        settle: Duration,
        rounds: usize,
    ) -> Result<Recovered, Error<M::Error>> {
        if settle.is_zero() {
            return Err(Error::NoSettle);
        }
        let dropped = self.program().pending().iter();
        let dropped = dropped.map(|input| input.key.clone()).collect();
        self.replace_pending(Vec::new())?;
        let mut clean = None;
        for round in 0..rounds {
            let view = self.barrier(|barrier| barrier.view())?;
            let undo = recovery(&view, self.now(), &format!("{label}{round}-"));
            if undo.is_empty() {
                clean = Some(true);
                break;
            }
            self.extend_pending(undo)?;
            if let Stop::Finished(_) = self.advance(after(self.now(), settle))? {
                clean = Some(false);
                break;
            }
        }
        let clean = match clean {
            Some(clean) => clean,
            None => {
                let view = self.barrier(|barrier| barrier.view())?;
                recovery(&view, self.now(), "").is_empty()
            }
        };
        Ok(Recovered { clean, dropped })
    }

    /// Bring the run to a verdict: [`Self::recover`], hit the host
    /// breakpoint `finish` so the oracle wraps up, and run until the oracle
    /// finishes or `patience` passes. An oracle that has not declared
    /// `finish` is asked again every `settle`.
    ///
    /// Each time the oracle is asked is an input of the program, keyed
    /// `judge-finish-N`, so the judged run's program reproduces its verdict
    /// from boot. The inputs that were still to come are dropped, as
    /// [`Self::recover`] drops them, and the judgement names them.
    ///
    /// Recovery starts where the run stands and the first ask follows it, so
    /// advance to the time to judge from before calling.
    ///
    /// This spends the run. Judge a branch restored for the purpose, and
    /// continue from its saved point.
    pub fn judge(
        &mut self,
        finish: &str,
        settle: Duration,
        patience: Duration,
    ) -> Result<Judgement, Error<M::Error>> {
        if settle.is_zero() {
            return Err(Error::NoSettle);
        }
        let Recovered {
            clean: recovered,
            dropped,
        } = self.recover("judge-", settle, 8)?;
        let deadline = after(self.now(), patience);
        let mut signalled = false;
        let mut asked = 0;
        while self.finished().is_none() && self.now() < deadline {
            if !signalled {
                let key = InputKey::new(format!("judge-finish-{asked}"));
                let key = key.expect("a valid input key");
                asked += 1;
                let missed = self.missed().len();
                self.extend_pending([Input::host_break(self.now(), key, finish)])?;
                // Advancing to now delivers what is due and runs nothing.
                self.advance(self.now())?;
                signalled = self.missed().len() == missed;
            }
            let pause = if signalled { patience } else { settle };
            self.advance(after(self.now(), pause).min(deadline))?;
        }
        Ok(Judgement {
            at: self.now(),
            recovered,
            signalled,
            dropped,
            finished: self.finished(),
            properties: self.properties(),
        })
    }
}

fn after(now: Timestamp, duration: Duration) -> Timestamp {
    let nanos = u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX);
    Timestamp::from_nanos(now.as_nanos().saturating_add(nanos))
}

/// What [`Run::recover`] did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Recovered {
    /// Every fault was ended and every machine was running.
    pub clean: bool,
    /// The inputs that were still to come, which the run therefore never
    /// took.
    pub dropped: Vec<InputKey>,
}

/// How a judged run ended.
#[derive(Clone, Debug)]
pub struct Judgement {
    pub at: Timestamp,
    /// Every fault was ended and every machine was running before the
    /// oracle was asked to finish. See [`Run::recover`]: a crash the guest
    /// caused itself shows in `properties`, not here.
    pub recovered: bool,
    /// The oracle had declared the finish breakpoint and was hit.
    pub signalled: bool,
    /// The inputs that were still to come when the run was judged, which it
    /// therefore never took.
    pub dropped: Vec<InputKey>,
    /// The oracle's exit; `None` when it had not finished in time.
    pub finished: Option<ExitReason>,
    pub properties: Option<Assessment>,
}

/// The inputs at `at` that end what `view` shows in force: a heal for every
/// active handle, and for each machine the counterpart of its state. An
/// application that exited by itself is left as it is. Keys begin with
/// `label`.
pub fn recovery(view: &WorldView, at: Timestamp, label: &str) -> Vec<Input> {
    const DEADLINE: Duration = Duration::from_secs(5);
    let key = |text: String| InputKey::new(text).expect("a valid input key");
    let mut inputs: Vec<Input> = view
        .handles
        .iter()
        .enumerate()
        .map(|(index, handle)| {
            Input::action(
                at,
                key(format!("{label}heal-{index}")),
                Action::Heal(HealTarget::Handle(handle.clone())),
            )
        })
        .collect();
    for machine in &view.machines {
        let target = Target::Machine(machine.id);
        let (what, action) = match machine.state {
            MachineState::Paused => ("resume", Action::Resume(target)),
            MachineState::Down => ("reboot", Action::Reboot(target)),
            MachineState::Frozen => (
                "thaw",
                Action::ResumeProcess {
                    target,
                    deadline: DEADLINE,
                },
            ),
            MachineState::Stopped => (
                "restart",
                Action::Restart {
                    target,
                    deadline: DEADLINE,
                },
            ),
            _ => continue,
        };
        inputs.push(Input::action(
            at,
            key(format!("{label}{what}-{}", machine.id.index())),
            action,
        ));
    }
    inputs
}
