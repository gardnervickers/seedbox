//! Private streaming trigger state, independent of source-channel policy.
//!
//! The caller resolves machine names and feeds the ordered union of committed
//! stdout, stderr and raw console bytes, preserving their observation times.
//! A match returns a Stop plan at the current committed barrier, not back at the
//! byte's earlier observation time. This component neither routes nor mutates
//! Driver. The caller owns nondecreasing commit progress, future expiry barriers
//! and arm-ID-ordered admission through `due`/`resolve`. Guest command completion
//! remains separate from admitting a trigger effect.

// The public limits, which program-armed triggers will share with these.
use super::PROCESS_EFFECT_TIMEOUT_NS;
use seedbox_engine_protocol::control::{MAX_TRIGGERS, MAX_TRIGGER_MARKER as MAX_MARKER};

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

/// The fault a matched trigger admits; member names are already resolved.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum TriggerAction {
    /// Stop the named machine's application.
    Stop(String),
    /// Partition these sorted, disjoint member sets.
    Partition {
        left: Vec<String>,
        right: Vec<String>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Record {
    pub(crate) id: u32,
    pub(crate) source: String,
    pub(crate) marker: Vec<u8>,
    pub(crate) action: TriggerAction,
    pub(crate) armed_at: u64,
    pub(crate) match_deadline: u64,
    pub(crate) prefix: usize,
    pub(crate) seen: u64,
    pub(crate) resets: u64,
    pub(crate) matched_at: Option<u64>,
    pub(crate) effect_at: Option<u64>,
    pub(crate) fault_id: Option<u32>,
    // Guest inspection needs the actual opaque partition
    // handle after admission; fault IDs use a separate allocator. Retaining it
    // here avoids preallocation at arm time or client-side handle reconstruction.
    /// Actual partition handle allocated when this trigger fires; absent for stops.
    pub(crate) handle: Option<String>,
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
    pub(crate) action: TriggerAction,
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
        action: TriggerAction,
    ) -> Result<u32, String> {
        if self.records.len() >= MAX_TRIGGERS {
            return Err(format!("trigger budget exhausted ({MAX_TRIGGERS} per run)"));
        }
        if marker.is_empty() || marker.len() > MAX_MARKER {
            return Err(format!("marker must contain 1..={MAX_MARKER} bytes"));
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
            action,
            armed_at: now,
            match_deadline: deadline,
            prefix: 0,
            seen: 0,
            resets: 0,
            matched_at: None,
            effect_at: None,
            fault_id: None,
            handle: None,
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
                        action: record.action.clone(),
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
                action: record.action.clone(),
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
    /// Returns the newly expired IDs in arm order.
    pub(crate) fn expire(&mut self, now: u64) -> Vec<u32> {
        let mut expired = Vec::new();
        for record in &mut self.records {
            if record.status == Status::Armed && record.match_deadline <= now {
                record.status = Status::Expired;
                expired.push(record.id);
            }
        }
        expired
    }

    /// Mark policy admission (not application completion) at the scheduled time.
    pub(crate) fn resolve(
        &mut self,
        id: u32,
        at: u64,
        result: Result<(u32, Option<String>), String>,
    ) -> Result<(), String> {
        let record = self.record_mut(id)?;
        if record.status != Status::Matched || record.effect_at != Some(at) {
            return Err("trigger effect admission at wrong state or time".into());
        }
        match result {
            Ok((fault_id, handle)) => {
                let partition = matches!(record.action, TriggerAction::Partition { .. });
                if fault_id == 0
                    || partition != handle.is_some()
                    || handle.as_ref().is_some_and(|value| value.is_empty())
                {
                    return Err("trigger admission lacks its actual partition handle".into());
                }
                record.status = Status::Fired;
                record.fault_id = Some(fault_id);
                record.handle = handle;
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

    /// Returns the newly cut-off IDs in arm order.
    pub(crate) fn cut_off(&mut self, now: u64) -> Vec<u32> {
        let mut cut = Vec::new();
        for record in &mut self.records {
            if matches!(record.status, Status::Armed | Status::Matched) {
                record.status = Status::CutOff;
                record.cut_off_at = Some(now);
                cut.push(record.id);
            }
        }
        cut
    }

    fn record_mut(&mut self, id: u32) -> Result<&mut Record, String> {
        self.records
            .get_mut(id.checked_sub(1).ok_or("unknown trigger ID")? as usize)
            .ok_or_else(|| "unknown trigger ID".into())
    }
}

#[cfg(test)]
mod tests;

mod persist;
