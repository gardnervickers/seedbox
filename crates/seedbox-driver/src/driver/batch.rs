//! Batch execution: due machines run on the Driver's thread in their own
//! time bases, and accepted runs commit in identity order.
use super::*;

/// Each batch machine's start, bound and offered input count, in its own
/// time base, and the simulation time its machine times map from.
pub(super) type BatchBounds = BTreeMap<MachineId, (Timestamp, Timestamp, usize, Timestamp)>;

/// A batch machine's start, bound and due inputs, in its own time base, and
/// whether its inbox is a timed one: inputs after the due ones are dated
/// inside the run ([`Inbox::with_timed`]).
pub(super) type BatchView = (MachineId, Timestamp, Timestamp, VecDeque<Input>, bool);

/// A finished run: the taken machine and RNG, the offered inputs it left,
/// and its outputs and stop, in its own time base.
pub(super) struct RunResult<M: Machine> {
    pub(super) id: MachineId,
    machine: M,
    rng: SeededRng,
    pub(super) leftover: VecDeque<Input>,
    outputs: Vec<crate::pv::Output>,
    stopped: Result<StopReason, Error<M::Error>>,
}

/// One machine's run on the Driver's thread, held while a split machine
/// executes elsewhere (see [`Machine::split_run`]).
pub(super) struct Flight<M: Machine> {
    id: MachineId,
    machine: M,
    rng: SeededRng,
    inbox: VecDeque<Input>,
    outputs: Vec<crate::pv::Output>,
    start: Timestamp,
    bound: Timestamp,
    timed: bool,
    execution: crate::execution::ExecutionConfig,
}

/// A split run, unless it finished as it started.
pub(super) enum Started<M: Machine> {
    Finished(RunResult<M>),
    Flying(Flight<M>),
}

impl<M: Machine> Flight<M> {
    /// Call the machine with this run's context, reporting a panic as an error.
    fn call<T>(
        &mut self,
        context: &mut M::Context,
        call: impl FnOnce(&mut M, &mut RunContext<'_, M::Context>) -> Result<T, M::Error>,
    ) -> Result<T, Error<M::Error>> {
        let id = self.id;
        catch_unwind(AssertUnwindSafe(|| {
            call(
                &mut self.machine,
                &mut RunContext {
                    worker: context,
                    clock: &FixedClock(self.start.as_nanos()),
                    rng: &mut self.rng,
                    execution: self.execution,
                    inbox: run_inbox(&mut self.inbox, self.start, self.bound, self.timed),
                    outbox: Outbox::new(&mut self.outputs, self.start, self.bound),
                },
            )
        }))
        .map_err(|_| Error::Panicked { id })
        .and_then(|result| result.map_err(|source| Error::Machine { id, source }))
    }

    /// Whether the run has stopped, so that finishing it will not wait.
    pub(super) fn ready(&mut self) -> Result<bool, Error<M::Error>> {
        catch_unwind(AssertUnwindSafe(|| {
            self.machine
                .split_run()
                .expect("split run in flight")
                .ready()
        }))
        .map_err(|_| Error::Panicked { id: self.id })
    }

    pub(super) fn watch_reply(&mut self, ticket: u64) -> Option<std::os::fd::RawFd> {
        self.machine.split_run()?.watch_reply(ticket)
    }
    /// Wait for the run to stop and return its result.
    pub(super) fn finish(mut self, context: &mut M::Context) -> RunResult<M> {
        let stopped = self.call(context, |machine, context| {
            machine
                .split_run()
                .expect("split run in flight")
                .finish(context)
        });
        self.result(stopped)
    }

    pub(super) fn result(self, stopped: Result<StopReason, Error<M::Error>>) -> RunResult<M> {
        RunResult {
            id: self.id,
            leftover: self.inbox,
            machine: self.machine,
            rng: self.rng,
            outputs: self.outputs,
            stopped,
        }
    }
}

/// A run's inbox, due through `start`, and in a timed run also offering the
/// inputs dated before `bound`.
fn run_inbox(
    inputs: &mut VecDeque<Input>,
    start: Timestamp,
    bound: Timestamp,
    timed: bool,
) -> Inbox<'_> {
    let inbox = Inbox::new(inputs, start);
    if timed {
        inbox.with_timed(bound)
    } else {
        inbox
    }
}

/// One machine's finished batch run, still in its own time base.
pub(super) struct RunReport<E> {
    id: MachineId,
    outputs: Vec<crate::pv::Output>,
    stopped: Result<StopReason, Error<E>>,
    /// Whether the run accepted a Crash or Reboot input.
    admitted_end: bool,
}

/// A validated batch run, converted back to simulation time.
pub(super) struct AcceptedRun {
    id: MachineId,
    reached: Timestamp,
    stopped: StopReason,
    outputs: Vec<crate::pv::Output>,
    /// The run's reached position, in machine time.
    position: Timestamp,
    /// A pending timer, in machine time.
    timer: Option<Timestamp>,
    last_ended: Option<u64>,
}

impl AcceptedRun {
    /// The simulation time the run reached.
    pub(super) fn reached(&self) -> Timestamp {
        self.reached
    }

    /// Its stop, in simulation time.
    pub(super) fn stopped(&self) -> StopReason {
        self.stopped
    }
}

impl<M> Driver<M>
where
    M: Machine,
{
    /// Give each of `ids` a zero-budget admission run at `now`, the current
    /// barrier: it takes its due inputs, adopts shared time and makes a due
    /// timer eligible, and executes nothing.
    pub(super) fn run_batch(
        &mut self,
        ids: &[MachineId],
        now: Timestamp,
    ) -> Result<(), Error<M::Error>> {
        self.run_batch_as(ids, now, None)
    }

    /// Run a zero-budget admission batch. `rebooting` names the
    /// operation-local prospective boot of a single machine's Reboot
    /// admission run: its outputs carry that boot, which
    /// `Control::admission` commits only after the run is confirmed.
    pub(super) fn run_batch_as(
        &mut self,
        ids: &[MachineId],
        now: Timestamp,
        rebooting: Option<u64>,
    ) -> Result<(), Error<M::Error>> {
        if ids.is_empty() {
            return Ok(());
        }
        debug_assert!(rebooting.is_none() || ids.len() == 1);
        debug_assert_eq!(now, self.now(), "admission runs happen at the barrier");
        let upto = now;
        let offered_commands = self.offered_commands(ids);
        let offered_stops = self.offered_stops(ids);
        let (bounds, views) = self.batch_views(ids, now, upto, true)?;
        let reports = self.execute_batch(views, &bounds);
        // Validate each stop in its machine's time base, then convert back.
        // The conversion returns the earliest simulation time whose machine
        // time reaches the reported one, so nothing is reported early, and a
        // machine time at most the mapped bound maps to at most `upto`.
        let mut accepted = Vec::with_capacity(reports.len());
        for report in reports {
            let (start, bound, _, lower) = bounds[&report.id];
            accepted.push(self.accept_run(report, start, bound, lower, now, upto)?);
        }
        self.confirm_offered_commands(&offered_commands, rebooting)?;
        self.commit_runs(accepted, &offered_stops, rebooting);
        Ok(())
    }

    /// The boot a batch's outputs and accepted commands belong to.
    fn batch_boot(entry: &Entry<M>, rebooting: Option<u64>) -> u64 {
        rebooting.unwrap_or(entry.lifecycle.boot())
    }

    /// Guest commands queued for the batch's machines, offered to this run.
    pub(super) fn offered_commands(
        &self,
        ids: &[MachineId],
    ) -> Vec<(MachineId, NonZeroU32, Timestamp)> {
        ids.iter()
            .flat_map(|&machine| {
                self.execution.machines[machine.0]
                    .inbox
                    .iter()
                    .filter_map(move |input| {
                        control::command(&input.payload).map(|(id, _)| (machine, id, input.at))
                    })
            })
            .collect()
    }

    /// Application stop commands queued for the oracle, when it is in the batch.
    pub(super) fn offered_stops(&self, ids: &[MachineId]) -> Vec<NonZeroU32> {
        if !ids.contains(&self.execution.oracle) {
            return Vec::new();
        }
        self.execution.machines[self.execution.oracle.0]
            .inbox
            .iter()
            .filter_map(|input| {
                if let Request::Application {
                    id,
                    command: crate::pv::ApplicationCommand::Stop,
                } = input.payload
                {
                    Some(id)
                } else {
                    None
                }
            })
            .collect()
    }

    /// Map the common barrier into every machine's own time base before
    /// submitting any work: start, bound and due inputs. Inputs that became
    /// due before the last rate change are offered at that change.
    ///
    /// An `admission` run has no budget: it starts at `now`'s machine time,
    /// its bound. Only a lane's run starts at a timer deadline a fast clock
    /// places inside the nanosecond (`Entry::start_time`); an admission run
    /// that started there would execute the machine from the deadline to
    /// the barrier, also while it is paused or after the run has ended.
    pub(super) fn batch_views(
        &self,
        ids: &[MachineId],
        now: Timestamp,
        upto: Timestamp,
        admission: bool,
    ) -> Result<(BatchBounds, Vec<BatchView>), Error<M::Error>> {
        debug_assert!(!admission || now == upto);
        let mut bounds = BTreeMap::new();
        let mut views = Vec::with_capacity(ids.len());
        for &id in ids {
            let (view, lower) = self.batch_view(id, now, upto, admission)?;
            bounds.insert(id, (view.1, view.2, view.3.len(), lower));
            views.push(view);
        }
        Ok((bounds, views))
    }

    /// Inputs and bounds for one machine, also used directly by a lane, and
    /// the simulation time its machine times map from. `admission` is as
    /// for [`Self::batch_views`]. Its inbox is not a timed one; a lane makes
    /// it one and adds the inputs dated inside the run.
    pub(super) fn batch_view(
        &self,
        id: MachineId,
        now: Timestamp,
        upto: Timestamp,
        admission: bool,
    ) -> Result<(BatchView, Timestamp), Error<M::Error>> {
        let entry = &self.execution.machines[id.0];
        let start = if admission {
            entry.machine_time(id, now)?
        } else {
            entry.start_time(id, now)?
        };
        let bound = entry.machine_time(id, upto)?;
        let anchor = Timestamp::from_nanos(entry.time.anchor());
        // A run that continues from a barrier nothing needs, with nothing
        // delivered, maps from the machine's last shared cut; any other run
        // starts at a shared cut.
        let back = self.execution.network.latency_ns().saturating_sub(1);
        let limit = Timestamp::from_nanos(now.as_nanos().saturating_sub(back));
        let lower = entry.lower_for(now, limit, self.execution.needed == Some(now));
        let inbox = entry
            .inbox
            .iter()
            .map(|input| {
                Ok(Input {
                    at: entry.machine_time(id, input.at.max(anchor))?.min(start),
                    payload: input.payload.clone(),
                })
            })
            .collect::<Result<VecDeque<_>, ClockOverflow>>()?;
        Ok(((id, start, bound, inbox, false), lower))
    }

    /// Run every view and return each machine and its RNG to its entry,
    /// removing the inputs the run accepted. Reports are ordered by identity.
    fn execute_batch(
        &mut self,
        views: Vec<BatchView>,
        bounds: &BatchBounds,
    ) -> Vec<RunReport<M::Error>> {
        let mut results: Vec<RunResult<M>> = if views.iter().any(|v| self.splits(v.0)) {
            let mut round = round::Runs::new();
            let count = views.len();
            for view in views {
                self.submit_run(view, &mut round);
            }
            (0..count).map(|_| self.next_run(&mut round)).collect()
        } else {
            views.into_iter().map(|view| self.run_here(view)).collect()
        };
        results.sort_by_key(|result| result.id);
        results
            .into_iter()
            .map(|result| {
                let offered = bounds[&result.id].2;
                self.receive_run(result, offered)
            })
            .collect()
    }

    /// Take one view's machine and RNG from its entry for a run.
    fn take_flight(&mut self, (id, start, bound, inbox, timed): BatchView) -> Flight<M> {
        let entry = &mut self.execution.machines[id.0];
        Flight {
            id,
            machine: entry.machine.take().expect("stopped machine"),
            rng: entry.rng.take().expect("stopped machine RNG"),
            inbox,
            outputs: Vec::new(),
            start,
            bound,
            timed,
            execution: entry.lifecycle.execution(),
        }
    }

    /// Run one view's machine on this thread until it stops.
    pub(super) fn run_here(&mut self, view: BatchView) -> RunResult<M> {
        let mut flight = self.take_flight(view);
        let bound = flight.bound;
        let stopped = flight.call(self.host.context(), |machine, context| {
            machine.run(context, bound)
        });
        flight.result(stopped)
    }

    /// Whether machine `id` executes elsewhere, so that the Driver's thread
    /// can keep its runs in flight beside others' instead of running it inline.
    pub(super) fn splits(&mut self, id: MachineId) -> bool {
        self.execution.machines[id.0]
            .machine
            .as_mut()
            .and_then(M::split_run)
            .is_some()
    }

    /// Start one view's run on this thread and keep it in flight unless it
    /// finished at once.
    pub(super) fn start_flight(&mut self, view: BatchView) -> Started<M> {
        let mut flight = self.take_flight(view);
        let bound = flight.bound;
        match flight.call(self.host.context(), |machine, context| {
            let split = machine.split_run().expect("split run");
            split.start(context, bound)
        }) {
            Ok(None) => Started::Flying(flight),
            Ok(Some(stopped)) => Started::Finished(flight.result(Ok(stopped))),
            Err(error) => Started::Finished(flight.result(Err(error))),
        }
    }

    /// Return a finished run's machine and RNG to its entry and remove the
    /// inputs it accepted out of the `offered` ones.
    pub(super) fn receive_run(
        &mut self,
        result: RunResult<M>,
        offered: usize,
    ) -> RunReport<M::Error> {
        let RunResult {
            id,
            machine,
            rng,
            leftover,
            outputs,
            stopped,
        } = result;
        let remaining = leftover.len();
        let entry = &mut self.execution.machines[id.0];
        entry.machine = Some(machine);
        entry.rng = Some(rng);
        // Accepted inputs are the taken prefix; the rest stay in simulation time.
        let taken = offered.saturating_sub(remaining).min(entry.inbox.len());
        let admitted_end = entry
            .inbox
            .iter()
            .take(taken)
            .any(|input| matches!(input.payload, Request::Crash | Request::Reboot));
        entry.inbox.drain(..taken);
        RunReport {
            id,
            outputs,
            stopped,
            admitted_end,
        }
    }

    /// Validate one run against its machine-time `start` and `bound`, and
    /// convert its stop and outputs back to simulation time: each output and
    /// a blocked or exited position to the earliest simulation time at or
    /// after `lower` that reaches it, a timer to the earliest at or after
    /// `now`, the run's start.
    pub(super) fn accept_run(
        &self,
        report: RunReport<M::Error>,
        start: Timestamp,
        bound: Timestamp,
        lower: Timestamp,
        now: Timestamp,
        upto: Timestamp,
    ) -> Result<AcceptedRun, Error<M::Error>> {
        let RunReport {
            id,
            outputs,
            stopped: result,
            admitted_end,
        } = report;
        let stopped = result?;
        let entry = &self.execution.machines[id.0];
        let last_ended = boot_end::validate(
            &outputs,
            entry.lifecycle.boot(),
            entry.last_ended_published,
            entry.stopped,
            stopped,
            admitted_end,
        )
        .map_err(|detail| Error::InvalidBootEnd { id, detail })?;
        let reached = match stopped {
            StopReason::BoundaryReached => bound,
            StopReason::Blocked { reached, deadline } => {
                if deadline.is_some_and(|at| at <= reached.max(start)) {
                    return Err(Error::InvalidStop { id, stopped });
                }
                reached
            }
            StopReason::Exited { reached, .. } => reached,
        };
        if reached < entry.position.max(start) || reached > bound {
            return Err(Error::InvalidStop { id, stopped });
        }
        if let Some(output) = outputs.iter().find(|output| output.at > reached) {
            return Err(Error::InvalidOutput {
                id,
                at: output.at,
                reached,
            });
        }
        if outputs.iter().any(|output| {
            matches!(&output.payload, Response::TransmitFrame { bytes }
                if !self.execution.network.attached(id) || !crate::pv::frame_in_bounds(bytes))
        }) {
            return Err(Error::InvalidFrame { id });
        }
        let simulation = |at| entry.simulation_time(lower, at).ok_or(ClockOverflow(id));
        let (converted, timer) = match stopped {
            StopReason::BoundaryReached => (StopReason::BoundaryReached, None),
            StopReason::Blocked { deadline, .. } => (
                StopReason::Blocked {
                    reached: simulation(reached)?,
                    // A timer no simulation time can reach never fires.
                    deadline: deadline.and_then(|at| entry.simulation_time(now, at)),
                },
                deadline,
            ),
            StopReason::Exited { reason, .. } => (
                StopReason::Exited {
                    reached: simulation(reached)?,
                    reason,
                },
                None,
            ),
        };
        let outputs = outputs
            .into_iter()
            .map(|output| {
                Ok(crate::pv::Output {
                    at: simulation(output.at)?,
                    payload: output.payload,
                })
            })
            .collect::<Result<Vec<_>, ClockOverflow>>()?;
        let simulated = match converted {
            StopReason::BoundaryReached => upto,
            StopReason::Blocked { reached, .. } | StopReason::Exited { reached, .. } => reached,
        };
        debug_assert!(simulated >= lower && simulated <= upto);
        Ok(AcceptedRun {
            id,
            reached: simulated,
            stopped: converted,
            outputs,
            position: reached,
            timer,
            last_ended,
        })
    }

    /// Confirm every offered guest command the batch consumed from its inbox,
    /// at the time it was offered.
    pub(super) fn confirm_offered_commands(
        &mut self,
        offered: &[(MachineId, NonZeroU32, Timestamp)],
        rebooting: Option<u64>,
    ) -> Result<(), Error<M::Error>> {
        for &(id, command, at) in offered {
            let entry = &mut self.execution.machines[id.0];
            if !entry.inbox.iter().any(|input| {
                control::command(&input.payload).is_some_and(|(pending, _)| pending == command)
            }) {
                let boot = Self::batch_boot(entry, rebooting);
                self.execution
                    .control
                    .accepted(id, &mut entry.lifecycle, boot, command, at)
                    .map_err(Error::Control)?;
            }
        }
        Ok(())
    }

    /// Record accepted runs: consumed oracle stops, progress, the pending
    /// timer, and staged outputs.
    pub(super) fn commit_runs(
        &mut self,
        accepted: impl IntoIterator<Item = AcceptedRun>,
        offered_stops: &[NonZeroU32],
        rebooting: Option<u64>,
    ) {
        for AcceptedRun {
            id,
            reached,
            stopped,
            outputs,
            position,
            timer,
            last_ended,
        } in accepted
        {
            let entry = &mut self.execution.machines[id.0];
            let boot = Self::batch_boot(entry, rebooting);
            if id == self.execution.oracle {
                for &command in offered_stops {
                    let unconsumed = entry.inbox.iter().any(|input| matches!(input.payload,
                        Request::Application { id, command: crate::pv::ApplicationCommand::Stop } if id == command));
                    if !unconsumed {
                        self.execution.oracle_stops.insert((boot, command));
                    }
                }
            }
            entry.reached = reached;
            entry.stopped = stopped;
            entry.position = position;
            entry.timer = timer;
            entry.last_ended_published = last_ended;
            entry
                .staged
                .extend(outputs.into_iter().map(|output| MachineResponse {
                    machine: id,
                    boot,
                    at: output.at,
                    payload: output.payload,
                }));
            if matches!(stopped, StopReason::Exited { .. }) {
                // Only admitted work belongs to this ended boot. Future
                // requests remain ordered in the driver timeline.
                entry.inbox.clear();
            }
        }
    }
}
