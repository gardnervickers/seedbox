//! Version 2 record identities are separate from public text and pN numbering.
use super::*;
wire_tuple_struct!(Handle);
wire_tuple_struct!(RecordId);
wire_enum!(Text;units{}tuples{0=>Allocated(v0),1=>Named(v0)}records{});
wire_struct!(Allocator { last });
wire_enum!(Phase; units {0=>Pending,1=>Active,2=>Healing,3=>Lost,4=>PendingHealing} tuples {} records {});
wire_enum!(Change; units {3=>Partition,4=>Unpartition,6=>Unshape,7=>Select,8=>Unselect,10=>Unfault} tuples {0=>Execution(v0),1=>Drift(v0),2=>Step(v0),5=>Shape(v0,v1),9=>Fault(v0)} records {});
wire_enum!(HandleTarget; units {} tuples {0=>Machine(v0),3=>Connection(v0)} records {1=>Links{left,right},2=>DirectedLinks{from,to}});
wire_struct!(Record {
    text,
    target,
    change,
    phase,
    revision
});
wire_enum!(Transition; units {0=>Apply,1=>Heal} tuples {} records {});
wire_struct!(ExecutionPlan {
    record_id,
    handle,
    target,
    change,
    lost,
    transition,
    revision
});
wire_struct!(Handles {
    allocator,
    last_record,
    records,
    poisoned
});
impl Handles {
    pub(crate) fn check_owned_input_persist(
        &self,
        id: RecordId,
        text: &str,
        settled: bool,
    ) -> crate::persistence::Result<()> {
        crate::persistence::require(
            self.records
                .get(&id)
                .is_some_and(|r| r.text.matches(text) && (!settled || !r.phase.unapplied())),
            "input owned record",
        )
    }
    pub(crate) fn check_persist(
        &self,
        exists: &impl Fn(&str) -> bool,
    ) -> crate::persistence::Result<()> {
        use crate::persistence::require;
        require(!self.poisoned, "poisoned handles")?;
        let mut active = std::collections::BTreeSet::new();
        let mut allocated = std::collections::BTreeSet::new();
        for (id, r) in &self.records {
            require(id.0 > 0 && id.0 <= self.last_record, "record allocator")?;
            match &r.text {
                Text::Allocated(n) => require(
                    n.0 > 0 && n.0 <= self.allocator.last && allocated.insert(n.0),
                    "handle allocator",
                )?,
                Text::Named(n) => require(!n.is_empty(), "empty named handle")?,
            }
            require(
                r.phase.unapplied() || active.insert(r.text.to_string()),
                "duplicate active handle",
            )?;
            if let Change::Step(n) = r.change {
                require(
                    n.unsigned_abs() <= crate::oracle::MAX_CLOCK_OFFSET_NS,
                    "realtime handle range",
                )?;
            }
            require(
                !matches!(r.phase, Phase::Lost | Phase::PendingHealing)
                    || matches!(r.change, Change::Step(_)),
                "lost or queued-heal nonstep handle",
            )?;
            require(
                !matches!(
                    r.change,
                    Change::Unpartition | Change::Unshape | Change::Unselect | Change::Unfault
                ),
                "inverse installed handle",
            )?;
            require(
                matches!(
                    (&r.target, r.change),
                    (
                        HandleTarget::Machine(_),
                        Change::Execution(_)
                            | Change::Drift(_)
                            | Change::Step(_)
                            | Change::Fault(_)
                    ) | (HandleTarget::Links { .. }, Change::Partition)
                        | (HandleTarget::DirectedLinks { .. }, Change::Shape(..))
                        | (HandleTarget::Connection(_), Change::Select)
                ),
                "handle target/change",
            )?;
            match &r.target {
                HandleTarget::Machine(n) => require(exists(n), "handle target")?,
                HandleTarget::Links { left, right }
                | HandleTarget::DirectedLinks {
                    from: left,
                    to: right,
                } => {
                    require(!left.is_empty() && !right.is_empty(), "handle link sets")?;
                    for n in left.iter().chain(right) {
                        require(exists(n), "handle member")?;
                    }
                }
                HandleTarget::Connection(c) => require(
                    !c.from.is_empty()
                        && c.from != c.to
                        && c.src_port != 0
                        && c.dst_port != 0
                        && c.incarnation != 0,
                    "handle connection",
                )?,
            }
        }
        Ok(())
    }
    pub(crate) fn check_plan_persist(&self, p: &ExecutionPlan) -> crate::persistence::Result<()> {
        crate::persistence::require(
            self.records.get(&p.record_id).is_some_and(|r| {
                r.text == p.handle
                    && r.target == p.target
                    && match p.transition {
                        Transition::Apply => r.phase.unapplied() && p.revision == 0,
                        Transition::Heal => {
                            r.revision == p.revision
                                && matches!(r.phase, Phase::Healing | Phase::PendingHealing)
                        }
                    }
                    && Some(p.change)
                        == match (p.transition, p.lost) {
                            (Transition::Apply, _) => Some(r.change),
                            (Transition::Heal, false) => Self::inverse(r.change),
                            (Transition::Heal, true) => Some(Change::Step(0)),
                        }
            }),
            "handle plan/revision",
        )
    }
}
