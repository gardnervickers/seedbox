//! Private streaming trigger state, independent of source-channel policy.
//!
//! The caller resolves machine names and feeds the ordered union of committed
//! stdout, stderr and raw console bytes, preserving their observation times.
//! A match returns a Stop plan at the current committed barrier, not back at the
//! byte's earlier observation time. This component neither routes nor mutates
//! Driver. The caller owns nondecreasing commit progress, future expiry barriers
//! and arm-ID-ordered admission through `due`/`resolve`. Guest command completion
//! remains separate from admitting a trigger effect.

const MAX_TRIGGERS: usize = 64;
const MAX_MARKER: usize = 256;
use super::PROCESS_EFFECT_TIMEOUT_NS;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Status {
    Armed,
    Cancelled,
    Matched,
    Expired,
    Rejected,
    Fired,
    CutOff,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Record {
    pub(crate) id: u32,
    pub(crate) source: String,
    pub(crate) marker: Vec<u8>,
    pub(crate) target: String,
    pub(crate) armed_at: u64,
    pub(crate) match_deadline: u64,
    pub(crate) prefix: usize,
    pub(crate) seen: u64,
    pub(crate) resets: u64,
    pub(crate) matched_at: Option<u64>,
    pub(crate) effect_at: Option<u64>,
    pub(crate) fault_id: Option<u32>,
    pub(crate) cancelled_at: Option<u64>,
    pub(crate) cut_off_at: Option<u64>,
    pub(crate) status: Status,
    pub(crate) rejection_reason: Option<String>,
}

/// A checked application Stop effect at its discovery commit barrier. Names are already resolved by the
/// caller; the plan grants no permission to apply into a peer's committed past.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Effect {
    pub(crate) trigger_id: u32,
    pub(crate) target: String,
    pub(crate) at: u64,
    pub(crate) deadline: u64,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct Triggers {
    records: Vec<Record>,
    failure: Vec<Vec<usize>>,
}
impl Triggers {
    pub(crate) fn records(&self) -> &[Record] {
        &self.records
    }

    pub(crate) fn arm(
        &mut self,
        source: String,
        marker: Vec<u8>,
        now: u64,
        timeout: u64,
        target: String,
    ) -> Result<u32, String> {
        if self.records.len() >= MAX_TRIGGERS {
            return Err("trigger budget exhausted (64 per run)".into());
        }
        if marker.is_empty() || marker.len() > MAX_MARKER {
            return Err("marker must contain 1..=256 bytes".into());
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
            let mut prefix = failure[i - 1];
            while prefix > 0 && marker[i] != marker[prefix] {
                prefix = failure[prefix - 1];
            }
            if marker[i] == marker[prefix] {
                prefix += 1;
            }
            failure[i] = prefix;
        }
        let id = self.records.len() as u32 + 1;
        self.records.push(Record {
            id,
            source,
            marker,
            target,
            armed_at: now,
            match_deadline: deadline,
            prefix: 0,
            seen: 0,
            resets: 0,
            matched_at: None,
            effect_at: None,
            fault_id: None,
            cancelled_at: None,
            cut_off_at: None,
            status: Status::Armed,
            rejection_reason: None,
        });
        self.failure.push(failure);
        Ok(id)
    }

    /// Feed a committed chunk with its actual canonical observation timestamp.
    /// Observation cannot exceed the current commit barrier. Older-than-arm and
    /// at/after-deadline bytes cannot seed a prefix. Matches preserve observation
    /// time but admit at this commit, without artificial latency or backdating.
    /// Feed the full committed batch before `due` chooses arm-ID admission order.
    /// Errors provide no rollback promise; the owner must terminate processing.
    pub(crate) fn committed(
        &mut self,
        source: &str,
        at: u64,
        bytes: &[u8],
        committed_at: u64,
    ) -> Result<Vec<Effect>, String> {
        if at > committed_at {
            return Err("trigger observation exceeds commit barrier".into());
        }
        let mut effects = Vec::new();
        for (record, failure) in self.records.iter_mut().zip(&self.failure) {
            if record.status != Status::Armed
                || record.source != source
                || at < record.armed_at
                || at >= record.match_deadline
            {
                continue;
            }
            for &byte in bytes {
                let seen = record
                    .seen
                    .checked_add(1)
                    .ok_or("trigger byte count overflow")?;
                let mut prefix = record.prefix;
                while prefix > 0 && byte != record.marker[prefix] {
                    prefix = failure[prefix - 1];
                }
                if byte == record.marker[prefix] {
                    prefix += 1;
                }
                if prefix == record.marker.len() {
                    let effect = committed_at;
                    let deadline = effect
                        .checked_add(PROCESS_EFFECT_TIMEOUT_NS)
                        .ok_or("trigger effect deadline overflow")?;
                    record.seen = seen;
                    record.prefix = prefix;
                    record.status = Status::Matched;
                    record.matched_at = Some(at);
                    record.effect_at = Some(effect);
                    effects.push(Effect {
                        trigger_id: record.id,
                        target: record.target.clone(),
                        at: effect,
                        deadline,
                    });
                    break;
                }
                record.seen = seen;
                record.prefix = prefix;
            }
        }
        Ok(effects)
    }

    /// Effects ready for admission in arm-ID order, independent of the order
    /// in which different sources matched. Resolve each before advancing time;
    /// repeated observation does not itself admit or consume an effect.
    pub(crate) fn due(&self, now: u64) -> Vec<Effect> {
        self.records
            .iter()
            .filter(|record| record.status == Status::Matched && record.effect_at == Some(now))
            .map(|record| Effect {
                trigger_id: record.id,
                target: record.target.clone(),
                at: now,
                deadline: now + PROCESS_EFFECT_TIMEOUT_NS, /* checked when the match was recorded */
            })
            .collect()
    }

    pub(crate) fn cancel(&mut self, id: u32, now: u64) -> Result<(), String> {
        let record = self.record_mut(id)?;
        if record.status != Status::Armed || now < record.armed_at || now > record.match_deadline {
            return Err("trigger is no longer armed".into());
        }
        record.status = Status::Cancelled;
        record.cancelled_at = Some(now);
        Ok(())
    }

    /// Expire due unmatched triggers. Caller ordering decides whether a cancel
    /// at the exact deadline runs before this expiry; bytes at that time never match.
    pub(crate) fn expire(&mut self, now: u64) {
        for record in &mut self.records {
            if record.status == Status::Armed && record.match_deadline <= now {
                record.status = Status::Expired;
            }
        }
    }

    /// Mark policy admission (not application completion) at the scheduled time.
    pub(crate) fn resolve(
        &mut self,
        id: u32,
        at: u64,
        result: Result<u32, String>,
    ) -> Result<(), String> {
        let record = self.record_mut(id)?;
        if record.status != Status::Matched || record.effect_at != Some(at) {
            return Err("trigger effect admission at wrong state or time".into());
        }
        match result {
            Ok(fault_id) => {
                record.status = Status::Fired;
                record.fault_id = Some(fault_id);
            }
            Err(reason) => {
                record.status = Status::Rejected;
                record.rejection_reason = Some(reason);
            }
        }
        Ok(())
    }

    pub(crate) fn boundary(&self) -> Option<u64> {
        self.records
            .iter()
            .filter_map(|record| match record.status {
                Status::Armed => Some(record.match_deadline),
                Status::Matched => record.effect_at,
                _ => None,
            })
            .min()
    }

    /// A source boot change clears partial matching, not the armed trigger.
    pub(crate) fn reset_source(&mut self, source: &str) -> Result<(), String> {
        for record in &mut self.records {
            if record.status == Status::Armed && record.source == source {
                let resets = record
                    .resets
                    .checked_add(1)
                    .ok_or("trigger reset count overflow")?;
                record.prefix = 0;
                record.resets = resets;
            }
        }
        Ok(())
    }

    pub(crate) fn cut_off(&mut self, now: u64) {
        for record in &mut self.records {
            if matches!(record.status, Status::Armed | Status::Matched) {
                record.status = Status::CutOff;
                record.cut_off_at = Some(now);
            }
        }
    }

    fn record_mut(&mut self, id: u32) -> Result<&mut Record, String> {
        self.records
            .get_mut(id.checked_sub(1).ok_or("unknown trigger ID")? as usize)
            .ok_or_else(|| "unknown trigger ID".into())
    }
}

#[cfg(test)]
mod tests;
