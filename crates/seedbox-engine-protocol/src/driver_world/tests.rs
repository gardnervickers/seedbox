use super::*;
use crate::control::{Action, Plan};
fn input(key: &str, at_ns: u64) -> TimedInput {
    TimedInput {
        key: key.into(),
        at_ns,
        action: Action::Crash {
            machine: "a".into(),
        },
        timeout_ns: 10,
    }
}
fn admitted(input: &TimedInput, id: u32) -> InputAdmission {
    InputAdmission {
        key: input.key.clone(),
        plan: Plan {
            id,
            at_ns: input.at_ns,
            deadline_ns: input.at_ns + input.timeout_ns,
            action: input.action.clone(),
        },
    }
}
#[test]
fn consumed_prefix_is_exact_and_pending_suffix_is_replaceable_without_reseeding() {
    let first = input("first", 0);
    let next = input("next", 2);
    let mut program = Program {
        inputs: vec![first.clone(), next.clone()],
        next: 1,
        admissions: vec![admitted(&first, 9)],
        rejections: vec![],
    };
    program.validate().unwrap();
    let original = program.clone();
    let mut changed = first.clone();
    changed.timeout_ns += 1;
    assert!(program.replace_pending(vec![changed, next]).is_err());
    assert_eq!(program, original);
    let replacement = input("edited", 3);
    program
        .replace_pending(vec![first.clone(), replacement.clone()])
        .unwrap();
    assert_eq!(program.inputs, vec![first, replacement]);
    assert_eq!(program.next, 1);
    assert_eq!(program.admissions, original.admissions);
}
#[test]
fn rejects_cursor_history_order_identity_deadline_and_unconsumed_rows() {
    let first = input("first", 0);
    let second = input("second", 1);
    let valid = Program {
        inputs: vec![first.clone(), second.clone()],
        next: 2,
        admissions: vec![admitted(&first, 3), admitted(&second, 5)],
        rejections: vec![],
    };
    valid.validate().unwrap();
    for n in 0..5 {
        let mut invalid = valid.clone();
        match n {
            0 => invalid.next = 3,
            1 => invalid.admissions[1].plan.id = 2,
            2 => invalid.admissions[1].key = "first".into(),
            3 => invalid.admissions[0].plan.deadline_ns += 1,
            _ => {
                invalid.admissions.remove(0);
                invalid.rejections.push(InputRejection {
                    key: "second".into(),
                    at_ns: 1,
                    reason: "rejected".into(),
                });
            }
        };
        assert!(invalid.validate().is_err(), "case {n}");
    }
}
#[test]
fn state_rejection_consumes_only_its_authored_entry() {
    let first = input("first", 0);
    let second = input("second", 0);
    let program = Program {
        inputs: vec![first, second.clone()],
        next: 2,
        admissions: vec![admitted(&second, 1)],
        rejections: vec![InputRejection {
            key: "first".into(),
            at_ns: 0,
            reason: "not running".into(),
        }],
    };
    program.validate().unwrap();
}
#[test]
fn initial_ready_seen_set_and_ever_latch_survive_serialization_and_repeated_boots() {
    let mut ready = Readiness {
        initial: vec!["a".into(), "oracle".into()],
        seen: vec![],
        latched: false,
    };
    ready.observe("a");
    ready.observe("spawned");
    ready.observe("a");
    ready.validate().unwrap();
    assert!(!ready.latched);
    let mut restored: Readiness =
        serde_json::from_slice(&serde_json::to_vec(&ready).unwrap()).unwrap();
    restored.observe("oracle");
    restored.validate().unwrap();
    assert!(restored.latched);
    // A crash/reboot has no clearing operation: readiness is an ever latch.
    restored.observe("a");
    assert!(restored.latched);
    assert_eq!(restored.seen.len(), 2);
}
#[test]
fn readiness_decode_rejects_invented_latches_duplicate_and_foreign_members() {
    for ready in [
        Readiness {
            initial: vec!["a".into()],
            seen: vec![],
            latched: true,
        },
        Readiness {
            initial: vec!["a".into()],
            seen: vec!["b".into()],
            latched: true,
        },
        Readiness {
            initial: vec!["a".into(), "a".into()],
            seen: vec![],
            latched: false,
        },
    ] {
        assert!(ready.validate().is_err());
    }
}

#[test]
fn authored_cursor_checks_saved_time_before_restore() {
    let consumed = input("consumed", 10);
    let pending = input("pending", 20);
    let program = Program {
        inputs: vec![consumed.clone(), pending],
        next: 1,
        admissions: vec![admitted(&consumed, 1)],
        rejections: vec![],
    };
    program.validate_at(10).unwrap();
    program.validate_at(20).unwrap();
    assert!(program.validate_at(9).unwrap_err().contains("consumed"));
    assert!(program.validate_at(21).unwrap_err().contains("pending"));
}

#[test]
fn readiness_keeps_nonempty_names_without_a_new_semantic_length_pin() {
    let name = "n".repeat(65_537);
    let mut readiness = Readiness {
        initial: vec![name.clone()],
        seen: vec![],
        latched: false,
    };
    readiness.validate().unwrap();
    readiness.observe(&name);
    readiness.validate().unwrap();
    let restored: Readiness =
        serde_json::from_slice(&serde_json::to_vec(&readiness).unwrap()).unwrap();
    assert_eq!(restored, readiness);
}

#[test]
fn observer_history_is_not_serializable_but_current_outcome_continues() {
    let mut source = Publication {
        oracle_exit: Some(17),
        ..Default::default()
    };
    source.session.next_driver_sequence = 91;
    source.session.observation_records = 72;
    source.session.applied_faults.push(AppliedFault {
        requested_at_ns: 1,
        applied_at_ns: 2,
        service: "a".into(),
        action: "crash".into(),
        detail: "confirmed".into(),
    });
    let bytes = serde_json::to_vec(&source).unwrap();
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&bytes).unwrap(),
        serde_json::json!({"oracle_exit":17})
    );
    let restored: Publication = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(restored.oracle_exit, source.oracle_exit);
    assert_eq!(restored.session, PublicationSession::default());
    assert_eq!(
        source.session.next_driver_sequence, 91,
        "encoding does not drain source history"
    );
}
#[test]
fn older_envelope_version_has_actionable_refusal_before_body_decode() {
    let text = r#"{"contract":"driver-world-v1","version":2}"#;
    let error = serde_json::from_str::<Checkpoint>(text)
        .unwrap_err()
        .to_string();
    assert!(error.contains("checkpoint was made by an older or incompatible engine; replay with its retained engine or run fresh"), "{error}");
    assert!(
        !error.contains("missing field"),
        "old version is diagnosed before body decode"
    );
}
