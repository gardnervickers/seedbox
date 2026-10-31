//! Device faults the machine owns across boots. The worker of a live boot
//! draws, counts and holds; this mirror keeps the installed faults, the
//! counts of boots that ended and the holds still counting, so each new
//! worker starts from it and heals report counts over every boot.
use super::protocol::{BootFaults, FaultReport, InstalledFault};
use seedbox_machine::machine::disk::fault::Hold;
use seedbox_machine::pv::{DeviceFault, FaultCounts};

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct Faults {
    /// Installed faults in identity order, with the counts of ended boots.
    installed: Vec<Installed>,
    /// Holds still counting, as of the last ended boot.
    holds: Vec<Hold>,
    /// Final counts of healed faults not yet taken.
    healed: Vec<(u64, FaultCounts)>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Installed {
    id: u64,
    seed: u64,
    fault: DeviceFault,
    ended: FaultCounts,
}

fn add(a: FaultCounts, b: FaultCounts) -> Result<FaultCounts, String> {
    Ok(FaultCounts {
        matched: a
            .matched
            .checked_add(b.matched)
            .ok_or("PV fault count overflow")?,
        affected: a
            .affected
            .checked_add(b.affected)
            .ok_or("PV fault count overflow")?,
    })
}

impl Faults {
    pub(super) fn install(&mut self, id: u64, seed: u64, fault: DeviceFault) -> Result<(), String> {
        if self.installed.iter().any(|f| f.id >= id) || self.healed.iter().any(|(h, _)| *h == id) {
            return Err("PV fault identity reused or out of order".into());
        }
        if matches!(fault, DeviceFault::Preempt { .. })
            && self
                .installed
                .iter()
                .any(|f| matches!(f.fault, DeviceFault::Preempt { .. }))
        {
            return Err("PV machine already has a preempt fault".into());
        }
        self.installed.push(Installed {
            id,
            seed,
            fault,
            ended: FaultCounts::default(),
        });
        Ok(())
    }
    pub(super) fn installed(&self, id: u64) -> Option<InstalledFault> {
        self.installed
            .iter()
            .find(|f| f.id == id)
            .map(|f| InstalledFault {
                id: f.id,
                seed: f.seed,
                fault: f.fault,
            })
    }
    /// Heal `id`, adding the live boot's counts to those of ended boots. A
    /// preempt fault counts nothing, so its heal reports no counts.
    pub(super) fn heal(&mut self, id: u64, live: FaultCounts) -> Result<(), String> {
        let index = self
            .installed
            .iter()
            .position(|f| f.id == id)
            .ok_or("PV heal of an uninstalled fault")?;
        let fault = self.installed.remove(index);
        let counts = add(fault.ended, live)?;
        if !matches!(fault.fault, DeviceFault::Preempt { .. }) {
            self.healed.push((id, counts));
        }
        Ok(())
    }
    pub(super) fn take(&mut self, id: u64) -> Option<FaultCounts> {
        let index = self.healed.iter().position(|(h, _)| *h == id)?;
        Some(self.healed.remove(index).1)
    }
    pub(super) fn hold(&mut self, hold: Hold) -> Result<(), String> {
        if self.holds.iter().any(|h| h.trigger == hold.trigger) || hold.nth == 0 {
            return Err("PV block request hold reused or empty".into());
        }
        self.holds.push(hold);
        Ok(())
    }
    pub(super) fn release(&mut self, trigger: u32) {
        self.holds.retain(|h| h.trigger != trigger);
    }
    /// The hold of `trigger` took its request in the live boot.
    pub(super) fn held(&mut self, trigger: u32) {
        self.release(trigger);
    }
    /// Whether a live boot has anything to report: an installed fault or a
    /// hold still counting.
    pub(super) fn live(&self) -> bool {
        !self.installed.is_empty() || !self.holds.is_empty()
    }
    /// What a fresh worker for physical boot `boot` starts with.
    pub(super) fn boot(&self, boot: u64) -> BootFaults {
        BootFaults {
            boot,
            installed: self
                .installed
                .iter()
                .map(|f| InstalledFault {
                    id: f.id,
                    seed: f.seed,
                    fault: f.fault,
                })
                .collect(),
            holds: self.holds.clone(),
        }
    }
    /// Fold an ending boot's final report into the counts of ended boots.
    pub(super) fn retire(&mut self, report: &FaultReport) -> Result<(), String> {
        for (id, counts) in &report.counts {
            let fault = self
                .installed
                .iter_mut()
                .find(|f| f.id == *id)
                .ok_or("PV worker counted an uninstalled fault")?;
            fault.ended = add(fault.ended, *counts)?;
        }
        if report
            .holds
            .iter()
            .any(|h| !self.holds.iter().any(|m| m.trigger == h.trigger))
        {
            return Err("PV worker reported an unknown hold".into());
        }
        self.holds = report.holds.clone();
        Ok(())
    }
    pub(super) fn validate(&self) -> Result<(), String> {
        let mut previous = None;
        let mut preempts = 0;
        for f in &self.installed {
            if previous.is_some_and(|p| p >= f.id) || f.ended.affected > f.ended.matched {
                return Err("PV retained fault order or counts".into());
            }
            preempts += usize::from(matches!(f.fault, DeviceFault::Preempt { .. }));
            previous = Some(f.id);
        }
        let mut triggers = std::collections::BTreeSet::new();
        if preempts > 1
            || self
                .holds
                .iter()
                .any(|h| h.nth == 0 || h.seen >= h.nth || !triggers.insert(h.trigger))
        {
            return Err("PV retained preempt or holds".into());
        }
        Ok(())
    }
}

seedbox_machine::record!(Installed {
    id: u64,
    seed: u64,
    fault: DeviceFault,
    ended: FaultCounts
});
seedbox_machine::record!(Faults {
    installed: Vec<Installed>,
    holds: Vec<Hold>,
    healed: Vec<(u64, FaultCounts)>
});

#[cfg(test)]
mod tests {
    use super::*;
    use seedbox_machine::pv::{DiskEffect, DiskOp, DiskScope};

    #[test]
    fn counts_accumulate_over_boots_and_heal_once() {
        let mut faults = Faults::default();
        let fault = DeviceFault::DiskError {
            op: DiskOp::Any,
            ppm: 5,
            effect: DiskEffect::Eio,
            scope: DiskScope::All,
        };
        faults.install(2, 7, fault).unwrap();
        assert!(faults.install(2, 7, fault).is_err());
        faults
            .install(3, 8, DeviceFault::Preempt { ppm: 9 })
            .unwrap();
        assert!(faults
            .install(4, 8, DeviceFault::Preempt { ppm: 9 })
            .is_err());
        faults
            .hold(Hold {
                trigger: 1,
                op: DiskOp::Flush,
                nth: 3,
                seen: 0,
                until: None,
            })
            .unwrap();
        let counts = |matched, affected| FaultCounts { matched, affected };
        let report = FaultReport {
            counts: vec![(2, counts(4, 1))],
            holds: vec![Hold {
                trigger: 1,
                op: DiskOp::Flush,
                nth: 3,
                seen: 2,
                until: None,
            }],
            healed: None,
        };
        faults.retire(&report).unwrap();
        assert_eq!(faults.boot(5).holds[0].seen, 2);
        assert_eq!(faults.boot(5).installed.len(), 2);
        faults.heal(2, counts(1, 1)).unwrap();
        assert_eq!(faults.take(2), Some(counts(5, 2)));
        assert_eq!(faults.take(2), None);
        faults.heal(3, counts(0, 0)).unwrap();
        assert_eq!(faults.take(3), None, "preempt counts nothing");
        faults.validate().unwrap();
        assert!(faults
            .retire(&FaultReport {
                counts: vec![(2, counts(1, 0))],
                ..Default::default()
            })
            .is_err());
    }
}
