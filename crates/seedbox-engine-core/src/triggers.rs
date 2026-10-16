//! Bounded streaming matchers. Marker triggers watch a machine's committed
//! UART sink; typed triggers watch committed SDK records, lifecycle
//! transitions and routed frames. No speculative bytes, host log tail or
//! wall clock feeds either.
use seedbox_control::{
    check_trigger_budget, valid_trigger_marker, Action, EventSelector, Plan, Transition,
    TriggerRecord, TriggerSpec, TriggerStatus, MAX_TRIGGERS, MAX_TRIGGER_MARKER,
    PROCESS_EFFECT_TIMEOUT_NS,
};
use serde_json::Value;
use sha2::{Digest, Sha256};

#[cfg(test)]
mod checkpoint_tests {
    use super::*;

    #[test]
    fn restored_partial_match_uses_rebuilt_failure_table() {
        let mut original = Triggers::default();
        original
            .arm(
                "a".into(),
                b"ABABAC".to_vec(),
                0,
                100,
                Action::Heal {
                    handle: "cut".into(),
                },
            )
            .unwrap();
        for byte in b"ABA" {
            original.committed("a", 1, *byte, 10, 1).unwrap();
        }
        let saved = serde_json::to_value(&original).unwrap();
        let mut restored: Triggers = serde_json::from_value(saved.clone()).unwrap();
        for byte in b"BABAC" {
            original.committed("a", 2, *byte, 10, 2).unwrap();
            restored.committed("a", 2, *byte, 10, 2).unwrap();
        }
        assert_eq!(original.records, restored.records);
        assert_eq!(restored.records[0].status, TriggerStatus::Matched);
        let matched: Triggers =
            serde_json::from_slice(&serde_json::to_vec(&restored).unwrap()).unwrap();
        assert_eq!(matched.records, restored.records);
        for (field, value) in [("prefix", 6), ("id", 2), ("match_deadline_ns", 0)] {
            let mut invalid = saved.clone();
            invalid[0][field] = serde_json::json!(value);
            assert!(serde_json::from_value::<Triggers>(invalid).is_err());
        }
    }
}

#[derive(Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(try_from = "Vec<TriggerRecord>", into = "Vec<TriggerRecord>")]
pub struct Triggers {
    pub records: Vec<TriggerRecord>,
    failure: Vec<Vec<usize>>,
}
impl From<Triggers> for Vec<TriggerRecord> {
    fn from(triggers: Triggers) -> Self {
        triggers.records
    }
}
impl TryFrom<Vec<TriggerRecord>> for Triggers {
    type Error = String;
    fn try_from(records: Vec<TriggerRecord>) -> Result<Self, Self::Error> {
        let mut restored = Self::default();
        for record in records {
            if record.selector.is_some() {
                restored.restore_typed(record)?;
                continue;
            }
            if record.prefix > record.marker.len()
                || (record.status == TriggerStatus::Armed && record.prefix == record.marker.len())
                || record.prefix as u64 > record.seen
            {
                return Err("invalid checkpoint trigger prefix".into());
            }
            let timeout = record
                .match_deadline_ns
                .checked_sub(record.armed_at_ns)
                .ok_or("invalid checkpoint trigger deadline")?;
            // Rebuild the derived matcher table with the same admission checks
            // used for new triggers; persisted tables must never drive indexing.
            let id = restored.arm(
                record.source.clone(),
                record.marker.clone(),
                record.armed_at_ns,
                timeout,
                record.action.clone(),
            )?;
            if record.id != id {
                return Err("invalid checkpoint trigger sequence".into());
            }
            restored.records[id as usize - 1] = record;
        }
        restored.reclaim();
        Ok(restored)
    }
}

/// One committed occurrence that typed triggers can match.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Observed {
    Event {
        machine: String,
        name: String,
    },
    Assertion {
        machine: String,
        property: String,
        holds: bool,
    },
    SetupComplete {
        machine: String,
    },
    State {
        machine: String,
        name: String,
        value: String,
    },
    /// A typed trigger, by input key, admitted its effect.
    Fired {
        key: String,
    },
    Lifecycle {
        machine: String,
        transition: Transition,
    },
    ConnectionOpen {
        from: String,
        to: String,
        port: u16,
    },
    Frame {
        from: String,
        to: String,
    },
}
impl Observed {
    /// The SDK records typed triggers watch.
    pub fn from_record(
        machine: &str,
        record: &seedbox_engine_protocol::property::Record,
    ) -> Option<Self> {
        use seedbox_engine_protocol::property::{Kind, Record};
        let machine = machine.to_owned();
        Some(match record {
            Record::Event { name, .. } => Self::Event {
                machine,
                name: name.clone(),
            },
            Record::Assert {
                id,
                kind,
                condition,
                ..
            } => Self::Assertion {
                machine,
                property: id.clone(),
                holds: *condition || matches!(kind, Kind::Reachable | Kind::Unreachable),
            },
            Record::SetupComplete { .. } => Self::SetupComplete { machine },
            Record::State { name, value } => Self::State {
                machine,
                name: name.clone(),
                value: value.clone(),
            },
            _ => return None,
        })
    }
}
fn selects(selector: &EventSelector, event: &Observed) -> bool {
    let any = |want: &Option<String>, have: &String| want.as_ref().is_none_or(|w| w == have);
    match (selector, event) {
        (EventSelector::Event { name, machine: m }, Observed::Event { machine, name: n }) => {
            name == n && any(m, machine)
        }
        (
            EventSelector::Assertion {
                property,
                holds,
                machine: m,
            },
            Observed::Assertion {
                machine,
                property: p,
                holds: h,
            },
        ) => property == p && holds.is_none_or(|want| want == *h) && any(m, machine),
        (EventSelector::SetupComplete { machine: m }, Observed::SetupComplete { machine }) => {
            any(m, machine)
        }
        (
            EventSelector::State {
                name,
                value: v,
                machine: m,
            },
            Observed::State {
                machine,
                name: n,
                value,
            },
        ) => name == n && any(v, value) && any(m, machine),
        (EventSelector::Fired { trigger }, Observed::Fired { key }) => trigger == key,
        (
            EventSelector::Lifecycle {
                machine: m,
                transition: t,
            },
            Observed::Lifecycle {
                machine,
                transition,
            },
        ) => m == machine && t == transition,
        (
            EventSelector::ConnectionOpen {
                from: f,
                to: t,
                port: p,
            },
            Observed::ConnectionOpen { from, to, port },
        ) => any(f, from) && any(t, to) && p.is_none_or(|want| want == *port),
        (EventSelector::Frame { from: f, to: t }, Observed::Frame { from, to }) => {
            any(f, from) && any(t, to)
        }
        _ => false,
    }
}
impl Triggers {
    pub fn arm(
        &mut self,
        source: String,
        marker: Vec<u8>,
        now: u64,
        timeout: u64,
        action: Action,
    ) -> Result<u32, String> {
        check_trigger_budget(&self.records)?;
        if !valid_trigger_marker(&marker) {
            return Err(format!(
                "marker must contain 1..={MAX_TRIGGER_MARKER} bytes"
            ));
        }
        if timeout == 0 {
            return Err("match timeout must be positive".into());
        }
        let deadline = now
            .checked_add(timeout)
            .ok_or("trigger match deadline overflow")?;
        deadline
            .checked_add(PROCESS_EFFECT_TIMEOUT_NS)
            .ok_or("trigger effect deadline overflow")?;
        let mut failure = vec![0; marker.len()];
        for i in 1..marker.len() {
            let mut j = failure[i - 1];
            while j > 0 && marker[i] != marker[j] {
                j = failure[j - 1];
            }
            if marker[i] == marker[j] {
                j += 1;
            }
            failure[i] = j;
        }
        let id = self.records.len() as u32 + 1;
        self.records.push(TriggerRecord {
            id,
            source,
            marker,
            armed_at_ns: now,
            match_deadline_ns: deadline,
            action,
            prefix: 0,
            seen: 0,
            resets: 0,
            matched_at_ns: None,
            effect_at_ns: None,
            fault_id: None,
            cancelled_at_ns: None,
            cut_off_at_ns: None,
            status: TriggerStatus::Armed,
            rejection_reason: None,
            key: None,
            selector: None,
            nth: 1,
            delay_ns: 0,
        });
        self.failure.push(failure);
        Ok(id)
    }
    /// Arm a typed trigger from a due input. Callers check its shape and
    /// machine names first; a full budget is the input's rejection.
    pub fn arm_typed(&mut self, key: &str, spec: &TriggerSpec, now: u64) -> Result<u32, String> {
        check_trigger_budget(&self.records)?;
        spec.validate()?;
        if let EventSelector::BlockRequest { machine, .. } = &spec.when {
            if self.armed_holds().any(|(m, _)| m == machine) {
                return Err(format!(
                    "{machine} already has an armed block_request trigger"
                ));
            }
        }
        let deadline = match spec.within_ns {
            Some(within) => now
                .checked_add(within)
                .ok_or("trigger match deadline overflow")?,
            None => u64::MAX,
        };
        let id = self.records.len() as u32 + 1;
        self.records.push(TriggerRecord {
            id,
            source: spec.when.machines().first().copied().unwrap_or("").into(),
            marker: Vec::new(),
            armed_at_ns: now,
            match_deadline_ns: deadline,
            action: spec.then.clone(),
            prefix: 0,
            seen: 0,
            resets: 0,
            matched_at_ns: None,
            effect_at_ns: None,
            fault_id: None,
            cancelled_at_ns: None,
            cut_off_at_ns: None,
            status: TriggerStatus::Armed,
            rejection_reason: None,
            key: Some(key.into()),
            selector: Some(spec.when.clone()),
            nth: spec.nth,
            delay_ns: spec.delay_ns,
        });
        self.failure.push(Vec::new());
        Ok(id)
    }
    /// Checkpoints hold typed records whole; recheck what arming checked.
    fn restore_typed(&mut self, record: TriggerRecord) -> Result<(), String> {
        let selector = record
            .selector
            .clone()
            .ok_or("typed trigger lacks a selector")?;
        let spec = TriggerSpec {
            when: selector.clone(),
            nth: record.nth,
            delay_ns: record.delay_ns,
            within_ns: None,
            then: record.action.clone(),
        };
        spec.validate()?;
        let expected_source = selector.machines().first().copied().unwrap_or("");
        if record.id as usize != self.records.len() + 1
            || self.records.len() >= MAX_TRIGGERS
            || record.key.as_deref().is_none_or(str::is_empty)
            || !record.marker.is_empty()
            || record.prefix != 0
            || record.resets != 0
            || record.source != expected_source
            || record.match_deadline_ns <= record.armed_at_ns
            || record.seen > record.nth
            || (record.status == TriggerStatus::Armed && record.seen >= record.nth)
        {
            return Err("invalid checkpoint typed trigger".into());
        }
        self.records.push(record);
        self.failure.push(Vec::new());
        Ok(())
    }
    /// Armed typed triggers exist, so committed occurrences must be offered.
    pub fn typed_armed(&self) -> bool {
        self.records
            .iter()
            .any(|r| r.status == TriggerStatus::Armed && r.selector.is_some())
    }
    /// Offer one committed occurrence at `at`. The nth match schedules the
    /// effect after the world's causal lookahead plus the rule's delay.
    pub fn observe(
        &mut self,
        event: &Observed,
        at: u64,
        lookahead: u64,
        max_time: u64,
    ) -> Result<(), String> {
        for r in &mut self.records {
            let Some(selector) = &r.selector else {
                continue;
            };
            if r.status != TriggerStatus::Armed
                || at < r.armed_at_ns
                || at >= r.match_deadline_ns
                || !selects(selector, event)
            {
                continue;
            }
            r.seen = r.seen.checked_add(1).ok_or("trigger count overflow")?;
            if r.seen == r.nth {
                let effect = at
                    .checked_add(lookahead)
                    .and_then(|t| t.checked_add(r.delay_ns))
                    .ok_or("trigger effect overflow")?;
                effect
                    .checked_add(PROCESS_EFFECT_TIMEOUT_NS)
                    .ok_or("trigger effect deadline overflow")?;
                if effect < max_time {
                    return Err("trigger effect is in committed machine past".into());
                }
                r.status = TriggerStatus::Matched;
                r.matched_at_ns = Some(at);
                r.effect_at_ns = Some(effect);
            }
        }
        Ok(())
    }
    /// The holds armed `block_request` triggers install, by machine. Each
    /// counts from installation; a successor incarnation starts afresh.
    pub fn armed_holds(&self) -> impl Iterator<Item = (&str, crate::virtio_block::Hold)> {
        self.records
            .iter()
            .filter_map(|r| match (&r.selector, r.status) {
                (Some(EventSelector::BlockRequest { machine, op }), TriggerStatus::Armed) => {
                    Some((
                        machine.as_str(),
                        crate::virtio_block::Hold {
                            trigger_id: r.id,
                            op: *op,
                            nth: r.nth,
                            seen: 0,
                        },
                    ))
                }
                _ => None,
            })
    }
    /// A machine held the request a `block_request` trigger selected at
    /// `at`. Its crash follows one causal lookahead later, independent of
    /// where rounds end; both times are recorded.
    pub fn hold(&mut self, id: u32, at: u64, lookahead: u64, max_time: u64) -> Result<(), String> {
        let r = id
            .checked_sub(1)
            .and_then(|i| self.records.get_mut(i as usize))
            .filter(|r| {
                r.status == TriggerStatus::Armed
                    && matches!(r.selector, Some(EventSelector::BlockRequest { .. }))
                    && at >= r.armed_at_ns
                    && at < r.match_deadline_ns
            })
            .ok_or("held block request for no armed trigger")?;
        let effect = at.checked_add(lookahead).ok_or("trigger effect overflow")?;
        effect
            .checked_add(PROCESS_EFFECT_TIMEOUT_NS)
            .ok_or("trigger effect deadline overflow")?;
        if effect < max_time {
            return Err("held request crash is in committed machine past".into());
        }
        r.seen = r.nth;
        r.status = TriggerStatus::Matched;
        r.matched_at_ns = Some(at);
        r.effect_at_ns = Some(effect);
        Ok(())
    }
    /// Resolved marker triggers free their matcher tables.
    fn reclaim(&mut self) {
        for (r, failure) in self.records.iter().zip(&mut self.failure) {
            if r.status != TriggerStatus::Armed && !failure.is_empty() {
                *failure = Vec::new();
            }
        }
    }
    pub fn observes(&self, source: &str) -> bool {
        self.records
            .iter()
            .any(|r| r.status == TriggerStatus::Armed && r.selector.is_none() && r.source == source)
    }
    pub fn cancel(&mut self, id: u32, now: u64) -> Result<(), String> {
        let r = self
            .records
            .get_mut(id.checked_sub(1).ok_or("unknown trigger ID")? as usize)
            .ok_or("unknown trigger ID")?;
        if r.selector.is_some() {
            return Err("input program triggers cannot be cancelled".into());
        }
        if r.status != TriggerStatus::Armed || now < r.armed_at_ns || now > r.match_deadline_ns {
            return Err("trigger is no longer armed".into());
        }
        r.status = TriggerStatus::Cancelled;
        r.cancelled_at_ns = Some(now);
        self.reclaim();
        Ok(())
    }
    /// Future effects use the same fixed causal latency as control admission.
    /// An old event cannot contribute even a partial prefix after admission.
    pub fn committed(
        &mut self,
        source: &str,
        at: u64,
        byte: u8,
        latency: u64,
        max_time: u64,
    ) -> Result<(), String> {
        for (r, failure) in self.records.iter_mut().zip(&self.failure) {
            if r.status != TriggerStatus::Armed
                || r.selector.is_some()
                || r.source != source
                || at < r.armed_at_ns
                || at >= r.match_deadline_ns
            {
                continue;
            }
            r.seen = r.seen.checked_add(1).ok_or("trigger byte count overflow")?;
            while r.prefix > 0 && byte != r.marker[r.prefix] {
                r.prefix = failure[r.prefix - 1];
            }
            if byte == r.marker[r.prefix] {
                r.prefix += 1;
            }
            if r.prefix == r.marker.len() {
                let effect = at.checked_add(latency).ok_or("trigger effect overflow")?;
                effect
                    .checked_add(PROCESS_EFFECT_TIMEOUT_NS)
                    .ok_or("trigger effect deadline overflow")?;
                if effect < max_time {
                    return Err("trigger effect is in committed machine past".into());
                }
                r.status = TriggerStatus::Matched;
                r.matched_at_ns = Some(at);
                r.effect_at_ns = Some(effect);
            }
        }
        self.reclaim();
        Ok(())
    }
    pub fn boundary(&self) -> u64 {
        self.records
            .iter()
            .filter_map(|r| match r.status {
                TriggerStatus::Armed => Some(r.match_deadline_ns),
                TriggerStatus::Matched => r.effect_at_ns,
                _ => None,
            })
            .min()
            .unwrap_or(u64::MAX)
    }
    /// Equal-time effects follow arm ID, independent of source-machine ties.
    /// Returns the input keys of typed triggers that fired, in that order.
    pub fn apply_boundary(
        &mut self,
        faults: &mut crate::faults::Faults,
        now: u64,
    ) -> Result<Vec<String>, String> {
        let mut fired = Vec::new();
        for r in &mut self.records {
            if r.status == TriggerStatus::Armed && r.match_deadline_ns == now {
                r.status = TriggerStatus::Expired;
            }
            if r.status == TriggerStatus::Matched && r.effect_at_ns == Some(now) {
                // Typed effects meet the same state checks as timed inputs.
                let refused = if r.selector.is_some() {
                    crate::input_program::validate_override_state(faults, &r.action).err()
                } else {
                    faults.admission_error(&r.action).map(Into::into)
                };
                if let Some(reason) = refused {
                    r.status = TriggerStatus::Rejected;
                    r.rejection_reason = Some(reason);
                    continue;
                }
                r.fault_id = Some(
                    faults.admit(
                        Plan {
                            id: 0,
                            at_ns: now,
                            deadline_ns: now
                                .checked_add(PROCESS_EFFECT_TIMEOUT_NS)
                                .ok_or("trigger effect deadline overflow")?,
                            action: r.action.clone(),
                        },
                        now,
                    )?,
                );
                r.status = TriggerStatus::Fired;
                fired.extend(r.key.clone());
            }
        }
        self.reclaim();
        Ok(fired)
    }
    pub fn reset_source(&mut self, source: &str) {
        for r in &mut self.records {
            if r.status == TriggerStatus::Armed && r.selector.is_none() && r.source == source {
                r.prefix = 0;
                r.resets += 1;
            }
        }
    }
    pub fn cut_off(&mut self, now: u64) {
        for r in &mut self.records {
            if r.status == TriggerStatus::Armed || r.status == TriggerStatus::Matched {
                r.cut_off_at_ns = Some(now);
                r.status = TriggerStatus::CutOff;
            }
        }
        self.reclaim();
    }
    pub fn hash_into(&self, hash: &mut Sha256) -> Result<(), String> {
        if let Some(evidence) = self.evidence()? {
            hash.update(b"seedbox-committed-triggers-v1\0");
            hash.update(serde_json::to_vec(&evidence).map_err(|e| e.to_string())?);
        }
        Ok(())
    }
    /// None is essential: unused triggers add no bytes or report keys.
    pub fn evidence(&self) -> Result<Option<Value>, String> {
        if self.records.is_empty() {
            Ok(None)
        } else {
            serde_json::to_value(&self.records)
                .map(Some)
                .map_err(|e| e.to_string())
        }
    }
}
