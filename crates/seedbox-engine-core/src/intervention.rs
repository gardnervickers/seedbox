//! Restricted interactive admission at a stopped world boundary. The host owns
//! persistence; this scope cannot advance machines or directly change lifecycle
//! and transport state.
use crate::faults::Faults;
use seedbox_control::{Action, Plan};
use seedbox_engine_protocol::host_control::Boundary;
pub use seedbox_engine_protocol::intervention::{History, Intervention};

pub struct Admission<'a> {
    faults: &'a mut Faults,
    program: Option<&'a mut crate::input_program::Program>,
    boundary: Boundary,
    changed: bool,
    checkpoint_available: bool,
    checkpoint_requested: bool,
    validate_action: &'a dyn Fn(&Action) -> Result<(), String>,
}

impl<'a> Admission<'a> {
    pub(crate) fn new(
        faults: &'a mut Faults,
        boundary: Boundary,
        validate_action: &'a dyn Fn(&Action) -> Result<(), String>,
    ) -> Self {
        Self {
            faults,
            program: None,
            boundary,
            changed: false,
            checkpoint_available: false,
            checkpoint_requested: false,
            validate_action,
        }
    }

    pub(crate) fn with_program(mut self, program: &'a mut crate::input_program::Program) -> Self {
        self.program = Some(program);
        self
    }
    pub(crate) fn with_checkpoint_support(mut self, available: bool) -> Self {
        self.checkpoint_available = available;
        self
    }

    /// Return the stopped world to its owner for snapshot capture. This only
    /// requests a segment stop; the owner acknowledges successful capture after
    /// the machine and coordinator snapshots have actually been saved.
    pub fn request_checkpoint(&mut self) -> Result<(), String> {
        self.active()?;
        if !self.checkpoint_available {
            return Err("checkpoint is unavailable at this segment boundary".into());
        }
        if self.changed {
            return Err("checkpoint cannot share a boundary with a new fault admission".into());
        }
        self.checkpoint_requested = true;
        Ok(())
    }

    pub(crate) fn checkpoint_requested(&self) -> bool {
        self.checkpoint_requested
    }

    /// The committed scheduler horizon covers all executed machine work.
    /// Newly admitted effects cannot precede it.
    pub fn earliest_effect_ns(&self) -> u64 {
        self.boundary.position.simulated_time_ns
    }

    pub fn propose(&self, request_id: String, plan: Plan) -> Result<Intervention, String> {
        self.active()?;
        if self
            .program
            .as_ref()
            .is_some_and(|program| program.contains_key(&request_id))
        {
            return Err(format!("duplicate input identity {request_id}"));
        }
        self.validate_action(&plan.action)?;
        let intervention = Intervention {
            request_id,
            position: self.boundary.position,
            plan: self.faults.propose(plan, self.earliest_effect_ns())?,
        };
        validate(&intervention)?;
        Ok(intervention)
    }

    /// The host must durably retain this record before calling commit, and must
    /// stop execution on any persistence or commit error. Revalidating here
    /// detects mismatched replay positions and intervening fault admissions.
    pub fn commit(&mut self, intervention: &Intervention) -> Result<(), String> {
        self.active()?;
        if self.checkpoint_requested {
            return Err("checkpoint already requested at this boundary".into());
        }
        if intervention.position != self.boundary.position {
            return Err("intervention position differs from the current boundary".into());
        }
        validate(intervention)?;
        self.validate_action(&intervention.plan.action)?;
        let at = self.earliest_effect_ns();
        if let Some(program) = &mut self.program {
            program.commit_live(
                self.faults,
                intervention.request_id.clone(),
                intervention.plan.clone(),
                at,
            )?;
        } else {
            self.faults.admit_exact(intervention.plan.clone(), at)?;
        }
        self.changed = true;
        Ok(())
    }

    pub(crate) fn changed(&self) -> bool {
        self.changed
    }

    fn validate_action(&self, action: &Action) -> Result<(), String> {
        (self.validate_action)(action)?;
        crate::input_program::validate_override_state(self.faults, action)
    }

    fn active(&self) -> Result<(), String> {
        if self.boundary.terminal {
            Err("simulation has completed".into())
        } else {
            Ok(())
        }
    }
}

fn validate(intervention: &Intervention) -> Result<(), String> {
    History {
        entries: vec![intervention.clone()],
        ..History::default()
    }
    .validate()
}

/// Reapply retained inputs at their original committed boundaries. Keep this
/// cursor across run segments; clone and restore it together with a world
/// snapshot when branching. A failed admission poisons the cursor because an
/// earlier entry at the same boundary may already have committed.
#[derive(Clone)]
pub struct Replay {
    history: History,
    next: usize,
    failed: bool,
}

impl Replay {
    /// Export only a healthy cursor at a fully consumed stopped boundary.
    pub fn checkpoint(
        &self,
        position: seedbox_engine_protocol::host_control::BoundaryPosition,
    ) -> Result<seedbox_engine_protocol::intervention::ReplayCheckpoint, String> {
        if self.failed {
            return Err("intervention replay previously failed".into());
        }
        let saved = seedbox_engine_protocol::intervention::ReplayCheckpoint {
            history: self.history.clone(),
            consumed: self.next,
            position,
        };
        saved.validate()?;
        Ok(saved)
    }

    pub fn from_checkpoint(
        saved: seedbox_engine_protocol::intervention::ReplayCheckpoint,
    ) -> Result<Self, String> {
        saved.validate()?;
        Ok(Self {
            history: saved.history,
            next: saved.consumed,
            failed: false,
        })
    }

    pub fn new(history: History) -> Result<Self, String> {
        history.validate()?;
        Ok(Self {
            history,
            next: 0,
            failed: false,
        })
    }

    /// Call before accepting live host controls at every stopped boundary.
    /// Playback controls may coexist; new faults must not alter this history.
    pub fn boundary(&mut self, admission: &mut Admission<'_>) -> Result<(), String> {
        if self.failed {
            return Err("intervention replay previously failed".into());
        }
        let result = self.apply(admission);
        if result.is_err() {
            self.failed = true;
        }
        result
    }

    fn apply(&mut self, admission: &mut Admission<'_>) -> Result<(), String> {
        while let Some(entry) = self.history.entries.get(self.next) {
            let current = admission.boundary.position;
            if entry.position.boundary_index > current.boundary_index {
                return Ok(());
            }
            if entry.position != current {
                return Err(format!(
                    "intervention {}: replay diverged from retained boundary {}",
                    entry.request_id, entry.position.boundary_index
                ));
            }
            admission
                .commit(entry)
                .map_err(|error| format!("intervention {}: {error}", entry.request_id))?;
            self.next += 1;
        }
        Ok(())
    }

    /// Required after the final segment. A run ending before a retained input
    /// is reached cannot be reported as successfully replaying that history.
    pub fn finish(&self) -> Result<(), String> {
        if self.failed {
            return Err("intervention replay previously failed".into());
        }
        if let Some(entry) = self.history.entries.get(self.next) {
            return Err(format!(
                "intervention {}: simulation ended before retained boundary {}",
                entry.request_id, entry.position.boundary_index
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use seedbox_engine_protocol::host_control::BoundaryPosition;

    fn boundary(index: u64) -> Boundary {
        Boundary {
            position: BoundaryPosition {
                boundary_index: index,
                completed_rounds: index,
                simulated_time_ns: index * 10,
            },
            terminal: false,
            application_ready: true,
        }
    }

    fn history() -> History {
        History {
            entries: vec![Intervention {
                request_id: "cut".into(),
                position: boundary(2).position,
                plan: Plan {
                    id: 1,
                    at_ns: 20,
                    deadline_ns: 20,
                    action: Action::Partition {
                        handle: "network".into(),
                        left: vec!["a".into()],
                        right: vec!["b".into()],
                    },
                },
            }],
            ..History::default()
        }
    }

    #[test]
    fn duplicate_override_identity_is_rejected_before_a_durable_proposal() {
        let input = crate::input_program::TimedInput {
            key: "existing".into(),
            at_ns: 100,
            action: Action::Partition {
                handle: "p".into(),
                left: vec!["a".into()],
                right: vec!["b".into()],
            },
            timeout_ns: 100,
        };
        let mut program = crate::input_program::Program::new(vec![input.clone()], 300, 0).unwrap();
        let mut faults = Faults::new(vec![], vec!["a".into(), "b".into()]).unwrap();
        let admission =
            Admission::new(&mut faults, boundary(2), &|_| Ok(())).with_program(&mut program);
        let error = admission
            .propose(
                "existing".into(),
                Plan {
                    id: 0,
                    at_ns: 20,
                    deadline_ns: 120,
                    action: input.action,
                },
            )
            .unwrap_err();
        assert_eq!(error, "duplicate input identity existing");
        assert!(faults.log.is_empty());
        assert!(program.admissions().is_empty());
    }

    #[test]
    fn admission_rejects_invalid_state_before_proposal_or_commit_without_mutation() {
        for (initial, action, error) in [
            (
                vec![],
                Action::Heal {
                    handle: "typo".into(),
                },
                "heal of inactive handle",
            ),
            (
                vec![
                    serde_json::json!({"id":1,"at_ns":10,"deadline_ns":10,"action":"pause","machine":"a"}),
                ],
                Action::Stop {
                    machine: "a".into(),
                },
                "target machine is paused",
            ),
            (
                vec![
                    serde_json::json!({"id":1,"at_ns":10,"deadline_ns":100,"action":"clock_skew","handle":"clock","machine":"a","offset_ns":1}),
                ],
                Action::Heal {
                    handle: "clock".into(),
                },
                "clock skew is not settled",
            ),
        ] {
            let mut faults = Faults::new(initial, vec!["a".into(), "b".into()]).unwrap();
            faults.apply_due(10);
            let plan = Plan {
                id: 0,
                at_ns: 20,
                deadline_ns: 30,
                action,
            };
            // A caller or retained record cannot bypass proposal validation.
            let intervention = Intervention {
                request_id: "invalid".into(),
                position: boundary(2).position,
                plan: faults.propose(plan.clone(), 20).unwrap(),
            };
            let before = serde_json::to_vec(&faults).unwrap();
            let mut admission = Admission::new(&mut faults, boundary(2), &|_| Ok(()));
            assert_eq!(
                admission.propose("invalid".into(), plan).unwrap_err(),
                error
            );
            assert_eq!(admission.commit(&intervention).unwrap_err(), error);
            assert!(!admission.changed());
            assert_eq!(serde_json::to_vec(&faults).unwrap(), before);
            assert!(faults.error.is_none());
        }
    }

    #[test]
    fn commit_rechecks_lifecycle_and_handle_state_after_a_proposal() {
        for heal in [false, true] {
            let initial = if heal {
                vec![
                    serde_json::json!({"id":1,"at_ns":10,"deadline_ns":10,"action":"partition","handle":"network","left":["a"],"right":["b"]}),
                    serde_json::json!({"id":2,"at_ns":20,"deadline_ns":20,"action":"heal","handle":"network"}),
                ]
            } else {
                vec![
                    serde_json::json!({"id":1,"at_ns":20,"deadline_ns":20,"action":"pause","machine":"a"}),
                ]
            };
            let mut faults = Faults::new(initial, vec!["a".into(), "b".into()]).unwrap();
            faults.apply_due(10);
            let action = if heal {
                Action::Heal {
                    handle: "network".into(),
                }
            } else {
                Action::Stop {
                    machine: "a".into(),
                }
            };
            let proposal = Admission::new(&mut faults, boundary(2), &|_| Ok(()))
                .propose(
                    "race".into(),
                    Plan {
                        id: 0,
                        at_ns: 20,
                        deadline_ns: 30,
                        action,
                    },
                )
                .unwrap();
            // Existing effects change state without consuming another admission ID.
            faults.apply_due(20);
            assert_eq!(
                faults.propose(proposal.plan.clone(), 20).unwrap().id,
                proposal.plan.id
            );
            let before = serde_json::to_vec(&faults).unwrap();
            let mut admission = Admission::new(&mut faults, boundary(2), &|_| Ok(()));
            assert_eq!(
                admission.commit(&proposal).unwrap_err(),
                if heal {
                    "heal of inactive handle"
                } else {
                    "target machine is paused"
                }
            );
            assert!(!admission.changed());
            assert_eq!(serde_json::to_vec(&faults).unwrap(), before);
        }
    }

    #[test]
    fn replay_rejects_unreached_or_divergent_inputs_without_silently_skipping_them() {
        for mutation in 0..4 {
            let mut replay = Replay::new(history()).unwrap();
            let mut faults = Faults::new(vec![], vec!["a".into(), "b".into()]).unwrap();
            let mut admission = Admission::new(&mut faults, boundary(1), &|_| Ok(()));
            replay.boundary(&mut admission).unwrap();
            assert!(replay.finish().unwrap_err().contains("ended before"));
            let saved = replay.checkpoint(boundary(1).position).unwrap();
            let bytes = serde_json::to_vec(&saved).unwrap();
            replay = Replay::from_checkpoint(serde_json::from_slice(&bytes).unwrap()).unwrap();
            assert!(replay.finish().unwrap_err().contains("ended before"));
            let mut position = boundary(2);
            match mutation {
                0 => position = boundary(3),
                1 => position.position.completed_rounds += 1,
                2 => position.position.simulated_time_ns += 1,
                _ => position.terminal = true,
            }
            let mut admission = Admission::new(&mut faults, position, &|_| Ok(()));
            assert!(replay.boundary(&mut admission).is_err());
            assert!(!admission.changed());
            // Even a later matching callback cannot revive a failed replay.
            let mut admission = Admission::new(&mut faults, boundary(2), &|_| Ok(()));
            assert!(replay
                .boundary(&mut admission)
                .unwrap_err()
                .contains("previously failed"));
            assert!(replay.finish().is_err());
        }
    }

    #[test]
    fn multiple_inputs_share_a_boundary_and_partial_failure_is_terminal() {
        let mut retained = history();
        retained.entries.push(Intervention {
            request_id: "second-cut".into(),
            position: boundary(2).position,
            plan: Plan {
                id: 2,
                at_ns: 20,
                deadline_ns: 20,
                action: Action::Partition {
                    handle: "second-network".into(),
                    left: vec!["a".into()],
                    right: vec!["b".into()],
                },
            },
        });
        for bad_id in [false, true] {
            let mut source = retained.clone();
            if bad_id {
                source.entries[1].plan.id = 3;
            }
            let mut replay = Replay::new(source).unwrap();
            let mut faults = Faults::new(vec![], vec!["a".into(), "b".into()]).unwrap();
            let mut admission = Admission::new(&mut faults, boundary(2), &|_| Ok(()));
            let result = replay.boundary(&mut admission);
            assert!(admission.changed());
            if bad_id {
                assert!(result.is_err());
                assert!(replay.finish().is_err());
                assert!(replay.boundary(&mut admission).is_err());
                assert!(replay.checkpoint(boundary(2).position).is_err());
            } else {
                result.unwrap();
                replay.finish().unwrap();
                // A durable cursor resumes after both same-boundary inputs;
                // re-observation cannot append the same plans twice.
                let saved = replay.checkpoint(boundary(2).position).unwrap();
                let bytes = serde_json::to_vec(&saved).unwrap();
                replay = Replay::from_checkpoint(serde_json::from_slice(&bytes).unwrap()).unwrap();
                replay.boundary(&mut admission).unwrap();
                for consumed in [0, 1, 3] {
                    let mut corrupt = saved.clone();
                    corrupt.consumed = consumed;
                    assert!(Replay::from_checkpoint(corrupt).is_err());
                }
                let mut corrupt = saved;
                corrupt.position.simulated_time_ns += 1;
                assert!(Replay::from_checkpoint(corrupt).is_err());
            }
        }
    }
}
