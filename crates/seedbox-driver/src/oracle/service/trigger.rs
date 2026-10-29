//! Trigger matching over committed bytes and held block requests, and
//! admission of fired trigger faults.
use super::*;
use crate::oracle::triggers::Watch;

impl Service {
    pub(crate) fn committed_bytes(
        &mut self,
        source: &str,
        observed: u64,
        bytes: &[u8],
        commit: u64,
    ) -> Result<(), Failure> {
        self.valid()?;
        self.triggers
            .committed(source, observed, bytes, commit)
            .map(|_| ())
            .map_err(Into::into)
    }
    /// `source` holds the disk request block_request trigger `id` watches.
    pub(crate) fn block_held(
        &mut self,
        id: u32,
        source: &str,
        observed: u64,
        commit: u64,
    ) -> Result<(), Failure> {
        self.valid()?;
        self.triggers
            .held(id, source, observed, commit)
            .map(|_| ())
            .map_err(Into::into)
    }
    /// The plan that stops `trigger`'s machine hold, if it watches one.
    pub(super) fn release(&self, trigger: u32) -> Option<Plan> {
        let record = self.triggers.record(trigger)?;
        matches!(record.watch, Watch::BlockRequest { .. }).then(|| Plan::Release {
            source: record.source.clone(),
            trigger,
        })
    }
    pub(crate) fn trigger_effects(
        &mut self,
        adapter: &mut impl Adapter,
        at: u64,
    ) -> Result<Vec<Plan>, Failure> {
        self.valid()?;
        let mut plans = vec![];
        for effect in self.triggers.due(at) {
            // The resolution causes any fault admission; publish it first.
            let mark = self.notices.len();
            let resolved = |status, fault| Notice::TriggerResolved {
                trigger: effect.trigger_id,
                at,
                status,
                fault,
            };
            let result = self.prepare(
                adapter,
                AdmissionMode::Trigger,
                Requester::Trigger(effect.trigger_id),
                None,
                match effect.action {
                    TriggerAction::Stop(target) => Command::Effect {
                        target: Target::Key(target),
                        effect: Effect::Process(ApplicationCommand::Stop),
                    },
                    // Members are re-resolved by the identical admission rules.
                    TriggerAction::Partition { left, right } => Command::Partition {
                        left: left.into_iter().map(Target::Key).collect(),
                        right: right.into_iter().map(Target::Key).collect(),
                    },
                    // Undelayed, while the held request is still unwritten.
                    TriggerAction::Crash(target) => Command::Effect {
                        target: Target::Key(target),
                        effect: Effect::Crash,
                    },
                },
                at,
                PROCESS_EFFECT_TIMEOUT_NS,
            );
            match result {
                Ok((id, p)) => {
                    let handle = self.pending.get(&id).and_then(Self::handle);
                    self.triggers
                        .resolve(effect.trigger_id, at, Ok((id, handle)))?;
                    self.notices.insert(
                        mark,
                        resolved(crate::oracle::triggers::Status::Fired, Some(id)),
                    );
                    plans.extend(p);
                }
                Err(PreparationError::Rejected(e)) => {
                    self.triggers
                        .resolve(effect.trigger_id, at, Err(e.message))?;
                    self.notices.insert(
                        mark,
                        resolved(crate::oracle::triggers::Status::Rejected, None),
                    );
                }
                Err(PreparationError::Fatal(mut e)) => {
                    // A preparation failure after policy admission can already
                    // carry the new fault's Failed settlement. Fired records
                    // admission, not a successful guest effect.
                    if let Some(settlement) = e.settlements.first() {
                        let fault = settlement.fault;
                        self.triggers.resolve(
                            effect.trigger_id,
                            at,
                            Ok((fault, settlement.handle.clone())),
                        )?;
                        self.notices.insert(
                            mark,
                            resolved(crate::oracle::triggers::Status::Fired, Some(fault)),
                        );
                    }
                    e.retain_before(plans);
                    return Err(e);
                }
            }
        }
        Ok(plans)
    }
    pub(crate) fn source_reset(&mut self, source: &str) -> Result<(), Failure> {
        self.valid()?;
        self.triggers.reset_source(source).map_err(Into::into)
    }
    /// Expire due unmatched triggers, returning the plans that release the
    /// machine holds of expired block_request triggers.
    pub(crate) fn expire_triggers(&mut self, at: u64) -> Vec<Plan> {
        let mut releases = vec![];
        for trigger in self.triggers.expire(at) {
            self.notices.push(Notice::TriggerResolved {
                trigger,
                at,
                status: crate::oracle::triggers::Status::Expired,
                fault: None,
            });
            releases.extend(self.release(trigger));
        }
        releases
    }
    pub(crate) fn cut_off_triggers(&mut self, at: u64) {
        for trigger in self.triggers.cut_off(at) {
            self.notices.push(Notice::TriggerResolved {
                trigger,
                at,
                status: crate::oracle::triggers::Status::CutOff,
                fault: None,
            });
        }
    }
    #[cfg(test)]
    pub(crate) fn triggers(&self) -> &crate::oracle::triggers::Triggers {
        &self.triggers
    }
    pub(crate) fn trigger_boundary(&self) -> Option<u64> {
        self.triggers.boundary()
    }
}
