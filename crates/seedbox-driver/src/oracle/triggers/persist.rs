//! Field and tag order is the checkpoint encoding; changing it needs a
//! checkpoint `VERSION` bump.
use super::*;
wire_enum!(Status; units {0=>Armed,1=>Cancelled,2=>Matched,3=>Expired,4=>Rejected,5=>Fired,6=>CutOff} tuples {} records {});
wire_enum!(TriggerAction; units {} tuples {0=>Stop(v0)} records {1=>Partition{left,right}});
wire_struct!(Record {
    id,
    source,
    marker,
    action,
    armed_at,
    match_deadline,
    prefix,
    seen,
    resets,
    matched_at,
    effect_at,
    fault_id,
    cancelled_at,
    cut_off_at,
    status,
    rejection_reason,
    handle
});
wire_struct!(Triggers { records, failure });

impl Triggers {
    pub(crate) fn check_persist(
        &self,
        now: u64,
        exists: &impl Fn(&str) -> bool,
    ) -> crate::persistence::Result<()> {
        use crate::persistence::require;
        require(
            self.records.len() <= MAX_TRIGGERS && self.failure.len() == self.records.len(),
            "trigger tables",
        )?;
        for (i, r) in self.records.iter().enumerate() {
            require(
                r.id as usize == i + 1
                    && exists(&r.source)
                    && !r.marker.is_empty()
                    && r.marker.len() <= MAX_MARKER
                    && r.prefix <= r.marker.len()
                    && (r.status != Status::Armed || r.prefix < r.marker.len())
                    && r.armed_at <= now
                    && r.match_deadline > r.armed_at
                    && r.match_deadline
                        .checked_add(PROCESS_EFFECT_TIMEOUT_NS)
                        .is_some()
                    && r.effect_at.is_none_or(|at| {
                        at <= now && at.checked_add(PROCESS_EFFECT_TIMEOUT_NS).is_some()
                    })
                    && r.matched_at.is_none_or(|at| at <= now)
                    && r.cancelled_at.is_none_or(|at| at <= now)
                    && r.cut_off_at.is_none_or(|at| at <= now)
                    && (r.handle.is_some()
                        == (r.status == Status::Fired
                            && matches!(r.action, TriggerAction::Partition { .. })))
                    && r.handle.as_ref().is_none_or(|handle| !handle.is_empty())
                    && (r.status != Status::Fired || r.fault_id.is_some_and(|id| id != 0)),
                "trigger record",
            )?;
            let mut failure = [0usize; MAX_MARKER];
            for j in 1..r.marker.len() {
                let mut p = failure[j - 1];
                while p > 0 && r.marker[j] != r.marker[p] {
                    p = failure[p - 1];
                }
                if r.marker[j] == r.marker[p] {
                    p += 1;
                }
                failure[j] = p;
            }
            require(
                failure[..r.marker.len()] == self.failure[i],
                "trigger failure table",
            )?;
        }
        Ok(())
    }
}
