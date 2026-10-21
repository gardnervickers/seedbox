#![cfg(unix)]

use seedbox_api::Phase;
use seedbox_server::catalogue::{Catalogue, Error};
use seedbox_server::test_support;
use test_support::request;

#[test]
fn cpu_backend_is_request_owned_and_retained_inputs_reject_conflicts() {
    use seedbox_api::InputSource;
    use seedbox_spec::backend::CpuBackend;
    let mut catalogue = Catalogue::in_memory();
    let mut fresh = request();
    fresh.options.backend = Some(CpuBackend::Tcg);
    let (mut source, _) = catalogue.submit(fresh.clone()).unwrap();
    assert_eq!(source.options.backend, Some(CpuBackend::Tcg));
    assert!(serde_json::to_value(&source.spec).unwrap()["simulator"]
        .get("backend")
        .is_none());
    assert_eq!(catalogue.submit(fresh).unwrap().0, source);
    source.status.phase = Phase::Completed;
    let source = catalogue.update(source).unwrap();
    let mut retained = request();
    retained.request_id = "retained-backend".into();
    retained.spec = source.spec.clone();
    retained.spec.inputs = InputSource::Retained {
        simulation: source.metadata.id,
    };
    retained.options.backend = Some(CpuBackend::Pv);
    assert!(
        matches!(catalogue.submit(retained.clone()), Err(Error::Invalid(message)) if message.contains("retained backend"))
    );
    retained.options.backend = None;
    assert_eq!(
        catalogue.submit(retained).unwrap().0.options.backend,
        Some(CpuBackend::Tcg)
    );
    for (tag, cpu, other) in [
        ("driver-pv", CpuBackend::Pv, CpuBackend::Tcg),
        ("driver-tcg", CpuBackend::Tcg, CpuBackend::Pv),
    ] {
        let mut resolved = test_support::resolved_request(tag);
        let InputSource::Resolved { definition } = &mut resolved.spec.inputs else {
            panic!("resolved fixture")
        };
        definition.runtime.execution["backend"] = tag.into();
        definition.runtime.execution["semantic_parameters"]["engine"] =
            serde_json::json!({"kind":"driver-world-v1", "engine_backend":tag});
        resolved.options.backend = Some(other);
        assert!(
            matches!(catalogue.submit(resolved.clone()), Err(Error::Invalid(message)) if message.contains("retained backend"))
        );
        resolved.options.backend = None;
        assert_eq!(
            catalogue.submit(resolved).unwrap().0.options.backend,
            Some(cpu)
        );
    }
}

#[test]
fn unknown_input_sources_return_not_found_without_creating_resources() {
    use seedbox_api::{InputSource, SeedRangeRequest, SimulationId};
    let missing: SimulationId = "k7m2p9qx".parse().unwrap();
    for inputs in [
        InputSource::Retained {
            simulation: missing.clone(),
        },
        InputSource::Prepared {
            simulation: missing.clone(),
        },
        InputSource::Checkpoint {
            simulation: missing.clone(),
            checkpoint: "capture".into(),
        },
    ] {
        let mut catalogue = Catalogue::in_memory();
        let mut submission = request();
        submission.spec.inputs = inputs.clone();
        assert!(
            matches!(catalogue.submit(submission.clone()), Err(Error::NotFound(id)) if id == missing)
        );
        assert_eq!(catalogue.list().count(), 0);
        let range = SeedRangeRequest {
            metadata: submission.metadata,
            options: submission.options,
            request_id: submission.request_id,
            spec: submission.spec,
            count: 2,
        };
        if matches!(inputs, InputSource::Checkpoint { .. }) {
            // Seed exploration rejects checkpoints before resolving any source.
            assert!(
                matches!(catalogue.submit_seeds(range), Err(Error::Invalid(message)) if message.contains("seed exploration requires"))
            );
        } else {
            assert!(
                matches!(catalogue.submit_seeds(range), Err(Error::NotFound(id)) if id == missing)
            );
        }
        assert_eq!(catalogue.list().count(), 0);
    }
}

#[test]
fn project_submissions_are_held_to_what_a_project_can_express() {
    let mut catalogue = Catalogue::in_memory();
    let mut relative = request();
    relative
        .spec
        .configuration
        .services
        .get_mut("oracle")
        .unwrap()
        .source = seedbox_spec::ImageSource::Build {
        context: "context".into(),
        dockerfile: "context/Dockerfile".into(),
    };
    let mut tolerance = request();
    tolerance.spec.configuration.schedule.stop_tolerance_ns = 1;
    let mut nul = request();
    nul.spec
        .configuration
        .services
        .get_mut("oracle")
        .unwrap()
        .command = Some(vec!["run\0".into()]);
    for (invalid, path) in [
        (relative, "$.services.oracle.source"),
        (tolerance, "$.schedule"),
        (nul, "$.services.oracle"),
    ] {
        assert!(
            matches!(catalogue.submit(invalid), Err(Error::Invalid(message)) if message.contains(path)),
            "{path}"
        );
    }
    assert_eq!(catalogue.list().count(), 0);
}

#[test]
fn accepted_submission_and_request_identity_are_idempotent() {
    let mut catalogue = Catalogue::in_memory();
    let mut invalid = request();
    invalid.spec.configuration.check.exit_code = 7;
    assert!(matches!(catalogue.submit(invalid), Err(Error::Invalid(_))));
    let (resource, created) = catalogue.submit(request()).unwrap();
    assert!(created);
    assert_eq!(resource.spec.seed, Some(123));
    let mut running = resource.clone();
    running.status.phase = Phase::Preparing;
    let running = catalogue.update(running).unwrap();
    assert!(matches!(
        catalogue.update(resource),
        Err(Error::RevisionConflict)
    ));
    let (retried, created) = catalogue.submit(request()).unwrap();
    assert!(!created);
    assert_eq!(retried, running);
    assert_eq!(catalogue.list().count(), 1);
    let mut different = request();
    different.spec.seed = Some(124);
    assert!(matches!(
        catalogue.submit(different),
        Err(Error::RequestConflict)
    ));
}

#[test]
fn random_seed_is_allocated_once_and_collision_checked_ids_are_distinct() {
    let mut catalogue = Catalogue::in_memory();
    let mut submit = request();
    submit.spec.seed = None;
    let (first, _) = catalogue.submit(submit.clone()).unwrap();
    assert!(first.spec.seed.is_some());
    assert_eq!(catalogue.submit(submit.clone()).unwrap().0, first);
    submit.request_id = "another-submission".into();
    let (second, _) = catalogue.submit(submit).unwrap();
    assert_ne!(first.metadata.id, second.metadata.id);
}

#[test]
fn control_receipts_share_ordering_and_never_reapply_after_completion() {
    use seedbox_api::{
        BoundaryPosition, ControlCommand, ControlRequest, ControlState, PlaybackRate,
        PlaybackStatus,
    };
    let mut catalogue = Catalogue::in_memory();
    let (mut resource, _) = catalogue.submit(request()).unwrap();
    let id = resource.metadata.id.clone();
    resource.status.phase = Phase::Preparing;
    resource = catalogue.update(resource).unwrap();
    resource.status.phase = Phase::Running;
    catalogue.update(resource).unwrap();
    let request = ControlRequest {
        request_id: "step-1".into(),
        command: ControlCommand::Step,
    };
    let (accepted, created) = catalogue.accept_control(&id, request.clone()).unwrap();
    assert!(created);
    let mut other = request.clone();
    other.request_id = "fault-2".into();
    other.command = ControlCommand::InjectFault {
        action: seedbox_api::FaultAction::Heal {
            handle: "cut".into(),
        },
    };
    assert!(matches!(
        catalogue.accept_control(&id, other.clone()),
        Err(Error::ControlBusy)
    ));
    let mut conflicting = request.clone();
    conflicting.command = ControlCommand::SetRate {
        rate: PlaybackRate::Unlimited,
    };
    assert!(matches!(
        catalogue.accept_control(&id, conflicting),
        Err(Error::RequestConflict)
    ));
    assert_eq!(
        catalogue.accept_control(&id, request.clone()).unwrap(),
        (accepted, false)
    );
    let state = ControlState::Acknowledged {
        checkpoint: None,
        intervention: None,
        detail: None,
        status: PlaybackStatus {
            achieved_rate: None,
            position: BoundaryPosition {
                boundary_index: 4,
                completed_rounds: 4,
                simulated_time_ns: 42,
            },
            rate: PlaybackRate::Paused,
            stepping: true,
            before_next_fault: false,
            terminal: false,
            application_ready: false,
        },
        error: None,
    };
    let ack = catalogue
        .acknowledge_control(&id, &request.request_id, state.clone())
        .unwrap();
    catalogue.accept_control(&id, other.clone()).unwrap();
    let mut completed = catalogue.get(&id).unwrap().clone();
    completed.status.phase = Phase::Completed;
    completed.status.termination = Some(seedbox_api::ExecutionTermination::Interrupted);
    catalogue.update(completed).unwrap();
    assert_eq!(
        catalogue
            .control_receipt(&id, &request.request_id)
            .unwrap()
            .unwrap(),
        &ack
    );
    assert_eq!(
        catalogue
            .control_receipt(&id, &other.request_id)
            .unwrap()
            .unwrap()
            .state,
        ControlState::Interrupted
    );
    assert!(catalogue.pending_controls().is_empty());
    assert_eq!(
        catalogue.accept_control(&id, request).unwrap(),
        (ack, false)
    );
    assert_eq!(
        catalogue
            .acknowledge_control(&id, &other.request_id, state)
            .unwrap()
            .state,
        ControlState::Interrupted
    );
}

#[test]
fn captured_checkpoint_registry_is_independent_of_descendant_ownership() {
    use seedbox_api::{
        BoundaryPosition, ControlCommand, ControlRequest, ControlState, PlaybackRate,
        PlaybackStatus,
    };
    use seedbox_engine_protocol::host_control::{CheckpointState, CheckpointStatus};
    let mut catalogue = Catalogue::in_memory();
    let (mut source, _) = catalogue
        .submit(test_support::resolved_request("source"))
        .unwrap();
    source.status.phase = Phase::Preparing;
    source = catalogue.update(source).unwrap();
    source.status.phase = Phase::Running;
    source = catalogue.update(source).unwrap();
    let id = source.metadata.id.clone();
    assert!(catalogue.captured_checkpoint(&id, "saved").is_err());
    catalogue
        .accept_control(
            &id,
            ControlRequest {
                request_id: "saved".into(),
                command: ControlCommand::Checkpoint,
            },
        )
        .unwrap();
    assert!(catalogue.captured_checkpoint(&id, "saved").is_err());
    let position = BoundaryPosition {
        boundary_index: 5,
        completed_rounds: 4,
        simulated_time_ns: 100,
    };
    catalogue
        .acknowledge_control(
            &id,
            "saved",
            ControlState::Acknowledged {
                status: PlaybackStatus {
                    achieved_rate: None,
                    position,
                    rate: PlaybackRate::Paused,
                    stepping: false,
                    before_next_fault: false,
                    terminal: false,
                    application_ready: true,
                },
                error: None,
                intervention: None,
                detail: None,
                checkpoint: Some(CheckpointStatus {
                    request_id: "saved".into(),
                    position,
                    state: CheckpointState::Captured,
                }),
            },
        )
        .unwrap();
    let mut spare = catalogue
        .control_receipt(&id, "saved")
        .unwrap()
        .unwrap()
        .state
        .clone();
    if let ControlState::Acknowledged {
        checkpoint: Some(checkpoint),
        ..
    } = &mut spare
    {
        checkpoint.request_id = "spare".into();
    }
    catalogue
        .accept_control(
            &id,
            ControlRequest {
                request_id: "spare".into(),
                command: ControlCommand::Checkpoint,
            },
        )
        .unwrap();
    catalogue.acknowledge_control(&id, "spare", spare).unwrap();
    assert_eq!(
        catalogue
            .captured_checkpoint(&id, "saved")
            .unwrap()
            .position,
        position
    );
    let mut branch = test_support::resolved_request("independent-descendant");
    branch.spec = source.spec.clone();
    branch
        .metadata
        .annotations
        .insert("derived-from".into(), id.to_string());
    let (created, _) = catalogue.submit(branch).unwrap();
    assert_ne!(created.metadata.id, id);
    assert_eq!(created.metadata.parent, None);
    assert_eq!(created.spec.seed, source.spec.seed);
    assert_eq!(catalogue.get(&id).unwrap(), &source);
    source.status.phase = Phase::Completed;
    source.status.termination = Some(seedbox_api::ExecutionTermination::Stopped);
    catalogue.update(source).unwrap();
    assert_eq!(catalogue.checkpoints(&id).unwrap().len(), 2);
    assert_eq!(catalogue.begin_delete(&id).unwrap(), vec![id.clone()]);
    let marked = catalogue.get(&id).unwrap().metadata.clone();
    assert!(marked.deletion_requested_at_unix_ms.is_some());
    catalogue.begin_delete(&id).unwrap();
    assert_eq!(catalogue.get(&id).unwrap().metadata, marked);
    catalogue.delete(&id).unwrap();
    assert!(catalogue.get(&created.metadata.id).is_ok());
    let mut child = catalogue.get(&created.metadata.id).unwrap().clone();
    child.status.phase = Phase::Completed;
    child.status.termination = Some(seedbox_api::ExecutionTermination::Stopped);
    catalogue.update(child).unwrap();
    catalogue.delete(&created.metadata.id).unwrap();
    catalogue.delete(&id).unwrap();
    catalogue.delete(&id).unwrap();
    assert_eq!(catalogue.list().count(), 0);
}

#[test]
fn seed_range_waits_for_frozen_inputs_and_continues_once() {
    use seedbox_api::{InputSource, SeedRangeRequest};
    let mut catalogue = Catalogue::in_memory();
    let base = request();
    let request = SeedRangeRequest {
        metadata: Default::default(),
        options: Default::default(),
        request_id: base.request_id,
        spec: base.spec,
        count: 3,
    };
    let (mut source, created) = catalogue.submit_seeds(request.clone()).unwrap();
    assert!(created);
    assert!(catalogue.advance_seeds().unwrap().is_none());
    let mut different = request.clone();
    different.count = 4;
    assert!(matches!(
        catalogue.submit_seeds(different),
        Err(Error::RequestConflict)
    ));
    source.status.phase = Phase::Preparing;
    source = catalogue.update(source).unwrap();
    source.spec = test_support::resolved_request("prepared").spec;
    source.status.phase = Phase::Completed;
    source = catalogue.update(source).unwrap();
    let (second, _) = catalogue.advance_seeds().unwrap().unwrap();
    assert_eq!(second.spec.seed, Some(124));
    assert!(matches!(second.spec.inputs, InputSource::Resolved { .. }));
    assert!(!catalogue.submit_seeds(request).unwrap().1);
    let (third, _) = catalogue.advance_seeds().unwrap().unwrap();
    assert_eq!(third.spec.seed, Some(125));
    assert_ne!(third.metadata.id, second.metadata.id);
    assert!(catalogue.advance_seeds().unwrap().is_none());
    let range = catalogue.seed_range(&source.metadata.id).unwrap();
    assert_eq!(range.members.len(), 3);
    assert_eq!(catalogue.stop_seeds(&source.metadata.id).unwrap().len(), 3);
    assert!(
        catalogue
            .seed_range(&source.metadata.id)
            .unwrap()
            .stop_requested
    );
    // The range is a management grouping; each submitted simulation is independent.
    for id in [&second.metadata.id, &third.metadata.id] {
        let mut member = catalogue.get(id).unwrap().clone();
        member.status.phase = Phase::Completed;
        member.status.termination = Some(seedbox_api::ExecutionTermination::Stopped);
        catalogue.update(member).unwrap();
    }
    assert_eq!(
        catalogue.begin_delete(&source.metadata.id).unwrap(),
        vec![source.metadata.id.clone()]
    );
    catalogue.delete(&source.metadata.id).unwrap();
    assert_eq!(catalogue.list().count(), 2);
    for id in [&second.metadata.id, &third.metadata.id] {
        assert!(matches!(
            catalogue.get(id).unwrap().spec.inputs,
            InputSource::Resolved { .. }
        ));
        catalogue.delete(id).unwrap();
    }
    assert_eq!(catalogue.list().count(), 0);
}

#[test]
fn deletion_rejects_new_controls_but_keeps_existing_receipt_retries() {
    use seedbox_api::{ControlCommand, ControlRequest};
    let mut catalogue = Catalogue::in_memory();
    let (mut resource, _) = catalogue.submit(request()).unwrap();
    let id = resource.metadata.id.clone();
    resource.status.phase = Phase::Preparing;
    resource = catalogue.update(resource).unwrap();
    resource.status.phase = Phase::Running;
    catalogue.update(resource).unwrap();
    let request = ControlRequest {
        request_id: "before-delete".into(),
        command: ControlCommand::Step,
    };
    let (receipt, _) = catalogue.accept_control(&id, request.clone()).unwrap();
    catalogue.begin_delete(&id).unwrap();
    assert_eq!(
        catalogue.accept_control(&id, request).unwrap(),
        (receipt, false)
    );
    assert!(matches!(
        catalogue.accept_control(
            &id,
            ControlRequest {
                request_id: "after-delete".into(),
                command: ControlCommand::Step,
            }
        ),
        Err(Error::ControlUnavailable)
    ));
}

#[test]
fn resolved_definitions_allow_independent_edits_and_outlive_their_source() {
    use seedbox_api::InputSource;
    let mut catalogue = Catalogue::in_memory();
    let (mut original, _) = catalogue
        .submit(test_support::resolved_request("original"))
        .unwrap();
    for phase in [Phase::Preparing, Phase::Running, Phase::Completed] {
        original.status.phase = phase;
        original = catalogue.update(original).unwrap();
    }
    let mut edited = test_support::resolved_request("edited-timeline");
    edited.spec = original.spec.clone();
    edited
        .metadata
        .annotations
        .insert("derived-from".into(), original.metadata.id.to_string());
    edited
        .spec
        .program
        .push(seedbox_spec::definition::Override {
            timeout_ns: seedbox_engine_protocol::input_program::default_timeout_ns(),
            id: "authored-1".into(),
            at_ns: 1_000_000_000,
            action: serde_json::json!({"action":"stop","machine":"oracle"}),
        });
    let InputSource::Resolved { definition } = &mut edited.spec.inputs else {
        panic!("expected resolved fixture")
    };
    definition.network.latency_ns += 1;
    let (scheduled, _) = catalogue.submit(edited.clone()).unwrap();
    assert_eq!(scheduled.spec.program, edited.spec.program);
    assert_eq!(
        scheduled.spec.configuration.network.latency_ns,
        original.spec.configuration.network.latency_ns + 1
    );
    assert_eq!(scheduled.metadata.parent, None);
    catalogue.begin_delete(&original.metadata.id).unwrap();
    catalogue.delete(&original.metadata.id).unwrap();
    assert_eq!(catalogue.get(&scheduled.metadata.id).unwrap(), &scheduled);
    edited.request_id = "after-source-deletion".into();
    edited.spec.seed = Some(124);
    let (independent, _) = catalogue.submit(edited).unwrap();
    assert_eq!(independent.spec.seed, Some(124));
    let json = serde_json::to_value(&independent.spec).unwrap();
    assert!(json.get("inputs").is_none() && json.get("configuration").is_none());
    assert!(json.get("runtime").is_some() && json.get("oracle").is_some());
}

#[test]
fn malformed_timeline_actions_reject_but_state_dependent_heals_are_admitted() {
    let mut catalogue = Catalogue::in_memory();
    for action in [
        seedbox_engine_protocol::control::Action::Stop {
            machine: "unknown".into(),
        },
        seedbox_engine_protocol::control::Action::Throttle {
            machine: "oracle".into(),
            handle: "slow".into(),
            factor_permille: 0,
        },
    ] {
        let mut invalid = request();
        invalid
            .spec
            .program
            .push(seedbox_spec::definition::Override {
                timeout_ns: seedbox_engine_protocol::input_program::default_timeout_ns(),
                id: "authored-1".into(),
                at_ns: 1,
                action: serde_json::to_value(action).unwrap(),
            });
        assert!(matches!(catalogue.submit(invalid), Err(Error::Invalid(_))));
        assert_eq!(catalogue.list().count(), 0);
    }
}

#[test]
fn state_dependent_heal_can_be_submitted_for_deterministic_runtime_rejection() {
    let mut catalogue = Catalogue::in_memory();
    let mut submission = request();
    submission
        .spec
        .program
        .push(seedbox_spec::definition::Override {
            timeout_ns: seedbox_engine_protocol::input_program::default_timeout_ns(),
            id: "heal-removed-prior-action".into(),
            at_ns: 1,
            action: serde_json::json!({"action":"heal","handle":"deleted-owner"}),
        });
    let (accepted, _) = catalogue.submit(submission).unwrap();
    assert_eq!(accepted.spec.program[0].id, "heal-removed-prior-action");
}
