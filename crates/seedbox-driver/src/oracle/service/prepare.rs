//! Fault preparation: command resolution, policy checks, seeded draws and
//! handle reservation, before a fault ID is allocated.
use super::*;

impl Service {
    /// Admit a host input through the ordinary-control path at its due time.
    /// The outer result is fatal failure; the inner result is admission or
    /// policy rejection. A rejection allocates no fault ID and produces no
    /// reply; no guest call is fabricated. `deadline_ns` bounds process
    /// dispatch and acknowledgment after admission.
    pub(crate) fn admit_input(
        &mut self,
        adapter: &mut impl Adapter,
        key: String,
        command: Command,
        now: u64,
        deadline_ns: u64,
    ) -> Result<Result<(u32, Vec<Plan>), OperationError>, Failure> {
        self.valid()?;
        match self.prepare(
            adapter,
            AdmissionMode::Control,
            Requester::Input(key),
            None,
            command,
            now,
            deadline_ns,
        ) {
            Ok(admitted) => Ok(Ok(admitted)),
            Err(PreparationError::Rejected(e)) => Ok(Err(e)),
            Err(PreparationError::Fatal(e)) => {
                self.poison();
                Err(e)
            }
        }
    }
    #[allow(clippy::too_many_arguments)]
    pub(super) fn prepare(
        &mut self,
        adapter: &mut impl Adapter,
        mode: AdmissionMode,
        requester: Requester,
        call: Option<Call>,
        command: Command,
        now: u64,
        deadline_ns: u64,
    ) -> Result<(u32, Vec<Plan>), PreparationError> {
        let (name, command) = self.unwrap_named(command)?;
        let deadline = now
            .checked_add(deadline_ns)
            .ok_or_else(|| OperationError::invalid("effect deadline overflow"))?;
        let compiled = command.compile_targets(
            |target| adapter.resolve(target),
            |target| adapter.network_member(target),
            || adapter.network_floor(),
        )?;
        let (subject, effect, handle, healing) =
            self.prepare_change(adapter, mode, command, compiled, name)?;
        self.install(
            adapter, mode, requester, call, subject, effect, handle, healing, now, deadline,
        )
    }
    /// Split a named command into its checked handle name and inner command.
    fn unwrap_named(&self, command: Command) -> Result<(Option<String>, Command), OperationError> {
        match command {
            Command::Named { handle, command } => {
                self.handles.check_name(&handle).map_err(|e| {
                    error(
                        ErrorCode::InvalidState,
                        if e == crate::oracle::handles::Error::DuplicateActive {
                            "duplicate active handle".into()
                        } else {
                            format!("handle: {e:?}")
                        },
                    )
                })?;
                Ok((Some(handle), *command))
            }
            command => Ok((None, command)),
        }
    }
    /// Resolve a command's subject and effect, and reserve its handle.
    fn prepare_change(
        &mut self,
        adapter: &mut impl Adapter,
        mode: AdmissionMode,
        command: Command,
        compiled: targets::CompiledTargets<Facts>,
        name: Option<String>,
    ) -> Result<HandleAdmission, PreparationError> {
        Ok(match command {
            Command::Spawn { machine } => {
                adapter.reserve_spawn(&machine)?;
                (
                    Subject::Machine(Facts {
                        name: machine,
                        boot: 0,
                        vm: Vm::Up,
                        process: Process::Running,
                        physically_ended: false,
                        ready: false,
                    }),
                    Effect::Spawn,
                    None,
                    false,
                )
            }
            Command::Named { .. } => {
                return Err(OperationError::invalid("nested named admission").into())
            }
            Command::Heal { handle: text } => {
                let plan = self.handles.heal(&text);
                self.prepare_heal(adapter, mode, plan)?
            }
            Command::HealRecord { record_id } => {
                if !self.handles.active_record(record_id) {
                    return Err(
                        error(ErrorCode::InvalidState, "input owns no active handle").into(),
                    );
                }
                let plan = self.handles.heal_record(record_id);
                self.prepare_heal(adapter, mode, plan)?
            }
            Command::Shape {
                policy,
                network_faults,
                ..
            } => {
                let (from, to) = compiled
                    .sides
                    .ok_or_else(|| Failure::from("missing compiled shape targets"))?;
                let plan = self.reserve_handle(
                    HandleTarget::DirectedLinks {
                        from: from.clone(),
                        to: to.clone(),
                    },
                    Change::Shape(policy, network_faults),
                    name,
                )?;
                (
                    Subject::Network(Scope::DirectedLinks { from, to }),
                    Effect::Shape(policy, network_faults),
                    Some(plan),
                    false,
                )
            }
            Command::DropConnection { connection } => {
                self.prepare_drop_connection(adapter, connection, name)?
            }
            Command::Throttle { config, .. } => {
                let facts = compiled
                    .machine
                    .ok_or_else(|| Failure::from("missing compiled machine target"))?;
                let plan = self.reserve_handle(
                    HandleTarget::Machine(facts.name.clone()),
                    Change::Execution(config),
                    name,
                )?;
                (
                    Subject::Machine(facts),
                    Effect::Execution(config),
                    Some(plan),
                    false,
                )
            }
            Command::StepRealtime { nanoseconds, .. } => {
                let facts = compiled
                    .machine
                    .ok_or_else(|| Failure::from("missing compiled machine target"))?;
                self.prepare_step(adapter, mode, facts, nanoseconds, name)?
            }
            Command::Drift { ppb, .. } => {
                let facts = compiled
                    .machine
                    .ok_or_else(|| Failure::from("missing compiled machine target"))?;
                self.prepare_drift(adapter, facts, ppb, name)?
            }
            Command::Partition { .. } => {
                let (left, right) = compiled
                    .sides
                    .ok_or_else(|| Failure::from("missing compiled partition targets"))?;
                let plan = self.reserve_handle(
                    HandleTarget::Links {
                        left: left.clone(),
                        right: right.clone(),
                    },
                    Change::Partition,
                    name,
                )?;
                (
                    Subject::Links { left, right },
                    Effect::Partition,
                    Some(plan),
                    false,
                )
            }
            Command::Effect { effect, .. } => (
                Subject::Machine(
                    compiled
                        .machine
                        .ok_or_else(|| Failure::from("missing compiled effect target"))?,
                ),
                effect,
                None,
                false,
            ),
        })
    }
    /// Reserve a connection selection, rolled back unless the incarnation is
    /// still available with selector capacity.
    fn prepare_drop_connection(
        &mut self,
        adapter: &impl Adapter,
        connection: Connection,
        name: Option<String>,
    ) -> Result<HandleAdmission, PreparationError> {
        let plan = self.reserve_handle(
            HandleTarget::Connection(connection.clone()),
            Change::Select,
            name,
        )?;
        let available = adapter.selection_available(&connection);
        if !matches!(available, Ok(true)) {
            self.handles
                .rejected(&plan)
                .map_err(|_| Failure::from("connection reservation rollback"))?;
            return Err(available
                .err()
                .unwrap_or_else(|| {
                    error(
                        ErrorCode::InvalidState,
                        "connection incarnation unavailable or selector capacity exhausted",
                    )
                })
                .into());
        }
        Ok((
            Subject::Network(Scope::Connection { connection }),
            Effect::Select,
            Some(plan),
            false,
        ))
    }
    fn prepare_step(
        &mut self,
        adapter: &mut impl Adapter,
        mode: AdmissionMode,
        facts: Facts,
        nanoseconds: Amount,
        name: Option<String>,
    ) -> Result<HandleAdmission, PreparationError> {
        Self::step_state(mode, &facts)?;
        // Draw only after admission checks, at this barrier.
        let offset = match nanoseconds {
            Amount::Exact(offset) => offset,
            Amount::Seeded(max) => Self::uniform(adapter, &facts.name, max)?,
        };
        let plan = self.reserve_handle(
            HandleTarget::Machine(facts.name.clone()),
            Change::Step(offset),
            name,
        )?;
        Ok((
            Subject::Machine(facts),
            Effect::StepRealtime(offset),
            Some(plan),
            false,
        ))
    }
    fn prepare_drift(
        &mut self,
        adapter: &mut impl Adapter,
        facts: Facts,
        ppb: Amount,
        name: Option<String>,
    ) -> Result<HandleAdmission, PreparationError> {
        // A machine runs at one rate. Like a refused throttle, a
        // refused duplicate consumes its handle number; it draws no value.
        if self.handles.drifting(&facts.name) {
            if name.is_none() {
                self.handles
                    .reserve()
                    .map_err(|e| error(ErrorCode::InvalidState, format!("clock_drift: {e:?}")))?;
            }
            return Err(error(
                ErrorCode::InvalidState,
                "target machine clock is already drifting",
            )
            .into());
        }
        let ppb = match ppb {
            Amount::Exact(rate) => rate,
            Amount::Seeded(max) => Self::uniform(adapter, &facts.name, max)?,
        };
        let rate = ClockRate::new(ppb).map_err(|e| OperationError::invalid(e.to_string()))?;
        let plan = self.reserve_handle(
            HandleTarget::Machine(facts.name.clone()),
            Change::Drift(rate),
            name,
        )?;
        Ok((
            Subject::Machine(facts),
            Effect::Drift(rate),
            Some(plan),
            false,
        ))
    }
    fn reserve_handle(
        &mut self,
        target: HandleTarget,
        change: Change,
        name: Option<String>,
    ) -> Result<ExecutionPlan, OperationError> {
        let label = match change {
            Change::Execution(_) => "throttle",
            Change::Drift(_) => "clock_drift",
            Change::Step(_) => "clock_skew",
            Change::Partition => "partition",
            Change::Shape(..) => "shape",
            Change::Select => "drop_connection",
            _ => "handle",
        };
        let result = if let Some(name) = name {
            self.handles.named(target, change, name)
        } else {
            match (target, change) {
                (HandleTarget::Machine(name), Change::Execution(c)) => {
                    self.handles.throttle(name, c)
                }
                (HandleTarget::Machine(name), Change::Drift(c)) => self.handles.drift(name, c),
                (HandleTarget::Machine(name), Change::Step(n)) => self.handles.step(name, n),
                (HandleTarget::Links { left, right }, Change::Partition) => {
                    self.handles.partition(left, right)
                }
                (target, change) => self.handles.network(target, change),
            }
        };
        result.map_err(|e| error(ErrorCode::InvalidState, format!("{label}: {e:?}")))
    }
    fn prepare_heal(
        &mut self,
        adapter: &impl Adapter,
        mode: AdmissionMode,
        plan: Result<ExecutionPlan, crate::oracle::handles::Error>,
    ) -> Result<HandleAdmission, PreparationError> {
        let plan = plan.map_err(|e| error(ErrorCode::InvalidState, format!("heal: {e:?}")))?;
        let subject = match Self::heal_subject(adapter, mode, &plan) {
            Ok(s) => s,
            Err(e) => {
                self.handles
                    .rejected(&plan)
                    .expect("matching local reservation");
                return Err(e.into());
            }
        };
        let effect = match plan.change {
            Change::Partition | Change::Shape(..) | Change::Select => {
                return Err(Failure::from("heal reapplies installed policy").into())
            }
            change => Effect::from(change),
        };
        Ok((subject, effect, Some(plan), true))
    }
    /// The subject of a heal. A partition heal restores links and binds to no
    /// machine boot; undoing a live step needs the same guest as the step.
    fn heal_subject(
        adapter: &impl Adapter,
        mode: AdmissionMode,
        plan: &ExecutionPlan,
    ) -> Result<Subject, OperationError> {
        let name = match &plan.target {
            HandleTarget::Links { left, right } => {
                for name in left.iter().chain(right) {
                    adapter.check_control_target(&Target::Key(name.clone()))?;
                }
                return Ok(Subject::Links {
                    left: left.clone(),
                    right: right.clone(),
                });
            }
            HandleTarget::DirectedLinks { from, to } => {
                for name in from.iter().chain(to) {
                    adapter.check_control_target(&Target::Key(name.clone()))?;
                }
                return Ok(Subject::Network(Scope::DirectedLinks {
                    from: from.clone(),
                    to: to.clone(),
                }));
            }
            HandleTarget::Connection(connection) => {
                adapter.check_control_target(&Target::Machine(connection.from.clone()))?;
                adapter.check_control_target(&Target::Machine(connection.to.clone()))?;
                return Ok(Subject::Network(Scope::Connection {
                    connection: connection.clone(),
                }));
            }
            HandleTarget::Machine(name) => name,
        };
        let facts = adapter.resolve(&Target::Key(name.clone()))?;
        if matches!(plan.change, Change::Step(_)) && !plan.lost {
            Self::step_state(mode, &facts)?;
        }
        Ok(Subject::Machine(facts))
    }
    /// Resolve partition sides to sorted canonical member names before any
    /// handle allocation: nonempty, attached, unique and disjoint.
    pub(super) fn partition_sides(
        adapter: &impl Adapter,
        left: &[Target],
        right: &[Target],
    ) -> Result<(Vec<String>, Vec<String>), OperationError> {
        targets::sides(left, right, |target| adapter.network_member(target))
    }
    /// Realtime steps are guest commands and share the process-command state
    /// rules, applied before any seeded draw.
    fn step_state(mode: AdmissionMode, facts: &Facts) -> Result<(), OperationError> {
        if matches!(mode, AdmissionMode::Control) && facts.physically_ended && facts.vm != Vm::Down
        {
            return Err(error(ErrorCode::TargetUnavailable, "target VM has exited"));
        }
        // As for process commands: paused is refused; a down target is
        // admitted and then settles Unavailable (see `install`).
        match facts.vm {
            Vm::Paused => Err(error(ErrorCode::InvalidState, "target machine is paused")),
            Vm::Up | Vm::Down => Ok(()),
        }
    }
    /// Uniform in [-max, max] by rejection sampling of seeded 64-bit draws.
    fn uniform(adapter: &mut impl Adapter, target: &str, max: u64) -> Result<i64, OperationError> {
        let span = max
            .checked_mul(2)
            .and_then(|n| n.checked_add(1))
            .filter(|_| i64::try_from(max).is_ok())
            .ok_or_else(|| OperationError::invalid("seeded clock bound overflow"))?;
        let limit = u64::MAX - u64::MAX % span;
        loop {
            let draw = adapter.draw(target)?;
            if draw < limit {
                return Ok((draw % span) as i64 - max as i64);
            }
        }
    }
}
