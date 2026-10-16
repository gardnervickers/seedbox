//! Driver checkpoint v4 field order; existing enum tags are retained.
use super::*;
wire_struct!(Commands { next, used });
wire_struct!(Observed {
    name,
    attached,
    commands,
    clock
});
wire_struct!(Control {
    service,
    controller,
    entries,
    spawnable,
    attached_spawnable,
    spawning,
    calls
});
wire_enum!(Event; units {3=>TriggerBoundary,4=>TriggerFinalize} tuples {1=>Plan(v0)} records {0=>Call{source,boot,id,observed,payload},2=>Input{key,action,handle},5=>Reply{target,boot,id,payload}});

impl Control {
    pub(crate) fn has_pending(&self, id: super::super::FaultId) -> bool {
        self.service.has_pending_persist(id.0)
    }
    pub(crate) fn check_persist<T>(
        &self,
        entries: &[Entry<T>],
        definitions: &[MachineDefinition],
        network: &super::super::network::Network,
        now: u64,
    ) -> crate::persistence::Result<()> {
        use crate::persistence::require;
        require(self.entries.len() == entries.len(), "control entries")?;
        require(
            self.service.reply_latency_ns == self.controller.map_or(0, |setup| setup.latency_ns),
            "controller reply latency",
        )?;
        if let Some(setup) = self.controller {
            require(
                self.calls.is_empty(),
                "configured calls must carry explicit boot ownership",
            )?;
            require(
                setup.latency_ns >= network.latency_ns(),
                "control latency below causal bound",
            )?;
            let controller = setup.controller.map(Self::source);
            self.service.check_controller_persist(
                controller.as_deref(),
                setup.latency_ns,
                entries[0].lifecycle.boot(),
            )?;
            if let Some(id) = setup.controller {
                require(
                    id == super::super::MachineId(0)
                        && self.entries.first().is_some_and(|entry| {
                            entry.name.as_ref().is_some_and(|name| !name.is_empty())
                        }),
                    "controller identity",
                )?;
            }
        }
        if self.controller.is_none() {
            self.service.check_sdk_calls_persist()?;
        }
        let mut names = BTreeSet::new();
        for d in definitions {
            require(names.insert(d.name.clone()), "duplicate future definition")?;
        }
        let declared: BTreeSet<_> = self
            .spawnable
            .iter()
            .chain(self.spawning.keys())
            .cloned()
            .collect();
        require(
            names == declared
                && self
                    .spawnable
                    .iter()
                    .all(|n| !self.spawning.contains_key(n)),
            "future definition reservations",
        )?;
        let reserved: BTreeSet<_> = self.spawning.values().map(|id| id.0).collect();
        require(
            reserved
                .into_iter()
                .eq(entries.len()..self.slots_persist()?),
            "reserved machine identities",
        )?;
        for definition in definitions {
            require(
                definition.network.is_some() == self.attached_spawnable.contains(&definition.name),
                "future attachment",
            )?;
        }
        for (i, o) in self.entries.iter().enumerate() {
            require(
                network.member_matches_persist(i, o.name.as_deref(), o.attached),
                "network/control identity",
            )?;
            o.clock
                .check_persist(RngDomain::ClockFaults, Some(network.member_seed(i)))?;
        }
        let exists = |s: &str| {
            s.strip_prefix("machine:")
                .and_then(|n| n.parse::<usize>().ok())
                .is_some_and(|i| s == format!("machine:{i}") && i < entries.len())
        };
        for (&(machine, session, request), &boot) in &self.calls {
            require(
                self.controller
                    .is_none_or(|setup| setup.controller == Some(machine)),
                "admitted call controller authority",
            )?;
            require(
                machine.0 < entries.len()
                    && session > 0
                    && request > 0
                    && boot <= entries[machine.0].lifecycle.boot(),
                "call boot identity",
            )?;
        }
        self.service
            .check_persist(now, &exists, &|name| self.spawning.contains_key(name))?;
        self.service
            .check_network_persist(network, &|name| self.reply_target(name))?;
        self.service
            .check_notices_persist(now, &|n| self.known_persist(n))
    }
}

impl Control {
    pub(crate) fn scratch_persist(
        &self,
        defs: &[MachineDefinition],
    ) -> crate::persistence::Result<u64> {
        let mut size = 0u64;
        for d in defs {
            size = size
                .checked_add(2 * (d.name.len() as u64 + std::mem::size_of::<String>() as u64))
                .ok_or(crate::persistence::CodecError::Limit("scratch overflow"))?;
        }
        size.checked_add((self.spawning.len() * std::mem::size_of::<usize>()) as u64)
            .ok_or(crate::persistence::CodecError::Limit("scratch overflow"))
    }
}

impl Control {
    pub(crate) fn known_persist(&self, n: &str) -> bool {
        Control::key_index(&self.entries, n).is_ok()
            || self.spawning.contains_key(n)
            || self.entries.iter().any(|e| e.name.as_deref() == Some(n))
    }
    pub(crate) fn slots_persist(&self) -> crate::persistence::Result<usize> {
        self.entries.len().checked_add(self.spawning.len()).ok_or(
            crate::persistence::CodecError::Invalid("machine identity overflow"),
        )
    }
    pub(crate) fn check_notice_persist(
        &self,
        n: &crate::oracle::service::Notice,
        now: u64,
    ) -> crate::persistence::Result<()> {
        self.service
            .check_notice_persist(n, now, &|n| self.known_persist(n))
    }
    pub(crate) fn check_plan_persist(
        &self,
        p: &crate::oracle::service::Plan,
        at: u64,
    ) -> crate::persistence::Result<()> {
        if let crate::oracle::service::Plan::Reply { boot, .. } = p {
            crate::persistence::require(
                self.controller.is_none() && boot.is_none(),
                "unbound service reply",
            )?;
        }
        self.service
            .check_plan_persist(p, at, &|n| self.known_persist(n))
    }
    pub(crate) fn calls_boot_persist(
        &self,
        source: &str,
        id: crate::pv::SimulationControlId,
    ) -> crate::persistence::Result<u64> {
        let source = self
            .reply_target(source)
            .map_err(|_| crate::persistence::CodecError::Invalid("reply source"))?;
        self.calls
            .get(&(source, id.session.get(), id.request.get()))
            .copied()
            .ok_or(crate::persistence::CodecError::Invalid(
                "reply lacks admitted call",
            ))
    }
    pub(crate) fn check_reply_persist(
        &self,
        target: MachineId,
        id: crate::pv::SimulationControlId,
        payload: &[u8],
        at: u64,
        now: u64,
    ) -> crate::persistence::Result<()> {
        use crate::persistence::{require, CodecError};
        let setup = self.controller.ok_or(CodecError::Invalid(
            "delayed reply without controller setting",
        ))?;
        require(
            setup.controller == Some(target),
            "reply controller authority",
        )?;
        require(
            payload.len() < crate::oracle::MAX_FRAME,
            "reply frame limit",
        )?;
        let value: serde_json::Value =
            serde_json::from_slice(payload).map_err(|_| CodecError::Invalid("reply payload"))?;
        let requested = value["requested_at_ns"]
            .as_u64()
            .ok_or(CodecError::Invalid("reply requested time"))?;
        let admitted = value["admitted_at_ns"]
            .as_u64()
            .ok_or(CodecError::Invalid("reply admitted time"))?;
        let replied = value["replied_at_ns"]
            .as_u64()
            .ok_or(CodecError::Invalid("reply time"))?;
        require(
            value["version"] == 1
                && value["id"] == id.request.get()
                && replied == at
                && admitted <= now
                && requested.checked_add(setup.latency_ns) == Some(admitted),
            "reply admission identity/time",
        )?;
        if value["effect_at_ns"].is_null() {
            require(
                replied
                    .checked_sub(setup.latency_ns)
                    .is_some_and(|decision| decision >= admitted && decision <= now),
                "reply decision/latency",
            )
        } else {
            let effect = value["effect_at_ns"]
                .as_u64()
                .ok_or(CodecError::Invalid("reply effect time"))?;
            require(
                effect >= admitted
                    && effect <= now
                    && effect.checked_add(setup.latency_ns) == Some(replied),
                "reply effect/latency",
            )
        }
    }
}
