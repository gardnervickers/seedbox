//! Batch execution: due machines run on the workers in their own time
//! bases, and accepted runs commit in identity order.
use super::*;

/// Each batch machine's start, bound and offered input count, in its own
/// time base.
type BatchBounds = BTreeMap<MachineId, (Timestamp, Timestamp, usize)>;

/// A batch machine's start, bound and due inputs, in its own time base.
type BatchView = (MachineId, Timestamp, Timestamp, VecDeque<Input>);

/// One machine's finished batch run, still in its own time base.
struct RunReport<E> {
    id: MachineId,
    outputs: Vec<crate::pv::Output>,
    stopped: Result<StopReason, Error<E>>,
    /// Whether the run accepted a Crash or Reboot input.
    admitted_end: bool,
}

/// A validated batch run, converted back to simulation time.
struct AcceptedRun {
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

impl<M> Driver<M>
where
    M: Machine + Send + 'static,
    M::Context: 'static,
    M::Error: Send + 'static,
{
    pub(super) fn run_batch(
        &mut self,
        ids: &[MachineId],
        upto: Timestamp,
    ) -> Result<(), Error<M::Error>> {
        self.run_batch_as(ids, upto, None)
    }

    /// Run a batch. `rebooting` names the operation-local prospective boot of
    /// a single machine's Reboot admission run: its outputs carry that boot,
    /// which `Control::admission` commits only after the run is confirmed.
    pub(super) fn run_batch_as(
        &mut self,
        ids: &[MachineId],
        upto: Timestamp,
        rebooting: Option<u64>,
    ) -> Result<(), Error<M::Error>> {
        if ids.is_empty() {
            return Ok(());
        }
        debug_assert!(rebooting.is_none() || ids.len() == 1);
        let now = self.now();
        let offered_commands = self.offered_commands(ids);
        let offered_stops = self.offered_stops(ids);
        let (bounds, views) = self.batch_views(ids, now, upto)?;
        let reports = self.execute_batch(views, &bounds)?;
        // Validate each stop in its machine's time base, then convert back.
        // The conversion returns the earliest simulation time whose machine
        // time reaches the reported one, so nothing is reported early, and a
        // machine time at most the mapped bound maps to at most `upto`.
        let mut accepted = Vec::with_capacity(reports.len());
        for report in reports {
            let (start, bound, _) = bounds[&report.id];
            accepted.push(self.accept_run(report, start, bound, now, upto)?);
        }
        self.confirm_offered_commands(&offered_commands, upto, rebooting)?;
        self.commit_runs(accepted, &offered_stops, rebooting);
        Ok(())
    }

    /// The boot a batch's outputs and accepted commands belong to.
    fn batch_boot(entry: &Entry<M>, rebooting: Option<u64>) -> u64 {
        rebooting.unwrap_or(entry.lifecycle.boot())
    }

    /// Guest commands queued for the batch's machines, offered to this run.
    fn offered_commands(&self, ids: &[MachineId]) -> Vec<(MachineId, NonZeroU32, Timestamp)> {
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
    fn offered_stops(&self, ids: &[MachineId]) -> Vec<NonZeroU32> {
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
    fn batch_views(
        &self,
        ids: &[MachineId],
        now: Timestamp,
        upto: Timestamp,
    ) -> Result<(BatchBounds, Vec<BatchView>), Error<M::Error>> {
        let mut bounds = BTreeMap::new();
        let mut views = Vec::with_capacity(ids.len());
        for &id in ids {
            let entry = &self.execution.machines[id.0];
            let start = entry.machine_time(id, now)?;
            let bound = entry.machine_time(id, upto)?;
            let anchor = Timestamp::from_nanos(entry.time.anchor());
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
            bounds.insert(id, (start, bound, inbox.len()));
            views.push((id, start, bound, inbox));
        }
        Ok((bounds, views))
    }

    /// Run every view on the workers and return each machine and its RNG to
    /// its entry, removing the inputs the run accepted. Reports are ordered
    /// by identity.
    fn execute_batch(
        &mut self,
        views: Vec<BatchView>,
        bounds: &BatchBounds,
    ) -> Result<Vec<RunReport<M::Error>>, Error<M::Error>> {
        let (send, receive) = mpsc::channel();
        let mut submission_error = None;
        for (id, start, bound, mut inbox) in views {
            let entry = &mut self.execution.machines[id.0];
            let mut machine = entry.machine.take().expect("stopped machine");
            let mut rng = entry.rng.take().expect("stopped machine RNG");
            let execution = entry.lifecycle.execution();
            let mut outputs = Vec::new();
            let result = send.clone();
            if let Err(error) = self.workers.submit(id.0, move |context| {
                let stopped = catch_unwind(AssertUnwindSafe(|| {
                    machine.run(
                        &mut RunContext {
                            worker: context,
                            clock: &FixedClock(start.as_nanos()),
                            rng: &mut rng,
                            execution,
                            inbox: Inbox::new(&mut inbox, start),
                            outbox: Outbox::new(&mut outputs, start, bound),
                        },
                        bound,
                    )
                }))
                .map_err(|_| Error::Panicked { id })
                .and_then(|result| result.map_err(|source| Error::Machine { id, source }));
                let _ = result.send((id, machine, rng, inbox.len(), outputs, stopped));
            }) {
                submission_error = Some(error);
                break;
            }
        }
        drop(send);
        // All submitted jobs must finish before changing the clock or returning.
        // Results are ordered by identity, never by worker completion time.
        let mut results: Vec<_> = receive.into_iter().collect();
        results.sort_by_key(|(id, _, _, _, _, _)| *id);
        let mut reports = Vec::with_capacity(results.len());
        for (id, machine, rng, remaining, outputs, stopped) in results {
            let entry = &mut self.execution.machines[id.0];
            entry.machine = Some(machine);
            entry.rng = Some(rng);
            // Accepted inputs are the taken prefix; the rest stay in simulation time.
            let taken = bounds[&id]
                .2
                .saturating_sub(remaining)
                .min(entry.inbox.len());
            let admitted_end = entry
                .inbox
                .iter()
                .take(taken)
                .any(|input| matches!(input.payload, Request::Crash | Request::Reboot));
            entry.inbox.drain(..taken);
            reports.push(RunReport {
                id,
                outputs,
                stopped,
                admitted_end,
            });
        }
        if let Some(error) = submission_error {
            return Err(error);
        }
        Ok(reports)
    }

    /// Validate one run against its machine-time `start` and `bound`, and
    /// convert its stop and outputs back to simulation time.
    fn accept_run(
        &self,
        report: RunReport<M::Error>,
        start: Timestamp,
        bound: Timestamp,
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
        let simulation = |at| entry.simulation_time(now, at).ok_or(ClockOverflow(id));
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
        debug_assert!(simulated >= now && simulated <= upto);
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

    /// Confirm every offered guest command the batch consumed from its inbox.
    fn confirm_offered_commands(
        &mut self,
        offered: &[(MachineId, NonZeroU32, Timestamp)],
        upto: Timestamp,
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
                    .accepted(id, &mut entry.lifecycle, boot, command, at, upto)
                    .map_err(Error::Control)?;
            }
        }
        Ok(())
    }

    /// Record accepted runs: consumed oracle stops, progress, the pending
    /// timer, and staged outputs.
    fn commit_runs(
        &mut self,
        accepted: Vec<AcceptedRun>,
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
