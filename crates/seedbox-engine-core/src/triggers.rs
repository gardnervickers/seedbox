//! Bounded streaming matchers. Only the machine's committed UART sink feeds
//! these; no speculative bytes, host log tail, packet inference or wall clock.
use seedbox_control::{
    check_trigger_budget, valid_trigger_marker, Action, Plan, TriggerRecord, TriggerStatus,
    MAX_TRIGGER_MARKER, PROCESS_EFFECT_TIMEOUT_NS,
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
        Ok(restored)
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
        check_trigger_budget(self.records.len())?;
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
        });
        self.failure.push(failure);
        Ok(id)
    }
    pub fn observes(&self, source: &str) -> bool {
        self.records
            .iter()
            .any(|r| r.status == TriggerStatus::Armed && r.source == source)
    }
    pub fn cancel(&mut self, id: u32, now: u64) -> Result<(), String> {
        let r = self
            .records
            .get_mut(id.checked_sub(1).ok_or("unknown trigger ID")? as usize)
            .ok_or("unknown trigger ID")?;
        if r.status != TriggerStatus::Armed || now < r.armed_at_ns || now > r.match_deadline_ns {
            return Err("trigger is no longer armed".into());
        }
        r.status = TriggerStatus::Cancelled;
        r.cancelled_at_ns = Some(now);
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
    pub fn apply_boundary(
        &mut self,
        faults: &mut crate::faults::Faults,
        now: u64,
    ) -> Result<(), String> {
        for r in &mut self.records {
            if r.status == TriggerStatus::Armed && r.match_deadline_ns == now {
                r.status = TriggerStatus::Expired;
            }
            if r.status == TriggerStatus::Matched && r.effect_at_ns == Some(now) {
                if let Some(reason) = faults.admission_error(&r.action) {
                    r.status = TriggerStatus::Rejected;
                    r.rejection_reason = Some(reason.into());
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
            }
        }
        Ok(())
    }
    pub fn reset_source(&mut self, source: &str) {
        for r in &mut self.records {
            if r.status == TriggerStatus::Armed && r.source == source {
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
