//! One retained worker owns one Driver. This is service coordination, not a
//! second scheduler: every positive or zero-time effect is delegated to Driver.
mod run;
#[cfg(test)]
mod tests;
use super::{
    checkpoint::{self, Capture},
    events::Publisher,
};
use crate::{
    execution::{ExecutionReport, StopReason, WorkloadExit},
    store::ContentDigest,
    Error, Result,
};
use seedbox_driver::driver::{
    self, Driver, DueOutcome, FaultOrigin, FaultRecord, InputKey, PreparedInput, PreparedOutcome,
    StepOutcome,
};
use seedbox_engine_protocol::{
    driver_world::{Inventory, State},
    host_control::{self as host, Command, ControlError, Reply, Request},
    intervention::Intervention,
};
use seedbox_machine::machine::{ExitReason, Machine, Timestamp};
#[cfg(test)]
use seedbox_machine::pv::Response;
use std::{
    collections::{BTreeMap, VecDeque},
    fs::File,
    io::BufWriter,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Instant,
};

fn fail(message: impl Into<String>) -> Error {
    Error::Execution(message.into())
}
pub(super) type Constructed = Arc<Mutex<VecDeque<String>>>;
struct Prepared {
    command: Command,
    input: PreparedInput,
    intervention: Intervention,
}
pub(super) struct Worker<M: Machine> {
    pub(super) driver: Driver<M>,
    pub(super) state: State,
    pub(super) inventory: Inventory,
    pub(super) output: PathBuf,
    pub(super) names: BTreeMap<driver::MachineId, String>,
    pub(super) constructed: Constructed,
    publisher: Publisher,
    prepared: Option<Prepared>,
    checkpoint: Option<host::CheckpointStatus>,
    checkpoint_refusal: Option<String>,
    terminal: bool,
    failed: bool,
    measurement: Option<(u64, Instant)>,
    write_snapshot: fn(M::Snapshot, &mut BufWriter<File>) -> Result<()>,
}
#[derive(Default)]
pub(super) struct Facts {
    pub admissions: Vec<FaultRecord>,
    pub rejections: Vec<(InputKey, String)>,
}
pub(super) enum Dispatch {
    Reply(Reply),
    Semantic(Request),
}
impl<M> Worker<M>
where
    M: Machine + Send + 'static,
    M::Context: 'static,
    M::Error: Send + std::fmt::Display + 'static,
{
    #[allow(clippy::too_many_arguments)]
    pub(super) fn new(
        mut driver: Driver<M>,
        state: State,
        inventory: Inventory,
        output: PathBuf,
        names: BTreeMap<driver::MachineId, String>,
        constructed: Constructed,
        observations: Option<&Path>,
        write_snapshot: fn(M::Snapshot, &mut BufWriter<File>) -> Result<()>,
    ) -> Result<Self> {
        // Observation is host-owned even when the Driver came from a capture.
        // Do not retain frame bodies/facts for non-publishing callers.
        driver.set_frame_capacity(if observations.is_some() { 1024 } else { 0 });
        let mut state = state;
        state.world.publication.session = Default::default();
        Ok(Self {
            driver,
            state,
            inventory,
            output,
            names,
            constructed,
            publisher: Publisher::connect(observations)?,
            prepared: None,
            checkpoint: None,
            checkpoint_refusal: None,
            terminal: false,
            failed: false,
            measurement: None,
            write_snapshot,
        })
    }
    pub(super) fn flush(&mut self) -> Result<Facts> {
        if self.failed {
            return Err(fail(
                "Driver worker is unusable after a prior execution/publication failure",
            ));
        }
        let result = (|| {
            let mut facts = Facts::default();
            let mut batch = self.driver.drain_observations();
            for committed in &batch.changes {
                match &committed.change {
                    driver::CommittedChange::MachineAdded { machine, .. } => {
                        let name = self
                            .constructed
                            .lock()
                            .map_err(|_| fail("Driver construction ledger poisoned"))?
                            .pop_front()
                            .ok_or_else(|| {
                                fail("Driver committed machine has no factory result")
                            })?;
                        if self.names.insert(*machine, name.clone()).is_some() {
                            return Err(fail("Driver machine identity committed twice"));
                        }
                        self.state.world.machine_names.push(name);
                    }
                    driver::CommittedChange::Ready { machine, .. } => {
                        let name = self
                            .names
                            .get(machine)
                            .ok_or_else(|| fail("Driver ready fact has no machine binding"))?;
                        self.state.world.readiness.observe(name);
                    }
                    driver::CommittedChange::FaultAdmitted(record) => {
                        facts.admissions.push(record.clone())
                    }
                    driver::CommittedChange::InputRejected { key, rejection } => facts
                        .rejections
                        .push((key.clone(), rejection.detail.clone())),
                    driver::CommittedChange::ApplicationOutcome { machine, code, .. }
                        if self.names.get(machine) == self.state.world.machine_names.first() =>
                    {
                        self.state.world.publication.oracle_exit = Some(*code);
                    }
                    driver::CommittedChange::FaultSettled {
                        record,
                        status,
                        outcome_at,
                        detail,
                        ..
                    } if *status == driver::SettlementStatus::Applied => {
                        let (machine, _) =
                            super::events::target_machine(&self.names, &record.target)?;
                        self.state.world.publication.session.applied_faults.push(
                            seedbox_engine_protocol::driver_world::AppliedFault {
                                requested_at_ns: record.requested_at.as_nanos(),
                                applied_at_ns: outcome_at.unwrap_or(committed.at).as_nanos(),
                                service: machine.unwrap_or("").into(),
                                action: super::events::action_name(record.action)?.into(),
                                detail: detail.clone(),
                            },
                        );
                    }
                    _ => {}
                }
            }
            batch.changes.clear(); // Required bookkeeping above consumed this prefix.
            self.publisher.batch(&mut self.state, &self.names, batch)?;
            Ok(facts)
        })();
        if result.is_err() {
            self.failed = true;
            self.terminal = true;
        }
        result
    }
    pub(super) fn playback(&mut self) -> Result<driver::PlaybackStatus> {
        self.driver
            .with_barrier(|barrier| Ok(barrier.playback()))
            .map_err(|e| fail(e.to_string()))
    }
    pub(super) fn status(&mut self, id: u64) -> Result<Reply> {
        if self.failed {
            return Err(fail(
                "Driver worker is unusable after a prior execution/publication failure",
            ));
        }
        let playback = self.playback()?;
        self.state.world.position = position(playback.position);
        let achieved_rate = self.measurement.and_then(|(at, host)| {
            std::num::NonZeroU64::new(u64::try_from(host.elapsed().as_nanos()).unwrap_or(u64::MAX))
                .map(|wall_time_ns| host::RateMeasurement {
                    simulated_time_ns: playback.position.at.as_nanos().saturating_sub(at),
                    wall_time_ns,
                })
        });
        Ok(Reply {
            id,
            position: self.state.world.position,
            terminal: self.terminal,
            application_ready: self.state.world.readiness.latched,
            rate: rate_to_wire(playback.rate),
            achieved_rate,
            stepping: !self.terminal && playback.stepping,
            error: None,
            intervention: self
                .prepared
                .as_ref()
                .map(|scope| scope.intervention.clone()),
            checkpoint: self.checkpoint.clone(),
            detail: None,
        })
    }
    /// Route playback/checkpoint transport commands here; semantic requests are
    /// returned intact for the concrete fault adapter, never silently rejected
    /// or reinterpreted as another kind of control.
    pub(super) fn playback_request(&mut self, request: Request) -> Result<Dispatch> {
        let mut reply = self.status(request.id)?;
        if self.terminal && request.command != Command::Status {
            reply.error = Some(ControlError::Completed);
            return Ok(Dispatch::Reply(reply));
        }
        if self.prepared.is_some()
            && !matches!(
                request.command,
                Command::Status | Command::PrepareFault { .. } | Command::CommitFault { .. }
            )
        {
            reply.error = Some(ControlError::FaultPending);
            return Ok(Dispatch::Reply(reply));
        }
        if self
            .checkpoint
            .as_ref()
            .is_some_and(|c| c.state == host::CheckpointState::Requested)
            && !matches!(
                request.command,
                Command::Status | Command::Checkpoint { .. }
            )
        {
            reply.error = Some(ControlError::CheckpointPending);
            return Ok(Dispatch::Reply(reply));
        }
        let result = match &request.command {
            Command::Status => {
                if let Some(detail) = &self.checkpoint_refusal {
                    reply.error = Some(ControlError::InvalidCheckpoint);
                    reply.detail = Some(detail.clone());
                }
                return Ok(Dispatch::Reply(reply));
            }
            Command::SetRate { rate } => self
                .driver
                .with_barrier(|barrier| barrier.set_rate(rate_from_wire(*rate))),
            Command::PauseBeforeNextFault { rate } => self
                .driver
                .with_barrier(|barrier| barrier.pause_before_next_fault(rate_from_wire(*rate))),
            Command::Step => self.driver.with_barrier(|barrier| barrier.step_quantum()),
            Command::Checkpoint { request_id } => {
                let playback = self.playback()?;
                if self
                    .checkpoint
                    .as_ref()
                    .is_some_and(|old| old.request_id == *request_id)
                {
                    return Ok(Dispatch::Reply(reply));
                }
                let error = if playback.rate != driver::PlaybackRate::Paused {
                    Some(ControlError::NotPaused)
                } else if !self.state.world.readiness.latched {
                    Some(ControlError::NotReady)
                } else if !valid_request_id(request_id) {
                    Some(ControlError::InvalidCheckpoint)
                } else {
                    None
                };
                if let Some(error) = error {
                    reply.error = Some(error)
                } else {
                    self.checkpoint_refusal = None;
                    self.checkpoint = Some(host::CheckpointStatus {
                        request_id: request_id.clone(),
                        position: reply.position,
                        state: host::CheckpointState::Requested,
                    });
                    reply.checkpoint = self.checkpoint.clone();
                }
                return Ok(Dispatch::Reply(reply));
            }
            Command::PrepareFault { .. } | Command::CommitFault { .. } => {
                return Ok(Dispatch::Semantic(request))
            }
        };
        match result {
            Ok(()) => {
                if matches!(
                    request.command,
                    Command::SetRate { .. } | Command::PauseBeforeNextFault { .. }
                ) {
                    self.measurement =
                        Some((self.state.world.position.simulated_time_ns, Instant::now()));
                }
                self.flush()?;
                self.status(request.id).map(Dispatch::Reply)
            }
            Err(error) => {
                reply.error = Some(control_error(&error));
                reply.detail = Some(error.to_string());
                Ok(Dispatch::Reply(reply))
            }
        }
    }
    pub(super) fn capture_pending(
        &mut self,
    ) -> checkpoint::CaptureResult<Option<host::CheckpointStatus>> {
        if self.failed {
            return Err(checkpoint::CaptureFailure::Publication(fail(
                "Driver worker is unusable after publication failure",
            )));
        }
        let Some(checkpoint) = self
            .checkpoint
            .clone()
            .filter(|c| c.state == host::CheckpointState::Requested)
        else {
            return Ok(None);
        };
        self.flush()
            .map_err(checkpoint::CaptureFailure::Publication)?;
        let names = &self.names;
        let publisher = &mut self.publisher;
        let result = checkpoint::capture(
            &mut self.driver,
            Capture {
                inventory: &self.inventory,
                state: &mut self.state,
                prepared: self.prepared.is_some(),
            },
            &self.output,
            &checkpoint.request_id,
            |state, batch| publisher.batch(state, names, batch),
            self.write_snapshot,
        );
        if let Err(error) = result {
            // Refusal is returned once, never automatically retried by the
            // boundary loop. Lost publication state is separately terminal.
            self.checkpoint = None;
            if matches!(error, checkpoint::CaptureFailure::Refused(_)) {
                self.checkpoint_refusal = Some(error.to_string());
            }
            if matches!(error, checkpoint::CaptureFailure::Publication(_)) {
                self.terminal = true;
                self.failed = true;
            }
            return Err(error);
        }
        let captured = host::CheckpointStatus {
            state: host::CheckpointState::Captured,
            ..checkpoint
        };
        self.checkpoint = Some(captured.clone());
        Ok(Some(captured))
    }
    /// Check the worker-only gates before asking the Driver to prepare. The
    /// actual named admission stays in Driver, not in a service handle shim.
    pub(super) fn may_prepare(&mut self) -> Result<Option<ControlError>> {
        if self.terminal {
            return Ok(Some(ControlError::Completed));
        }
        if self.prepared.is_some() {
            return Ok(Some(ControlError::FaultPending));
        }
        if self
            .checkpoint
            .as_ref()
            .is_some_and(|c| c.state == host::CheckpointState::Requested)
        {
            return Ok(Some(ControlError::CheckpointPending));
        }
        if !self.state.world.readiness.latched {
            return Ok(Some(ControlError::NotReady));
        }
        if self.playback()?.before_fault.is_some() {
            return Ok(Some(ControlError::Unsupported));
        }
        if self
            .state
            .world
            .input_program
            .inputs
            .get(self.state.world.input_program.next)
            .is_some_and(|input| input.at_ns <= self.state.world.position.simulated_time_ns)
        {
            return Ok(Some(ControlError::InvalidFault));
        }
        Ok(None)
    }
    /// Translate the caller's action without resolving bounds or aliases, then
    /// obtain the stopped Driver proposal. An exact retry reuses that proposal.
    pub(super) fn prepare_request(&mut self, request: &Request) -> Result<Reply> {
        let Command::PrepareFault {
            request_id,
            action,
            timeout_ns,
        } = &request.command
        else {
            return Err(fail("invalid Driver prepare request"));
        };
        if let Some(scope) = &self.prepared {
            let same = scope.command == request.command;
            let mut reply = self.status(request.id)?;
            if !same {
                reply.error = Some(ControlError::FaultPending);
            }
            return Ok(reply);
        }
        if let Some(error) = self.may_prepare()? {
            let mut reply = self.status(request.id)?;
            reply.error = Some(error);
            return Ok(reply);
        }
        let proposal = (|| -> std::result::Result<PreparedInput, String> {
            let (action, handle) = super::actions::translate(action, *timeout_ns)?;
            let key = InputKey::new(request_id.clone()).map_err(|error| error.to_string())?;
            self.driver
                .with_barrier(|barrier| match handle {
                    Some(handle) => barrier.prepare_input_with_handle(key, action, handle),
                    None => barrier.prepare_input(key, action),
                })
                .map_err(|error| error.to_string())
        })();
        match proposal {
            Ok(input) => self.prepared(request, input),
            Err(detail) => {
                let mut reply = self.status(request.id)?;
                reply.error = Some(ControlError::InvalidFault);
                reply.detail = Some(detail);
                Ok(reply)
            }
        }
    }
    /// Retain the Driver's plain non-mutating proposal while the external
    /// server journals its exact Intervention. No machine or Driver mutation.
    pub(super) fn prepared(&mut self, request: &Request, input: PreparedInput) -> Result<Reply> {
        let Command::PrepareFault {
            request_id,
            action,
            timeout_ns,
        } = &request.command
        else {
            return Err(fail("invalid Driver prepare dispatch"));
        };
        let mut reply = self.status(request.id)?;
        if let Some(scope) = &self.prepared {
            if scope.command == request.command {
                reply.intervention = Some(scope.intervention.clone());
                return Ok(reply);
            }
            reply.error = Some(ControlError::FaultPending);
            return Ok(reply);
        }
        if let Some(error) = self.may_prepare()? {
            reply.error = Some(error);
            return Ok(reply);
        }
        if input.position() != self.playback()?.position || input.key().as_str() != request_id {
            return Err(fail(
                "Driver proposal does not match current service position/key",
            ));
        }
        let record = match input.outcome() {
            PreparedOutcome::Admitted(record) => record,
            PreparedOutcome::Rejected(reason) => {
                reply.error = Some(ControlError::InvalidFault);
                reply.detail = Some(reason.detail.clone());
                return Ok(reply);
            }
        };
        let at = reply.position.simulated_time_ns;
        let intervention = Intervention {
            request_id: request_id.clone(),
            position: reply.position,
            plan: seedbox_control::Plan {
                id: record.fault.get(),
                at_ns: at,
                deadline_ns: at
                    .checked_add(*timeout_ns)
                    .ok_or_else(|| fail("Driver intervention deadline overflow"))?,
                action: action.clone(),
            },
        };
        let mut check = reply.clone();
        check.intervention = Some(intervention.clone());
        if serde_json::to_vec(&check)?.len() > host::MAX_DATAGRAM {
            return Err(fail(
                "Driver intervention proposal exceeds control datagram limit",
            ));
        }
        self.prepared = Some(Prepared {
            command: request.command.clone(),
            input,
            intervention: intervention.clone(),
        });
        reply.intervention = Some(intervention);
        Ok(reply)
    }
    pub(super) fn commit(&mut self, request: &Request) -> Result<Reply> {
        let Command::CommitFault { request_id } = &request.command else {
            return Err(fail("invalid Driver commit dispatch"));
        };
        let Some(scope) = self.prepared.take() else {
            let mut reply = self.status(request.id)?;
            reply.error = Some(ControlError::NoFaultPrepared);
            return Ok(reply);
        };
        if scope.intervention.request_id != *request_id {
            self.prepared = Some(scope);
            let mut reply = self.status(request.id)?;
            reply.error = Some(ControlError::NoFaultPrepared);
            return Ok(reply);
        }
        // Stage the source definition for observation-plan projection, but do
        // not advance its consumed cursor before successful exact admission.
        let at = scope.intervention.plan.at_ns;
        let program = &mut self.state.world.input_program;
        if program.inputs.iter().any(|input| input.key == *request_id)
            || program
                .inputs
                .get(program.next)
                .is_some_and(|input| input.at_ns <= at)
        {
            return Err(fail(
                "retained live intervention conflicts with authored program",
            ));
        }
        program.inputs.insert(
            program.next,
            seedbox_engine_protocol::input_program::TimedInput {
                key: request_id.clone(),
                at_ns: at,
                timeout_ns: scope.intervention.plan.deadline_ns - at,
                action: scope.intervention.plan.action.clone(),
            },
        );
        let result = self
            .driver
            .with_barrier(|barrier| barrier.commit_input(&scope.input));
        let facts = self.flush()?;
        let due = match result {
            Ok(outcome) => outcome,
            Err(error) => {
                self.failed = true;
                self.terminal = true;
                return Err(fail(format!(
                    "retained Driver intervention failed: {error}"
                )));
            }
        };
        let expected = match scope.input.outcome() {
            PreparedOutcome::Admitted(record) => record,
            PreparedOutcome::Rejected(_) => {
                return Err(fail("cannot commit a rejected Driver proposal"))
            }
        };
        let actual:Vec<_>=facts.admissions.iter().filter(|record|matches!(&record.origin,FaultOrigin::Input{key}if key.as_str()==request_id)).collect();
        if actual.as_slice() != [expected] || expected.fault.get() != scope.intervention.plan.id {
            return Err(fail(
                "Driver committed admission differs from prepared retained Intervention",
            ));
        }
        self.finish_due(due)?;
        self.state.world.input_program.admissions.push(
            seedbox_engine_protocol::input_program::InputAdmission {
                key: request_id.clone(),
                plan: scope.intervention.plan.clone(),
            },
        );
        self.state.world.input_program.next += 1;
        self.state.world.intervention_next = self
            .state
            .world
            .intervention_next
            .checked_add(1)
            .ok_or_else(|| fail("Driver intervention cursor exhausted"))?;
        self.state.world.input_program.validate().map_err(fail)?;
        let mut reply = self.status(request.id)?;
        reply.intervention = Some(scope.intervention);
        Ok(reply)
    }
    /// Translate only the next entry. Future inputs stay exclusively in the
    /// editable service suffix until their own due barrier.
    pub(super) fn authored_next(&mut self) -> Result<bool> {
        let Some(input) = self
            .state
            .world
            .input_program
            .inputs
            .get(self.state.world.input_program.next)
        else {
            return Ok(false);
        };
        let (action, handle) =
            super::actions::translate(&input.action, input.timeout_ns).map_err(fail)?;
        self.authored(action, handle)
    }
    /// Deliver exactly one authored entry at its due barrier. No future action
    /// is queued, so an editable checkpoint suffix has no hidden Driver copy.
    /// Driver owns admission, handle allocation, FIFO effects and rejection.
    pub(super) fn authored(
        &mut self,
        action: driver::Action,
        handle: Option<driver::FaultHandle>,
    ) -> Result<bool> {
        if self.failed || self.prepared.is_some() {
            return Err(fail(
                "authored input cannot overtake failed/prepared worker state",
            ));
        }
        let Some(input) = self
            .state
            .world
            .input_program
            .inputs
            .get(self.state.world.input_program.next)
            .cloned()
        else {
            return Ok(false);
        };
        let at = self.playback()?.position.at;
        if input.at_ns != at.as_nanos() {
            return Err(fail(
                "authored input must be delivered exactly at its due barrier",
            ));
        }
        let result = (|| {
            // Existing guest/trigger work due at this barrier precedes a newly
            // supplied authored entry, matching the live admission boundary.
            let due = self
                .driver
                .with_barrier(|barrier| barrier.apply_due_now())
                .map_err(|error| fail(error.to_string()))?;
            self.flush()?;
            let ready = matches!(due, DueOutcome::Applied { .. });
            self.finish_due(due)?;
            if !ready {
                return Ok(false);
            }
            let key = InputKey::new(input.key.clone()).map_err(|error| fail(error.to_string()))?;
            self.driver
                .with_barrier(|barrier| match handle {
                    Some(handle) => barrier.schedule_with_handle(at, key.clone(), action, handle),
                    None => barrier.schedule(at, key.clone(), action),
                })
                .map_err(|error| fail(error.to_string()))?;
            let due = self
                .driver
                .with_barrier(|barrier| barrier.apply_due_now())
                .map_err(|error| fail(error.to_string()))?;
            let facts = self.flush()?;
            let admissions: Vec<_> = facts.admissions.iter().filter(|record| matches!(&record.origin, FaultOrigin::Input { key: admitted } if admitted == &key)).collect();
            let rejections: Vec<_> = facts
                .rejections
                .iter()
                .filter(|(rejected, _)| rejected == &key)
                .collect();
            match (admissions.as_slice(), rejections.as_slice()) {
                ([record], []) => self.state.world.input_program.admissions.push(
                    seedbox_engine_protocol::input_program::InputAdmission {
                        key: input.key.clone(),
                        plan: seedbox_control::Plan {
                            id: record.fault.get(),
                            at_ns: input.at_ns,
                            deadline_ns: input
                                .at_ns
                                .checked_add(input.timeout_ns)
                                .ok_or_else(|| fail("authored deadline overflow"))?,
                            action: input.action,
                        },
                    },
                ),
                ([], [(_, detail)]) => self.state.world.input_program.rejections.push(
                    seedbox_engine_protocol::input_program::InputRejection {
                        key: input.key,
                        at_ns: input.at_ns,
                        reason: detail.clone(),
                    },
                ),
                _ => {
                    return Err(fail(
                        "authored input has no unique committed admission/rejection",
                    ))
                }
            }
            self.state.world.input_program.next += 1;
            self.state
                .world
                .input_program
                .validate_at(at.as_nanos())
                .map_err(fail)?;
            self.finish_due(due)?;
            Ok(true)
        })();
        if result.is_err() {
            self.failed = true;
            self.terminal = true;
        }
        result
    }
    pub(super) fn finish_due(&mut self, outcome: DueOutcome) -> Result<()> {
        if let DueOutcome::Finished { at, .. } = outcome {
            // The already-finished fast path clears pending step/arm without
            // running machines or changing time; due-only dispatch preserves it.
            let terminal = self.driver.step(at).map_err(|e| fail(e.to_string()))?;
            self.observe_terminal(&terminal)?;
            if !matches!(terminal, StepOutcome::Finished { .. }) {
                return Err(fail("Driver due-finish changed while finalizing playback"));
            }
            self.terminal = true;
            self.flush()?;
        }
        Ok(())
    }
    pub(super) fn step(&mut self, upto: Timestamp) -> Result<StepOutcome> {
        self.step_selected(upto, false)
    }
    pub(super) fn step_toward_external(&mut self, upto: Timestamp) -> Result<StepOutcome> {
        self.step_selected(upto, true)
    }
    fn step_selected(&mut self, upto: Timestamp, external_work: bool) -> Result<StepOutcome> {
        if self.failed {
            return Err(fail(
                "Driver worker is unusable after a prior execution/publication failure",
            ));
        }
        if self.prepared.is_some() {
            return Err(fail(
                "Driver execution cannot overtake prepared intervention",
            ));
        }
        let result = if external_work {
            self.driver.step_toward_external(upto)
        } else {
            self.driver.step(upto)
        };
        self.flush()?;
        let outcome = match result {
            Ok(outcome) => outcome,
            Err(error) => {
                self.failed = true;
                self.terminal = true;
                return Err(fail(error.to_string()));
            }
        };
        if let Err(error) = self.observe_terminal(&outcome) {
            self.failed = true;
            self.terminal = true;
            return Err(error);
        }
        if matches!(
            outcome,
            StepOutcome::Finished { .. } | StepOutcome::Stopped { .. }
        ) {
            self.terminal = true;
        }
        let status = self.playback()?;
        self.state.world.position = position(status.position);
        Ok(outcome)
    }
    fn observe_terminal(&mut self, outcome: &StepOutcome) -> Result<()> {
        if let StepOutcome::Finished {
            reason: ExitReason::ProgramExit { code },
            ..
        } = outcome
        {
            let code = u8::try_from(*code).map_err(|_| {
                self.failed = true;
                self.terminal = true;
                fail(format!("oracle exit code {code} cannot be represented by the public workload-exit contract"))
            })?;
            if self
                .state
                .world
                .publication
                .oracle_exit
                .is_some_and(|previous| previous != code)
            {
                self.failed = true;
                self.terminal = true;
                return Err(fail(
                    "Driver oracle exit differs from the published application outcome",
                ));
            }
            self.state.world.publication.oracle_exit = Some(code);
        }
        Ok(())
    }
    pub(super) fn execute(
        &mut self,
        until_ns: u64,
        control: &mut super::transport::Control,
    ) -> Result<StopReason> {
        run::execute(self, until_ns, control)
    }
    pub(super) fn quiescent_reason(&mut self) -> Result<StopReason> {
        self.driver
            .with_barrier(|barrier| {
                for (id, _) in barrier.machines() {
                    if barrier.paused(id)?
                        || matches!(
                            barrier.stopped(id)?,
                            seedbox_machine::machine::StopReason::Exited {
                                reason: ExitReason::Crashed,
                                ..
                            }
                        )
                    {
                        return Ok(StopReason::SimulatedTimeLimit);
                    }
                }
                Ok(StopReason::Quiescent)
            })
            .map_err(|e| fail(e.to_string()))
    }
    pub(super) fn report(
        &self,
        reason: StopReason,
        detail: Option<String>,
        witness: Option<ContentDigest>,
        full_state: Option<BTreeMap<String, ContentDigest>>,
    ) -> Result<ExecutionReport> {
        let publication = &self.state.world.publication;
        Ok(ExecutionReport {
            host_wait_ns: 0,
            simulated_time_ns: Some(self.state.world.position.simulated_time_ns),
            reason,
            workload_exit: publication.oracle_exit.map(|code| WorkloadExit {
                service: self
                    .state
                    .world
                    .machine_names
                    .first()
                    .cloned()
                    .unwrap_or_default(),
                code,
            }),
            // The event chain is only one evidence component. The concrete
            // native worker supplies validated execution/disk/network evidence;
            // encoded snapshots and observation samples are not substitutes.
            witness,
            full_state,
            faults: publication
                .session
                .applied_faults
                .iter()
                .map(|fault| crate::execution::FaultEvent {
                    requested_at_ns: fault.requested_at_ns,
                    applied_at_ns: fault.applied_at_ns,
                    service: fault.service.clone(),
                    action: fault.action.clone(),
                    detail: fault.detail.clone(),
                })
                .collect(),
            detail,
        })
    }
    pub(super) fn close(&mut self) -> Result<()> {
        self.publisher.close(&mut self.state)
    }
}
pub(super) fn position(position: driver::Position) -> host::BoundaryPosition {
    host::BoundaryPosition {
        boundary_index: position.boundary_index,
        completed_rounds: position.completed_quanta,
        simulated_time_ns: position.at.as_nanos(),
    }
}
pub(super) fn rate_from_wire(rate: host::PlaybackRate) -> driver::PlaybackRate {
    match rate {
        host::PlaybackRate::Paused => driver::PlaybackRate::Paused,
        host::PlaybackRate::Unlimited => driver::PlaybackRate::Unlimited,
        host::PlaybackRate::Limited {
            numerator,
            denominator,
        } => driver::PlaybackRate::Limited {
            numerator,
            denominator,
        },
    }
}
fn rate_to_wire(rate: driver::PlaybackRate) -> host::PlaybackRate {
    match rate {
        driver::PlaybackRate::Paused => host::PlaybackRate::Paused,
        driver::PlaybackRate::Unlimited => host::PlaybackRate::Unlimited,
        driver::PlaybackRate::Limited {
            numerator,
            denominator,
        } => host::PlaybackRate::Limited {
            numerator,
            denominator,
        },
    }
}
fn valid_request_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
}
fn control_error<E>(error: &driver::Error<E>) -> ControlError {
    match error {
        driver::Error::NotPaused => ControlError::NotPaused,
        driver::Error::StepPending => ControlError::StepPending,
        driver::Error::InvalidRate => ControlError::InvalidRate,
        driver::Error::Completed | driver::Error::Stopped => ControlError::Completed,
        driver::Error::BeforeFault => ControlError::Unsupported,
        _ => ControlError::InvalidFault,
    }
}
