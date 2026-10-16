//! Portable checkpoint input metadata. Captured consumption and admission IDs
//! are authoritative; only a validated pending override suffix is projected.
use crate::{Error, Result};
use seedbox_engine_protocol::{
    input_program::{validate_program, InputAdmission, InputRejection, TimedInput},
    intervention::History,
};
use seedbox_spec::definition::{Override, Start};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CapturedProgram {
    inputs: Vec<TimedInput>,
    next: usize,
    admissions: Vec<InputAdmission>,
    failed: bool,
    rejections: Vec<InputRejection>,
    /// Generated starts skipped so far; carried through projection unchanged.
    #[serde(default, skip_serializing_if = "std::collections::BTreeSet::is_empty")]
    skipped: std::collections::BTreeSet<String>,
    /// Consumed anchors and reseeds, which allocate no fault IDs.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    branches: Vec<seedbox_engine_protocol::input_program::BranchReceipt>,
}
impl CapturedProgram {
    fn validate(&self) -> Result<()> {
        validate_program(&self.inputs, u64::MAX).map_err(Error::Integrity)?;
        if self.failed
            || self.next > self.inputs.len()
            || self.next != self.admissions.len() + self.rejections.len() + self.branches.len()
        {
            return Err(Error::Integrity(
                "checkpoint has an invalid consumed input cursor".into(),
            ));
        }
        let (mut admitted, mut rejected, mut branched, mut previous) = (
            self.admissions.iter().peekable(),
            self.rejections.iter().peekable(),
            self.branches.iter(),
            0,
        );
        for input in &self.inputs[..self.next] {
            if let seedbox_engine_protocol::input_program::InputAction::Branch(action) =
                &input.action
            {
                let receipt = branched.next().ok_or_else(|| {
                    Error::Integrity("checkpoint consumed branch input lacks its receipt".into())
                })?;
                if receipt.key != input.key
                    || receipt.at_ns != input.at_ns
                    || &receipt.action != action
                {
                    return Err(Error::Integrity(
                        "checkpoint branch receipt disagrees with consumed input".into(),
                    ));
                }
                continue;
            }
            if rejected.peek().is_some_and(|r| r.key == input.key) {
                let row = rejected.next().expect("peeked");
                if row.at_ns != input.at_ns || row.reason.is_empty() {
                    return Err(Error::Integrity(
                        "checkpoint input rejection disagrees with consumption".into(),
                    ));
                }
            } else {
                let row = admitted.next().ok_or_else(|| {
                    Error::Integrity("checkpoint consumed input lacks admission evidence".into())
                })?;
                if row.key != input.key
                    || row.plan.at_ns != input.at_ns
                    || input.action.fault() != Some(&row.plan.action)
                    || row.plan.id <= previous
                    || Some(row.plan.deadline_ns) != input.at_ns.checked_add(input.timeout_ns)
                {
                    return Err(Error::Integrity(
                        "checkpoint admission disagrees with consumed input identity".into(),
                    ));
                }
                previous = row.plan.id;
            }
        }
        Ok(())
    }
}
fn program(start: &Start) -> Result<Option<CapturedProgram>> {
    let Start::Checkpoint { manifest, .. } = start else {
        return Ok(None);
    };
    let value = manifest
        .pointer("/state/world/input_program")
        .ok_or_else(|| Error::Integrity("checkpoint is missing its input program".into()))?;
    let program: CapturedProgram = serde_json::from_value(value.clone())?;
    program.validate()?;
    Ok(Some(program))
}
/// Validate retained consumption, including rejected inputs, without a backend.
pub fn validate(start: &Start) -> Result<()> {
    program(start).map(|_| ())
}

/// The frozen journal is captured with the checkpoint, not the source's current
/// journal. Every frozen host plan must already be consumed with exact admission
/// evidence. Never fabricate missing admissions or infer consumption from time.
pub fn project(start: &mut Start, overrides: &[Override], frozen: &History) -> Result<()> {
    frozen.validate().map_err(Error::Integrity)?;
    let Some(mut captured) = program(start)? else {
        return Ok(());
    };
    let mut seen = BTreeSet::new();
    for entry in &frozen.entries {
        if !seen.insert(&entry.request_id) {
            return Err(Error::Integrity("duplicate frozen checkpoint input".into()));
        }
        let input = captured.inputs[..captured.next]
            .iter()
            .find(|input| input.key == entry.request_id)
            .ok_or_else(|| {
                Error::Integrity(format!(
                    "frozen checkpoint input {} is not consumed",
                    entry.request_id
                ))
            })?;
        let admission = captured
            .admissions
            .iter()
            .find(|row| row.key == entry.request_id)
            .ok_or_else(|| {
                Error::Integrity("frozen checkpoint input lacks admission evidence".into())
            })?;
        if input.action.fault() != Some(&entry.plan.action)
            || input.at_ns != entry.plan.at_ns
            || input.at_ns.checked_add(input.timeout_ns) != Some(entry.plan.deadline_ns)
            || admission.plan != entry.plan
        {
            return Err(Error::Integrity(format!(
                "frozen checkpoint input {} conflicts with captured admission",
                entry.request_id
            )));
        }
    }
    let canonical: Vec<TimedInput> = overrides
        .iter()
        .map(|input| {
            Ok(TimedInput {
                key: input.id.clone(),
                at_ns: input.at_ns,
                action: serde_json::from_value(input.action.clone())?,
                timeout_ns: input.timeout_ns,
            })
        })
        .collect::<Result<_>>()?;
    validate_program(&canonical, u64::MAX).map_err(Error::Integrity)?;
    if canonical.get(..captured.next) != Some(&captured.inputs[..captured.next]) {
        return Err(Error::Integrity("canonical override order changes the captured consumed prefix; resubmit from a fresh start before capturing a checkpoint".into()));
    }
    captured.inputs = canonical;
    captured.validate()?;
    let Start::Checkpoint { manifest, .. } = start else {
        unreachable!()
    };
    // Keep cursor/admission/rejection/failed fields byte-for-byte as values.
    manifest["state"]["world"]["input_program"]["inputs"] = serde_json::to_value(captured.inputs)?;
    start
        .validate_overrides(overrides)
        .map_err(Error::Integrity)
}

#[cfg(test)]
mod tests {
    use super::*;
    use seedbox_control::{Action, Plan};
    use seedbox_engine_core::{faults::Faults, input_program::Program};
    use seedbox_engine_protocol::{host_control::BoundaryPosition, intervention::Intervention};
    fn input(key: &str, at_ns: u64) -> TimedInput {
        TimedInput {
            key: key.into(),
            at_ns,
            action: Action::PauseProcess {
                machine: "oracle".into(),
            }
            .into(),
            timeout_ns: 100,
        }
    }
    fn plan(id: u32, input: &TimedInput) -> Plan {
        Plan {
            id,
            at_ns: input.at_ns,
            deadline_ns: input.at_ns + input.timeout_ns,
            action: input.action.fault().expect("a fault input").clone(),
        }
    }
    fn fixture() -> (Start, Vec<Override>, History, Faults) {
        let host = input("earlier-host", 10);
        let authored = input("authored", 20);
        let rejected = input("rejected", 25);
        let future = input("admitted-future-host", 30);
        let later = input("later-host-at-same-boundary", 30);
        let pending = input("pending-authored", 40);
        let mut faults = Faults::new(Vec::new(), vec!["oracle".into()]).unwrap();
        let mut program = Program::new(vec![authored, rejected, pending], u64::MAX, 0).unwrap();
        program
            .commit_live(&mut faults, host.key.clone(), plan(1, &host), 10)
            .unwrap();
        program
            .admit_due(
                &mut faults,
                20,
                seedbox_engine_core::input_program::Gate {
                    ready: true,
                    quiet_until: 0,
                },
                &|_| Ok(()),
            )
            .unwrap();
        program
            .admit_due(
                &mut faults,
                25,
                seedbox_engine_core::input_program::Gate {
                    ready: true,
                    quiet_until: 0,
                },
                &|_| Err("portable rejected input".into()),
            )
            .unwrap();
        program
            .commit_live(&mut faults, future.key.clone(), plan(3, &future), 25)
            .unwrap();
        program.validate().unwrap();
        let frozen = History {
            version: 1,
            entries: vec![
                Intervention {
                    request_id: host.key.clone(),
                    position: BoundaryPosition {
                        boundary_index: 1,
                        completed_rounds: 1,
                        simulated_time_ns: 10,
                    },
                    plan: plan(1, &host),
                },
                Intervention {
                    request_id: future.key.clone(),
                    position: BoundaryPosition {
                        boundary_index: 2,
                        completed_rounds: 2,
                        simulated_time_ns: 25,
                    },
                    plan: plan(3, &future),
                },
            ],
        };
        let mut inputs: Vec<TimedInput> =
            serde_json::from_value(serde_json::to_value(&program).unwrap()["inputs"].clone())
                .unwrap();
        inputs.insert(4, later);
        let overrides = inputs
            .iter()
            .map(|input| Override {
                id: input.key.clone(),
                at_ns: input.at_ns,
                action: serde_json::to_value(&input.action).unwrap(),
                timeout_ns: input.timeout_ns,
            })
            .collect();
        let start = Start::Checkpoint {
            manifest: serde_json::json!({"state":{"world":{"input_program":program,"faults":faults}}}),
            assets: Default::default(),
        };
        (start, overrides, frozen, faults)
    }
    #[test]
    fn project_preserves_current_consumption_and_continues_only_pending_inputs() {
        let (mut start, overrides, frozen, mut faults) = fixture();
        let original = start.clone();
        project(&mut start, &overrides, &frozen).unwrap();
        let (
            Start::Checkpoint {
                manifest: before, ..
            },
            Start::Checkpoint {
                manifest: after, ..
            },
        ) = (&original, &start)
        else {
            unreachable!()
        };
        assert_eq!(
            before["state"]["world"]["faults"],
            after["state"]["world"]["faults"]
        );
        for field in ["next", "admissions", "rejections", "failed"] {
            assert_eq!(
                before["state"]["world"]["input_program"][field],
                after["state"]["world"]["input_program"][field]
            );
        }
        let mut restored: Program =
            serde_json::from_value(after["state"]["world"]["input_program"].clone()).unwrap();
        restored.validate().unwrap();
        let canonical: Vec<TimedInput> =
            serde_json::from_value(after["state"]["world"]["input_program"]["inputs"].clone())
                .unwrap();
        restored.replace_pending(canonical).unwrap();
        assert_eq!(
            restored.next_time(),
            30,
            "later host at the same capture boundary was not consumed"
        );
        assert_eq!(restored.admissions().len(), 3);
        assert_eq!(restored.rejections().len(), 1);
        restored
            .admit_due(
                &mut faults,
                30,
                seedbox_engine_core::input_program::Gate {
                    ready: true,
                    quiet_until: 0,
                },
                &|_| Ok(()),
            )
            .unwrap();
        assert_eq!(
            restored.admissions().last().unwrap().key,
            "later-host-at-same-boundary"
        );
        assert_eq!(restored.admissions().last().unwrap().plan.id, 4);
        assert_eq!(restored.next_time(), 40);
        restored
            .admit_due(
                &mut faults,
                40,
                seedbox_engine_core::input_program::Gate {
                    ready: true,
                    quiet_until: 0,
                },
                &|_| Ok(()),
            )
            .unwrap();
        assert_eq!(
            restored.admissions().last().unwrap().key,
            "pending-authored"
        );
        assert_eq!(restored.admissions().last().unwrap().plan.id, 5);
        assert_eq!(restored.next_time(), u64::MAX);
    }
    #[test]
    fn project_rejects_unknown_conflicting_duplicate_and_reordered_consumption() {
        let (start, overrides, frozen, _) = fixture();
        for change in 0..5 {
            let mut history = frozen.clone();
            match change {
                0 => history.entries[0].request_id = "unknown".into(),
                1 => history.entries[0].plan.deadline_ns += 1,
                2 => {
                    history.entries[0].plan.action = Action::ResumeProcess {
                        machine: "oracle".into(),
                    }
                }
                3 => history.entries[0].plan.id += 1,
                4 => history.entries.push(history.entries[1].clone()),
                _ => unreachable!(),
            }
            assert!(
                project(&mut start.clone(), &overrides, &history).is_err(),
                "change {change}"
            );
        }
        let mut changed = overrides.clone();
        changed[0].timeout_ns += 1;
        assert!(project(&mut start.clone(), &changed, &frozen).is_err());
        let mut changed = overrides.clone();
        changed.insert(0, input_override("not-consumed", 5));
        assert!(project(&mut start.clone(), &changed, &frozen)
            .unwrap_err()
            .to_string()
            .contains("consumed prefix"));
        let mut changed = overrides.clone();
        changed.push(changed[0].clone());
        assert!(project(&mut start.clone(), &changed, &frozen).is_err());
        let mut invalid = start.clone();
        let Start::Checkpoint { manifest, .. } = &mut invalid else {
            unreachable!()
        };
        manifest["state"]["world"]["input_program"]["next"] = 3.into();
        assert!(validate(&invalid).is_err());
    }
    fn input_override(key: &str, at_ns: u64) -> Override {
        let input = input(key, at_ns);
        Override {
            id: input.key,
            at_ns,
            action: serde_json::to_value(input.action).unwrap(),
            timeout_ns: input.timeout_ns,
        }
    }
}

#[cfg(test)]
mod same_time_tests {
    use super::*;
    use seedbox_control::{Action, Plan};
    use seedbox_engine_core::{faults::Faults, input_program::Program};
    use seedbox_engine_protocol::{host_control::BoundaryPosition, intervention::Intervention};
    #[test]
    fn same_time_live_admission_follows_consumed_authored_order_without_rewriting_ids() {
        let action = Action::PauseProcess {
            machine: "oracle".into(),
        };
        let authored = TimedInput {
            key: "authored".into(),
            at_ns: 10,
            action: action.clone().into(),
            timeout_ns: 100,
        };
        let host = TimedInput {
            key: "host".into(),
            at_ns: 10,
            action: action.clone().into(),
            timeout_ns: 100,
        };
        let pending = TimedInput {
            key: "pending".into(),
            at_ns: 20,
            action: action.clone().into(),
            timeout_ns: 100,
        };
        let mut program =
            Program::new(vec![authored.clone(), pending.clone()], u64::MAX, 0).unwrap();
        let mut faults = Faults::new(Vec::new(), vec!["oracle".into()]).unwrap();
        program
            .admit_due(
                &mut faults,
                10,
                seedbox_engine_core::input_program::Gate {
                    ready: true,
                    quiet_until: 0,
                },
                &|_| Ok(()),
            )
            .unwrap();
        let plan = Plan {
            id: 2,
            at_ns: 10,
            deadline_ns: 110,
            action,
        };
        program
            .commit_live(&mut faults, "host".into(), plan.clone(), 10)
            .unwrap();
        let frozen = History {
            version: 1,
            entries: vec![Intervention {
                request_id: "host".into(),
                position: BoundaryPosition {
                    boundary_index: 1,
                    completed_rounds: 0,
                    simulated_time_ns: 10,
                },
                plan,
            }],
        };
        let mut start = Start::Checkpoint {
            manifest: serde_json::json!({"state":{"world":{"input_program":program}}}),
            assets: Default::default(),
        };
        let overrides: Vec<Override> = [authored, host, pending]
            .into_iter()
            .map(|input| Override {
                id: input.key,
                at_ns: input.at_ns,
                action: serde_json::to_value(input.action).unwrap(),
                timeout_ns: input.timeout_ns,
            })
            .collect();
        project(&mut start, &overrides, &frozen).unwrap();
        let Start::Checkpoint { manifest, .. } = &start else {
            unreachable!()
        };
        let restored: Program =
            serde_json::from_value(manifest["state"]["world"]["input_program"].clone()).unwrap();
        restored.validate().unwrap();
        assert_eq!(restored.next_time(), 20);
        assert_eq!(
            restored
                .admissions()
                .iter()
                .map(|row| row.plan.id)
                .collect::<Vec<_>>(),
            [1, 2]
        );
        let mut reordered = overrides;
        reordered.swap(0, 1);
        assert!(project(&mut start, &reordered, &frozen)
            .unwrap_err()
            .to_string()
            .contains("consumed prefix"));
    }
}
