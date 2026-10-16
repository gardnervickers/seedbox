//! Zero-time service admission; ordinary step and authored FIFO order stay owned
//! by the Driver. No service journal, readiness gate or checkpoint policy here.
use super::*;
#[cfg(test)]
mod seeded_tests;

/// Ordinary control admission: allocated identity/plans, or a policy rejection.
type InputAdmission = Result<(u32, Vec<crate::oracle::service::Plan>), InputRejection>;

#[cfg(test)]
mod admission_value_tests;

/// Result of draining only work due at the current simulation time.
/// Admission/dispatch is not completion of a command awaiting future guest ACKs.
// TODO: Re-evaluate: service receipts need to distinguish an admitted held effect
// and oracle completion without using pacing's Waiting or positive-budget steps.
// Preserve the existing public by-value API and Copy canonical fault action.
#[expect(
    clippy::large_enum_variant,
    reason = "network records retain the existing public by-value outcome API"
)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DueOutcome {
    /// No dispatch work remains due now. Guest acknowledgments may still be pending.
    Applied { at: Timestamp },
    /// This admitted effective fault remains at the FIFO front, unapplied.
    BeforeFault { at: Timestamp, fault: FaultRecord },
    /// Zero-time work committed oracle completion; later work was not dispatched.
    /// Unconsumed step and arm state is preserved. The host may call `step(at)`
    /// at this returned time to perform ordinary finished-state cancellation;
    /// that call performs no machine work.
    Finished { at: Timestamp, reason: ExitReason },
}

/// A non-mutating admission projection, not a queued request or allocated ID.
// TODO: Re-evaluate: the legacy journal must know the exact admitted record before
// committing. A closed plain value separates outer preflight errors from policy
// rejection without introducing tokens, serde identity or a second scheduler.
// No opaque indirection or handle registry belongs in an exact proposal value.
#[expect(
    clippy::large_enum_variant,
    reason = "network records retain the existing public by-value proposal API"
)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PreparedOutcome {
    Admitted(FaultRecord),
    Rejected(InputRejection),
}

/// Owned proposal for a single current-boundary input, with read-only contents.
/// Dropping it does nothing: no request, ID, handle or event was reserved. It
/// carries no authorization or source identity. Commit reprojects the full
/// outcome against current state; callers exclusively retain it across their
/// journal transaction and must not journal rejected outcomes.
// TODO: Re-evaluate: an exclusive prepare/journal/commit scope needs an inspectable
// value, not an opaque token registry. Reprojection detects stale IDs/state before
// effects; position alone is not sufficient. External journaling stays caller-owned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedInput {
    handle: Option<FaultHandle>,
    record_id: Option<crate::oracle::RecordId>,
    key: InputKey,
    action: Action,
    position: Position,
    outcome: PreparedOutcome,
}
impl PreparedInput {
    /// The explicitly requested host name, or None for ordinary pN allocation.
    // TODO: Re-evaluate: aliases are part of exact admission, not a mapping
    // installed after journal commit or machine effects.
    pub fn handle(&self) -> Option<&FaultHandle> {
        self.handle.as_ref()
    }
    /// The proposed host input key.
    pub fn key(&self) -> &InputKey {
        &self.key
    }
    /// The proposed action, unchanged from prepare.
    pub fn action(&self) -> &Action {
        &self.action
    }
    /// The exact committed position at prepare.
    pub fn position(&self) -> Position {
        self.position
    }
    /// The projected admitted record or policy rejection; neither is published.
    pub fn outcome(&self) -> &PreparedOutcome {
        &self.outcome
    }
}

impl<M> Barrier<'_, M>
where
    M: Machine + Send + 'static,
    M::Context: 'static,
    M::Error: Send + 'static,
{
    /// Drain all work already due now, bypassing host pacing only for dispatch.
    ///
    /// Failed, completed, permanently stopped or already-held Drivers refuse
    /// before mutation. Unlike prepare/commit, existing due work is drained, not
    /// refused. The existing FIFO order is unchanged: prequeued dependent inputs
    /// are not reordered ahead of one another's generated effects. A caller that
    /// needs per-entry admission/effect order feeds one entry per drain.
    ///
    /// No positive machine budget, clock or quantum advance occurs. Zero-budget
    /// admission runs, publication, completion and pre-effect holds may occur.
    /// An armed effect uses the ordinary one-shot hold (consuming the arm). An
    /// unarmed effect while Paused with no pending step is held without changing
    /// the rate, pacing anchor, arm or pending step. Paused with a pending step
    /// permits dispatch. Resume or an admitted step releases an effect once.
    /// Unconsumed pacing state is otherwise untouched, including on completion.
    ///
    /// Applied means dispatch work is drained, not that future ACK-dependent
    /// settlements succeeded. Execution/control failures after dispatch begins
    /// poison exactly like step: staged machine output is cleared and confirmed
    /// facts remain drainable. Preflight refusals do not poison or publish.
    // TODO: Re-evaluate: pacing setters change anchors, arms and step targets.
    // An owned zero-time dispatch phase preserves legacy service admission without
    // changing ordinary step or creating a service-side scheduler.
    pub fn apply_due_now(&mut self) -> Result<DueOutcome, Error<M::Error>> {
        self.check_admission_boundary()?;
        let result = self.driver.drain_due_now(Instant::now());
        self.driver.finish_execution(result)
    }

    /// Project one input's exact admission at now without mutating the Driver.
    ///
    /// Refuses failed, completed, stopped, held or already-due work, then checks
    /// the same shape rules as schedule at now. Policy rejection is a successful
    /// PreparedOutcome::Rejected, distinct from these outer errors. Only Control
    /// and live-input bookkeeping are cloned, including pending-notice projection
    /// for Heal(Input). Machines are read-only; no native/factory call, RNG draw
    /// on the source, ID/handle reservation, notice drain or publication occurs.
    /// Dropping a proposal after a journal failure leaves the Driver unchanged.
    /// Readiness, authored cursor, user-handle and checkpoint gates belong to the
    /// exclusive service worker, not this Driver operation.
    // TODO: Re-evaluate: the legacy journal requires state validation and an exact
    // ID before durable write. A shared admission helper on cloned Control avoids
    // speculative real mutation or whole-Driver cloning.
    pub fn prepare_input(
        &self,
        key: InputKey,
        action: Action,
    ) -> Result<PreparedInput, Error<M::Error>> {
        self.prepare_input_as(key, action, None)
    }
    /// Project admission with a nonempty explicit host name. Pending duplicate
    /// text is allowed; an active namespace collision is rejected. Names do not
    /// consume guest pN numbers and participate in exact commit comparison.
    // TODO: Re-evaluate: shared cloned admission is necessary for guest heals,
    // aliases and pending races; a service-only after-commit map cannot do it.
    pub fn prepare_input_with_handle(
        &self,
        key: InputKey,
        action: Action,
        handle: FaultHandle,
    ) -> Result<PreparedInput, Error<M::Error>> {
        self.prepare_input_as(key, action, Some(handle))
    }
    fn prepare_input_as(
        &self,
        key: InputKey,
        action: Action,
        handle: Option<FaultHandle>,
    ) -> Result<PreparedInput, Error<M::Error>> {
        self.check_prepare_boundary()?;
        self.driver
            .check_schedule_shape(self.driver.now(), &key, &action)?;
        Driver::<M>::check_named_action(&action, handle.as_ref())?;
        let mut control = self.driver.control.clone();
        let mut inputs = self.driver.inputs.clone();
        for notice in control.service.take_notices() {
            Driver::<M>::track_input(&mut inputs, &notice);
        }
        let admitted = Driver::<M>::input_admission_with_handle(
            &mut control,
            &self.driver.machines,
            Some(&self.driver.network),
            &inputs,
            &key,
            &action,
            handle.as_ref(),
            self.driver.now(),
        )?;
        let (outcome, record_id) = match admitted {
            Ok((fault, _)) => {
                // An immediately settled admission need not remain pending, so
                // obtain its immutable record from its freshly produced notice.
                let admitted = control
                    .service
                    .take_notices()
                    .into_iter()
                    .find_map(|notice| match notice {
                        crate::oracle::service::Notice::Admitted(a) if a.fault == fault => Some(a),
                        _ => None,
                    })
                    .ok_or_else(|| Error::Control("projected admission lacks its notice".into()))?;
                (
                    PreparedOutcome::Admitted(
                        control
                            .record(
                                admitted.fault,
                                &admitted.requester,
                                &admitted.scope,
                                &admitted.action,
                                &admitted.handle,
                                admitted.requested_at,
                            )
                            .map_err(Error::Control)?,
                    ),
                    admitted.record_id,
                )
            }
            Err(rejection) => (PreparedOutcome::Rejected(rejection), None),
        };
        Ok(PreparedInput {
            handle,
            record_id,
            key,
            action,
            position: self.driver.position(),
            outcome,
        })
    }

    /// Commit a previously admitted projection, then drain its zero-time work.
    ///
    /// Uses prepare's preconditions and refuses a Rejected proposal. Before any
    /// mutation it reprojects the same key/action and compares the exact position
    /// and full outcome (ID, scope/boot, handle, origin and requested time). Any
    /// mismatch returns PreparedInputMismatch unchanged, not a compare-after-effect
    /// safety check. No source hash, registry or journal authorization is implied.
    ///
    /// After this preflight, the request is admitted once through the ordinary
    /// policy and commits its boundary; generated FIFO work is drained under
    /// apply_due_now's hold rule. A defensive real-record mismatch is a Control
    /// error before draining effects, not a preflight PreparedInputMismatch: the
    /// source has already changed. All post-mutation failures poison like step
    /// and must never be retried. External journal failure is handled by not
    /// calling this method; journal rollback/publication is entirely the caller's concern.
    // TODO: Re-evaluate: retaining a fault ID only in a host proposal is unsafe
    // when control state moves. Exact non-mutating reprojection precedes admission;
    // the post-admission guard detects implementation bugs, not stale proposals.
    pub fn commit_input(
        &mut self,
        prepared: &PreparedInput,
    ) -> Result<DueOutcome, Error<M::Error>> {
        self.check_prepare_boundary()?;
        if matches!(prepared.outcome, PreparedOutcome::Rejected(_)) {
            return Err(Error::InvalidRequest);
        }
        let current = self.prepare_input_as(
            prepared.key.clone(),
            prepared.action.clone(),
            prepared.handle.clone(),
        )?;
        if current != *prepared {
            return Err(Error::PreparedInputMismatch);
        }
        let result = (|| {
            let start = self.driver.observations.published.len();
            self.driver
                .inputs
                .insert(prepared.key.clone(), LiveInput::Pending);
            self.driver.admit_input(
                prepared.key.clone(),
                prepared.action.clone(),
                prepared.handle.clone(),
                self.driver.now(),
            )?;
            self.driver.commit_boundary()?;
            if !self.driver.observations.published[start..]
                .iter()
                .any(|event| {
                    matches!((&event.kind, &prepared.outcome),
                    (EventKind::FaultAdmitted(fault), PreparedOutcome::Admitted(expected))
                    if fault == expected)
                })
            {
                // Preflight was exact; divergence here is an implementation bug
                // after mutation. Poison through finish_execution, before effects.
                return Err(Error::Control(
                    "real input admission differs from prepared record".into(),
                ));
            }
            self.driver.drain_due_now(Instant::now())
        })();
        self.driver.finish_execution(result)
    }

    fn check_admission_boundary(&self) -> Result<(), Error<M::Error>> {
        self.driver.check_valid()?;
        self.check_mutation()
    }
    fn check_prepare_boundary(&self) -> Result<(), Error<M::Error>> {
        self.check_admission_boundary()?;
        if self.driver.has_due_work() {
            Err(Error::DueWorkPending)
        } else {
            Ok(())
        }
    }
}

impl<M> Driver<M>
where
    M: Machine + Send + 'static,
    M::Context: 'static,
    M::Error: Send + 'static,
{
    /// Same dispatch owners/order as apply_due: timeline, frames, eligible timers.
    /// CPU-runnable machines alone are not zero-time work.
    fn has_due_work(&self) -> bool {
        let now = self.now();
        self.events.first_key_value().is_some_and(|(at, events)| *at <= now && !events.is_empty())
            || self.network.next_arrival().is_some_and(|at| at <= now.as_nanos())
            || self.machines.iter().any(|entry| {
                !entry.lifecycle.paused()
                    && matches!(entry.stopped, StopReason::Blocked { deadline: Some(at), .. } if at == now)
            })
    }

    fn drain_due_now(&mut self, host_now: Instant) -> Result<DueOutcome, Error<M::Error>> {
        loop {
            if let Some(fault) = self.apply_due_mode(host_now, true)? {
                return Ok(DueOutcome::BeforeFault {
                    at: self.now(),
                    fault,
                });
            }
            if let Some((at, reason)) = self.finished {
                return Ok(DueOutcome::Finished { at, reason });
            }
            // A timer's zero-budget outputs can generate new same-time control
            // work. Drain that too, without positive budgets or pacing calls.
            if !self.has_due_work() {
                return Ok(DueOutcome::Applied { at: self.now() });
            }
        }
    }

    fn input_rejection(error: crate::oracle::OperationError) -> InputRejection {
        use crate::oracle::ErrorCode;
        InputRejection {
            reason: match error.code {
                ErrorCode::InvalidState => RejectionReason::InvalidState,
                ErrorCode::TargetUnavailable => RejectionReason::TargetUnavailable,
                ErrorCode::Unsupported => RejectionReason::Unsupported,
                _ => RejectionReason::InvalidRequest,
            },
            detail: error.message,
        }
    }
}

impl<M> Driver<M>
where
    M: Machine + Send + 'static,
    M::Context: 'static,
    M::Error: Send + 'static,
{
    #[cfg(test)]
    pub(super) fn input_admission(
        control: &mut control::Control,
        machines: &[Entry<M>],
        inputs: &BTreeMap<InputKey, LiveInput>,
        key: &InputKey,
        action: &Action,
        now: Timestamp,
    ) -> Result<InputAdmission, Error<M::Error>> {
        Self::input_admission_with_handle(control, machines, None, inputs, key, action, None, now)
    }
    #[allow(clippy::too_many_arguments)]
    pub(super) fn input_admission_with_handle(
        control: &mut control::Control,
        machines: &[Entry<M>],
        network: Option<&network::Network>,
        inputs: &BTreeMap<InputKey, LiveInput>,
        key: &InputKey,
        action: &Action,
        handle: Option<&FaultHandle>,
        now: Timestamp,
    ) -> Result<InputAdmission, Error<M::Error>> {
        use crate::oracle::{
            service::{Amount, Command, Effect},
            ErrorCode, OperationError, Target as Wire, PROCESS_EFFECT_TIMEOUT_NS,
        };
        use crate::pv::ApplicationCommand as App;
        let wire = |target: &Target| match target {
            Target::Machine(id) => Wire::Key(control::Control::source(*id)),
            Target::Name(name) => Wire::Machine(name.clone()),
        };
        let effect = |target: &Target, effect| Command::Effect {
            target: wire(target),
            effect,
        };
        let command = match action {
            Action::Pause(target) => Ok(effect(target, Effect::Pause)),
            Action::Resume(target) => Ok(effect(target, Effect::Resume)),
            Action::Crash(target) => Ok(effect(target, Effect::Crash)),
            Action::Reboot(target) => Ok(effect(target, Effect::Reboot)),
            Action::Shutdown(target) => Ok(effect(target, Effect::Shutdown)),
            Action::Stop { target, .. } => Ok(effect(target, Effect::Process(App::Stop))),
            Action::Restart { target, .. } => Ok(effect(target, Effect::Process(App::Start))),
            Action::PauseProcess { target, .. } => Ok(effect(target, Effect::Process(App::Pause))),
            Action::ResumeProcess { target, .. } => {
                Ok(effect(target, Effect::Process(App::Resume)))
            }
            Action::Throttle { target, config } => Ok(Command::Throttle {
                target: wire(target),
                config: *config,
            }),
            Action::Heal(HealTarget::Handle(handle)) => Ok(Command::Heal {
                handle: handle.0.clone(),
            }),
            Action::Heal(HealTarget::Input(input)) => {
                let id = match inputs.get(input) {
                    Some(LiveInput::Admitted { record_id, .. }) => *record_id,
                    Some(LiveInput::Handle(owned)) => Some(owned.record_id),
                    _ => None,
                };
                match id.filter(|&id| control.service.input_record_active(id)) {
                    Some(record_id) => Ok(Command::HealRecord { record_id }),
                    None => Err(OperationError {
                        code: ErrorCode::InvalidState,
                        message: format!("input {input} owns no active handle"),
                        handle: None,
                    }),
                }
            }
            Action::Spawn { name } => Ok(Command::Spawn {
                machine: name.clone(),
            }),
            Action::Partition { left, right } => Ok(Command::Partition {
                left: left.iter().map(wire).collect(),
                right: right.iter().map(wire).collect(),
            }),
            Action::Shape {
                from,
                to,
                policy,
                network_faults,
            } => Ok(Command::Shape {
                from: from.iter().map(wire).collect(),
                to: to.iter().map(wire).collect(),
                policy: *policy,
                network_faults: *network_faults,
            }),
            Action::DropConnection { connection } => Ok(Command::DropConnection {
                connection: connection.clone(),
            }),
            Action::DriftBounded {
                target,
                max_rate_ppb,
            } => Ok(Command::Drift {
                target: wire(target),
                ppb: Amount::Seeded(*max_rate_ppb),
            }),
            Action::StepRealtimeBounded {
                target,
                max_offset_ns,
            } => Ok(Command::StepRealtime {
                target: wire(target),
                nanoseconds: Amount::Seeded(*max_offset_ns),
            }),
            Action::Drift { target, rate } => Ok(Command::Drift {
                target: wire(target),
                ppb: Amount::Exact(i64::from(rate.ppb())),
            }),
            Action::StepRealtime {
                target,
                nanoseconds,
            } => Ok(Command::StepRealtime {
                target: wire(target),
                nanoseconds: Amount::Exact(nanoseconds.get()),
            }),
        };
        let deadline = match action.deadline() {
            Some(deadline) => u64::try_from(deadline.as_nanos())
                .map_err(|_| Error::Control("validated input deadline overflow".into()))?,
            None => PROCESS_EFFECT_TIMEOUT_NS,
        };
        let admitted = match command {
            Ok(command) => control
                .invoke_with_network(machines, network, |service, view| {
                    service.admit_input(
                        view,
                        key.as_str().to_owned(),
                        match handle {
                            Some(handle) => Command::Named {
                                handle: handle.0.clone(),
                                command: Box::new(command),
                            },
                            None => command,
                        },
                        now.as_nanos(),
                        deadline,
                    )
                })
                .map_err(Error::Control)?,
            Err(rejection) => Err(rejection),
        };
        Ok(admitted.map_err(Self::input_rejection))
    }

    pub(super) fn check_named_action(
        action: &Action,
        handle: Option<&FaultHandle>,
    ) -> Result<(), Error<M::Error>> {
        if let Some(handle) = handle {
            if handle.0.is_empty() || !action.installs_handle() {
                return Err(Error::InvalidRequest);
            }
        }
        Ok(())
    }
    pub(super) fn check_schedule_shape(
        &self,
        at: Timestamp,
        key: &InputKey,
        action: &Action,
    ) -> Result<(), Error<M::Error>> {
        if at < self.now() {
            return Err(Error::PastTime {
                now: self.now(),
                requested: at,
            });
        }
        match action.target() {
            Some(Target::Machine(id)) => {
                self.entry(*id)?;
            }
            Some(Target::Name(name)) => {
                self.control
                    .target(name)
                    .map_err(|_| Error::UnknownName(name.clone()))?;
            }
            None => {}
        }
        // Members may name declared machines spawned before admission.
        if let Action::Partition { left, right }
        | Action::Shape {
            from: left,
            to: right,
            ..
        } = action
        {
            for member in left.iter().chain(right) {
                match member {
                    Target::Machine(id) => {
                        self.entry(*id)?;
                    }
                    Target::Name(name) => {
                        if self.control.target(name).is_err() && !self.control.declared(name) {
                            return Err(Error::UnknownName(name.clone()));
                        }
                    }
                }
            }
        }
        if let Action::Shape {
            policy,
            network_faults,
            ..
        } = action
        {
            policy
                .validate_shape(self.network.latency_ns(), network_faults)
                .map_err(Error::NetworkConfiguration)?;
        }
        if let Action::DropConnection { connection } = action {
            for name in [&connection.from, &connection.to] {
                if self.control.target(name).is_err() && !self.control.declared(name) {
                    return Err(Error::UnknownName(name.clone()));
                }
            }
            if connection.from == connection.to
                || connection.src_port == 0
                || connection.dst_port == 0
                || connection.incarnation == 0
            {
                return Err(Error::InvalidRequest);
            }
        }
        if let Action::Spawn { name } = action {
            if !self.control.spawnable(name) {
                return Err(Error::UnknownName(name.clone()));
            }
        }
        if let Action::StepRealtime { nanoseconds, .. } = action {
            if nanoseconds.get().unsigned_abs() > crate::oracle::MAX_CLOCK_OFFSET_NS {
                return Err(Error::InvalidClockStep(*nanoseconds));
            }
        }
        if let Some(operation) = action.bounded_clock_operation() {
            operation
                .validate_at(at.as_nanos())
                .map_err(|error| Error::Control(error.message))?;
        }
        if let Some(deadline) = action.deadline() {
            let valid = u64::try_from(deadline.as_nanos())
                .ok()
                .filter(|&nanos| nanos > 0)
                .and_then(|nanos| at.as_nanos().checked_add(nanos))
                .is_some();
            if !valid {
                return Err(Error::InvalidDeadline(deadline));
            }
        }
        if self.inputs.contains_key(key) {
            return Err(Error::LiveInputKey(key.clone()));
        }
        Ok(())
    }
}

#[cfg(test)]
mod admission_tests;
