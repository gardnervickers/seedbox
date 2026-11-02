//! Driver checkpoint v4 field order; existing enum tags are retained.
use super::*;
wire_enum!(Vm; units {0=>Up,1=>Paused,2=>Down} tuples {} records {});
wire_enum!(Process; units {0=>Running,1=>Stopped,2=>Frozen} tuples {} records {});
wire_enum!(CommandKind; units {1=>StepRealtime} tuples {0=>Application(v0)} records {});
wire_enum!(Effect; units {0=>Spawn,1=>Pause,2=>Resume,3=>Crash,4=>Reboot,5=>Shutdown,10=>Partition,11=>Unpartition,13=>Unshape,14=>Select,15=>Unselect} tuples {6=>Execution(v0),7=>Drift(v0),8=>StepRealtime(v0),9=>Process(v0),12=>Shape(v0,v1)} records {16=>Fault{record,fault},17=>Unfault{record}});
wire_enum!(Scope; units {} tuples {} records {0=>Machine{name,boot},1=>Links{left,right},2=>DirectedLinks{from,to},3=>Connection{connection}});
wire_enum!(FaultAction; units {1=>Heal} tuples {0=>Effect(v0)} records {});
wire_enum!(Requester; units {} tuples {1=>Trigger(v0),2=>Input(v0)} records {0=>Call{source,id}});
wire_struct!(Admitted {
    record_id,
    fault,
    requester,
    scope,
    action,
    handle,
    requested_at,
    admitted_at
});
wire_enum!(Notice; units {} tuples {0=>Admitted(v0),1=>Settled(v0)} records {2=>TriggerArmed{trigger,source,target,at,deadline},3=>TriggerResolved{trigger,at,status,fault}});
wire_struct!(Settlement {
    record_id,
    network,
    counts,
    fault,
    requester,
    scope,
    action,
    requested_at,
    admitted_at,
    outcome_at,
    committed_at,
    status,
    handle,
    command,
    detail
});
wire_enum!(Plan; units {} tuples {0=>Settled(v0)} records {1=>Effect{fault,scope,at,effect,command,handle},2=>Wake{token,at},3=>Deadline{fault,at},4=>Reply{boot,source,id,at,payload},5=>Hold{source,trigger,op,nth,until},6=>Release{source,trigger}});
// The boxed immediate receipt has no extra wire tag. Charge its owned object
// before constructing it even though persisted timelines reject this plan arm.
impl crate::persistence::Wire for Box<Settlement> {
    fn put<W: std::io::Write>(
        &self,
        e: &mut crate::persistence::Encoder<W>,
    ) -> crate::persistence::Result<()> {
        self.as_ref().put(e)
    }
    fn get<R: std::io::Read>(
        d: &mut crate::persistence::Decoder<R>,
    ) -> crate::persistence::Result<Self> {
        d.claim_scratch::<Settlement>(1)?;
        Ok(Box::new(Settlement::get(d)?))
    }
}
wire_struct!(Call {
    boot,
    source,
    id,
    requested,
    admitted
});
wire_struct!(Pending {
    network,
    call,
    requester,
    scope,
    effect,
    command,
    dispatched,
    deadline,
    admitted,
    handle,
    result,
    healing,
    note
});
wire_struct!(Service {
    reply_latency_ns,
    handles,
    triggers,
    last,
    last_wake,
    pending,
    sleepers,
    poisoned,
    notices
});

impl Service {
    fn check_pending_handle_persist(&self, p: &Pending) -> crate::persistence::Result<()> {
        use crate::persistence::require;
        let expected = p.effect.handle_change();
        require(
            p.handle.is_some() == expected.is_some(),
            "pending handle effect",
        )?;
        if let Some(plan) = &p.handle {
            require(
                p.healing == plan.healing() && Some(plan.change) == expected,
                "pending handle transition",
            )?;
            require(
                match (&p.scope, &plan.target) {
                    (Scope::Machine { name, .. }, HandleTarget::Machine(target)) => name == target,
                    (Scope::Links { left, right }, HandleTarget::Links { left: a, right: b }) => {
                        left == a && right == b
                    }
                    (
                        Scope::DirectedLinks { from, to },
                        HandleTarget::DirectedLinks { from: a, to: b },
                    ) => from == a && to == b,
                    (Scope::Connection { connection }, HandleTarget::Connection(c)) => {
                        connection == c
                    }
                    _ => false,
                },
                "pending handle scope",
            )?;
        } else {
            require(!p.healing, "handleless heal")?;
        }
        if let Effect::Fault { record, .. } | Effect::Unfault { record } = p.effect {
            require(
                p.handle
                    .as_ref()
                    .is_some_and(|plan| plan.record_id.0 == record),
                "pending device fault record",
            )?;
        }
        if let Some(counts) = &p.network {
            counts.check_persist()?;
            require(
                p.effect == Effect::Unselect && p.healing && counts.selection.is_some(),
                "pending network settlement",
            )?;
            require(
                serde_json::to_value(counts).is_ok_and(|v| v == p.result),
                "pending network result",
            )?;
        } else {
            require(p.effect != Effect::Unselect, "selection heal counts absent")?;
        }
        Ok(())
    }
    fn check_record_identity_persist(
        &self,
        id: Option<super::super::handles::RecordId>,
        handle: &Option<String>,
    ) -> crate::persistence::Result<()> {
        crate::persistence::require(
            id.is_some() == handle.is_some()
                && id.is_none_or(|id| id.0 > 0 && id.0 <= self.handles.record_highwater()),
            "notice record identity",
        )
    }

    pub(crate) fn check_owned_input_persist(
        &self,
        id: super::super::handles::RecordId,
        text: &str,
        settled: bool,
    ) -> crate::persistence::Result<()> {
        self.handles.check_owned_input_persist(id, text, settled)
    }
    pub(crate) fn check_network_persist(
        &self,
        network: &crate::driver::NetworkState,
        resolve: &impl Fn(&str) -> Result<crate::driver::MachineId, String>,
    ) -> crate::persistence::Result<()> {
        for p in self.pending.values() {
            network.check_scope_persist(&p.scope, resolve)?;
            if let (Some(saved), Some(plan)) = (&p.network, &p.handle) {
                let before = saved
                    .selection
                    .ok_or(crate::persistence::CodecError::Invalid(
                        "pending selection counts",
                    ))?;
                let current = network
                    .selection_counts(&plan.handle.to_string())
                    .and_then(|s| s.selection)
                    .ok_or(crate::persistence::CodecError::Invalid(
                        "pending selection owner",
                    ))?;
                crate::persistence::require(
                    before.matched <= current.matched
                        && before.dropped <= current.dropped
                        && before
                            .matched
                            .checked_sub(before.dropped)
                            .zip(current.matched.checked_sub(current.dropped))
                            .is_some_and(|(a, b)| a <= b),
                    "pending selection counter history",
                )?;
            }
            if let Effect::Shape(policy, faults) = p.effect {
                policy
                    .validate_shape(network.latency_ns(), &faults)
                    .map_err(|_| crate::persistence::CodecError::Invalid("pending shape policy"))?;
            }
        }
        for notice in &self.notices {
            match notice {
                Notice::Admitted(a) => network.check_scope_persist(&a.scope, resolve)?,
                Notice::Settled(s) => network.check_scope_persist(&s.scope, resolve)?,
                _ => {}
            }
        }
        let mut expected = std::collections::BTreeSet::new();
        for (handle, change, target) in self.handles.retained_network() {
            network.check_owner_persist(&handle, change, target, resolve)?;
            expected.insert(handle);
        }
        network.check_owner_set_persist(&expected, &|id| self.token_persist(id))
    }
    pub(crate) fn has_pending_persist(&self, id: u32) -> bool {
        self.pending.contains_key(&id)
    }
    pub(crate) fn check_persist(
        &self,
        now: u64,
        exists: &impl Fn(&str) -> bool,
        spawn: &impl Fn(&str) -> bool,
    ) -> crate::persistence::Result<()> {
        use crate::persistence::require;
        require(!self.poisoned, "poisoned service")?;
        self.handles.check_persist(exists)?;
        self.triggers.check_persist(now, exists)?;
        // Each record has one pending owner, except a step not yet
        // acknowledged and the unsent heal queued behind it.
        let mut owners = BTreeMap::<_, (bool, bool)>::new();
        for (&id, p) in &self.pending {
            self.check_pending_handle_persist(p)?;
            if let Some(plan) = &p.handle {
                let owner = owners.entry(plan.record_id).or_default();
                let slot = if plan.healing() {
                    &mut owner.1
                } else {
                    &mut owner.0
                };
                require(!*slot, "duplicate pending handle record")?;
                *slot = true;
                let queued = self.handles.awaiting_start(plan.record_id);
                require(
                    !(owner.0 && owner.1) || queued,
                    "duplicate pending handle record",
                )?;
                require(
                    !(queued && plan.healing()) || p.command.is_none(),
                    "queued heal sent before its step",
                )?;
            }
            self.check_requester_persist(&p.requester, exists)?;
            if let Some(plan) = &p.handle {
                self.handles.check_plan_persist(plan)?;
            }
            require(
                id > 0
                    && id <= self.last
                    && p.admitted <= now
                    && p.deadline >= p.admitted
                    && p.dispatched.is_none_or(|at| at >= p.admitted && at <= now),
                "pending fault",
            )?;
            if let Some(c) = &p.call {
                require(
                    exists(&c.source) && c.requested <= c.admitted && c.admitted <= now,
                    "pending call",
                )?;
            }
            match &p.scope {
                Scope::Machine { name, .. } => require(
                    if p.effect == Effect::Spawn {
                        spawn(name)
                    } else {
                        exists(name)
                    },
                    "fault scope",
                )?,
                Scope::Connection { connection: c } => require(
                    !c.from.is_empty()
                        && c.from != c.to
                        && c.src_port != 0
                        && c.dst_port != 0
                        && c.incarnation != 0,
                    "fault connection",
                )?,
                Scope::DirectedLinks {
                    from: left,
                    to: right,
                }
                | Scope::Links { left, right } => {
                    require(!left.is_empty() && !right.is_empty(), "fault links")?;
                    for n in left.iter().chain(right) {
                        require(exists(n), "fault member")?;
                    }
                }
            }
        }
        for record in self.handles.unsettled_records() {
            let owner = owners.get(&record).copied().unwrap_or_default();
            let queued = self.handles.awaiting_start(record);
            require(
                (owner.0 || owner.1) && (!queued || (owner.0 && owner.1)),
                "unowned pending handle record",
            )?;
        }
        for (&token, (c, _)) in &self.sleepers {
            require(
                token > 0
                    && token <= self.last_wake
                    && exists(&c.source)
                    && c.requested <= c.admitted
                    && c.admitted <= now,
                "sleeper",
            )?;
        }
        Ok(())
    }
}

impl Service {
    pub(crate) fn check_requester_persist(
        &self,
        r: &Requester,
        exists: &impl Fn(&str) -> bool,
    ) -> crate::persistence::Result<()> {
        use crate::persistence::require;
        match r {
            Requester::Call { source, .. } => require(exists(source), "requester source"),
            Requester::Trigger(id) => require(
                *id > 0 && *id as usize <= self.triggers.records().len(),
                "requester trigger",
            ),
            Requester::Input(s) => require(
                (1..=128).contains(&s.len())
                    && s.bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-'),
                "requester input key",
            ),
        }
    }
    pub(crate) fn token_persist(&self, fault: u32) -> bool {
        fault > 0 && fault <= self.last
    }
    pub(crate) fn trigger_persist(&self, id: u32) -> bool {
        id > 0 && id as usize <= self.triggers.records().len()
    }
    pub(crate) fn check_notices_persist(
        &self,
        now: u64,
        known: &impl Fn(&str) -> bool,
    ) -> crate::persistence::Result<()> {
        for n in &self.notices {
            self.check_notice_persist(n, now, known)?;
        }
        Ok(())
    }
    pub(crate) fn check_notice_persist(
        &self,
        n: &Notice,
        now: u64,
        known: &impl Fn(&str) -> bool,
    ) -> crate::persistence::Result<()> {
        use crate::persistence::require;
        let scope = |s: &Scope| -> crate::persistence::Result<()> {
            match s {
                Scope::Machine { name, .. } => require(known(name), "notice scope"),
                Scope::Connection { connection: c } => require(
                    !c.from.is_empty()
                        && c.from != c.to
                        && c.src_port != 0
                        && c.dst_port != 0
                        && c.incarnation != 0,
                    "notice connection",
                ),
                Scope::DirectedLinks {
                    from: left,
                    to: right,
                }
                | Scope::Links { left, right } => {
                    require(!left.is_empty() && !right.is_empty(), "notice links")?;
                    for name in left.iter().chain(right) {
                        require(known(name), "notice link member")?;
                    }
                    Ok(())
                }
            }
        };
        match n {
            Notice::Admitted(a) => {
                self.check_record_identity_persist(a.record_id, &a.handle)?;
                require(
                    self.token_persist(a.fault)
                        && a.requested_at <= a.admitted_at
                        && a.admitted_at <= now,
                    "admitted notice",
                )?;
                self.check_requester_persist(&a.requester, known)?;
                scope(&a.scope)
            }
            Notice::Settled(s) => {
                self.check_record_identity_persist(s.record_id, &s.handle)?;
                if let Some(counts) = &s.network {
                    counts.check_persist()?;
                    require(s.action == FaultAction::Heal, "nonheal network result")?;
                }
                require(
                    self.token_persist(s.fault)
                        && s.requested_at <= s.admitted_at
                        && s.admitted_at <= s.committed_at
                        && s.committed_at <= now
                        && s.outcome_at.is_none_or(|at| at <= now),
                    "settled notice",
                )?;
                self.check_requester_persist(&s.requester, known)?;
                scope(&s.scope)
            }
            Notice::TriggerArmed {
                trigger,
                source,
                target,
                at,
                deadline,
            } => require(
                self.trigger_persist(*trigger)
                    && known(source)
                    && target.as_ref().is_none_or(|n| known(n))
                    && *at <= now
                    && deadline > at,
                "armed notice",
            ),
            Notice::TriggerResolved {
                trigger,
                at,
                status,
                fault,
            } => require(
                self.trigger_persist(*trigger)
                    && *at <= now
                    && !matches!(
                        status,
                        super::triggers::Status::Armed | super::triggers::Status::Matched
                    )
                    && fault.is_none_or(|f| self.token_persist(f)),
                "resolved notice",
            ),
        }
    }
    pub(crate) fn check_plan_persist(
        &self,
        p: &Plan,
        at: u64,
        known: &impl Fn(&str) -> bool,
    ) -> crate::persistence::Result<()> {
        use crate::persistence::require;
        match p {
            Plan::Settled(_) => Err(crate::persistence::CodecError::Invalid(
                "settlement cannot be queued",
            )),
            Plan::Hold { .. } | Plan::Release { .. } => Err(
                crate::persistence::CodecError::Invalid("a hold is sent at once, never queued"),
            ),
            Plan::Effect {
                fault,
                at: t,
                scope,
                ..
            } => {
                require(self.token_persist(*fault) && *t == at, "effect plan")?;
                match scope {
                    Scope::Machine { name, .. } => require(known(name), "effect scope"),
                    Scope::Connection { connection: c } => require(
                        !c.from.is_empty()
                            && c.from != c.to
                            && c.src_port != 0
                            && c.dst_port != 0
                            && c.incarnation != 0,
                        "effect connection",
                    ),
                    Scope::DirectedLinks {
                        from: left,
                        to: right,
                    }
                    | Scope::Links { left, right } => {
                        for n in left.iter().chain(right) {
                            require(known(n), "effect member")?;
                        }
                        Ok(())
                    }
                }
            }
            Plan::Deadline { fault, at: t } => {
                require(self.token_persist(*fault) && *t == at, "deadline plan")
            }
            Plan::Wake { token, at: t } => require(
                *token > 0 && *token <= self.last_wake && *t == at,
                "wake plan",
            ),
            Plan::Reply { source, at: t, .. } => require(known(source) && *t == at, "reply plan"),
        }
    }
}

impl Service {
    pub(crate) fn check_controller_persist(
        &self,
        controller: Option<&str>,
        latency: u64,
        boot_limit: u64,
    ) -> crate::persistence::Result<()> {
        use crate::persistence::require;
        for call in self
            .pending
            .values()
            .filter_map(|pending| pending.call.as_ref())
            .chain(self.sleepers.values().map(|(call, _)| call))
        {
            require(
                controller == Some(call.source.as_str())
                    && call.boot.is_some_and(|boot| boot <= boot_limit)
                    && call.requested.checked_add(latency) == Some(call.admitted),
                "pending call controller/admission",
            )?;
        }
        Ok(())
    }
}

impl Service {
    pub(crate) fn check_sdk_calls_persist(&self) -> crate::persistence::Result<()> {
        crate::persistence::require(
            self.pending
                .values()
                .filter_map(|pending| pending.call.as_ref())
                .chain(self.sleepers.values().map(|(call, _)| call))
                .all(|call| call.boot.is_none()),
            "SDK call has service boot authority",
        )
    }
}
