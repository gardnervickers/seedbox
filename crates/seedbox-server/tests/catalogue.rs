#![cfg(unix)]

mod support;
use seedbox_api::Phase;
use seedbox_server::catalogue::{Catalogue, Error};
use support::request;

#[test]
fn accepted_submission_and_request_identity_survive_restart() {
    let temp = tempfile::tempdir().unwrap();
    let directory = temp.path().join("catalogue");
    let mut catalogue = Catalogue::open(&directory).unwrap();
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
    drop(catalogue);

    let mut catalogue = Catalogue::open(&directory).unwrap();
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
fn catalogue_excludes_competing_servers_and_releases_ownership_on_drop() {
    let temp = tempfile::tempdir().unwrap();
    let directory = temp.path().join("catalogue");
    let first = Catalogue::open(&directory).unwrap();
    assert!(matches!(
        Catalogue::open(&directory),
        Err(Error::AlreadyOwned(_))
    ));
    drop(first);
    assert!(Catalogue::open(&directory).is_ok());
}

#[test]
fn random_seed_is_allocated_once_and_collision_checked_ids_are_distinct() {
    let temp = tempfile::tempdir().unwrap();
    let mut catalogue = Catalogue::open(&temp.path().join("catalogue")).unwrap();
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
fn control_receipts_share_ordering_survive_restart_and_never_reapply_after_completion() {
    use seedbox_api::{
        BoundaryPosition, ControlCommand, ControlRequest, ControlState, PlaybackRate,
        PlaybackStatus,
    };
    let temp = tempfile::tempdir().unwrap();
    let directory = temp.path().join("catalogue");
    let mut catalogue = Catalogue::open(&directory).unwrap();
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
    drop(catalogue);
    let mut catalogue = Catalogue::open(&directory).unwrap();
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
            terminal: false,
            application_ready: false,
        },
        error: None,
    };
    let ack = catalogue
        .acknowledge_control(&id, &request.request_id, state.clone())
        .unwrap();
    catalogue.accept_control(&id, other.clone()).unwrap();
    drop(catalogue);
    let manager = seedbox_server::manager::Manager::new(Catalogue::open(&directory).unwrap());
    manager.recover_interrupted().unwrap();
    manager
        .read(|catalogue| {
            assert_eq!(
                catalogue
                    .control_receipt(&id, &request.request_id)?
                    .unwrap(),
                &ack
            );
            assert_eq!(
                catalogue
                    .control_receipt(&id, &other.request_id)?
                    .unwrap()
                    .state,
                ControlState::Interrupted
            );
            assert!(catalogue.pending_controls().is_empty());
            Ok(())
        })
        .unwrap();
    drop(manager);
    let mut catalogue = Catalogue::open(&directory).unwrap();
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
    let temp = tempfile::tempdir().unwrap();
    let mut catalogue = Catalogue::open(&temp.path().join("catalogue")).unwrap();
    let (mut source, _) = catalogue
        .submit(support::resolved_request("source"))
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
    assert!(catalogue.delete_checkpoint(&id, "saved").is_err());
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
    let mut branch = support::resolved_request("independent-descendant");
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
    catalogue.delete_checkpoint(&id, "saved").unwrap();
    catalogue.delete_checkpoint(&id, "spare").unwrap();
    catalogue.delete_checkpoint(&id, "spare").unwrap();
    assert!(catalogue.captured_checkpoint(&id, "spare").is_err());
    let checkpoints = catalogue.checkpoints(&id).unwrap();
    assert!(checkpoints.is_empty());
    drop(catalogue);
    let mut reopened = Catalogue::open(&temp.path().join("catalogue")).unwrap();
    assert_eq!(reopened.checkpoints(&id).unwrap(), checkpoints);
    assert!(reopened.captured_checkpoint(&id, "spare").is_err());
    assert_eq!(reopened.begin_delete(&id).unwrap(), vec![id.clone()]);
    let marked = reopened.get(&id).unwrap().metadata.clone();
    assert!(marked.deletion_requested_at_unix_ms.is_some());
    reopened.begin_delete(&id).unwrap();
    assert_eq!(reopened.get(&id).unwrap().metadata, marked);
    reopened.delete(&id).unwrap();
    assert!(reopened.get(&created.metadata.id).is_ok());
    let mut child = reopened.get(&created.metadata.id).unwrap().clone();
    child.status.phase = Phase::Completed;
    child.status.termination = Some(seedbox_api::ExecutionTermination::Stopped);
    reopened.update(child).unwrap();
    reopened.delete(&created.metadata.id).unwrap();
    reopened.delete(&id).unwrap();
    reopened.delete(&id).unwrap();
    drop(reopened);
    assert_eq!(
        Catalogue::open(&temp.path().join("catalogue"))
            .unwrap()
            .list()
            .count(),
        0
    );
}

#[test]
fn seed_range_waits_for_frozen_inputs_and_continues_once_after_restart() {
    use seedbox_api::{InputSource, SeedRangeRequest};
    let temp = tempfile::tempdir().unwrap();
    let directory = temp.path().join("catalogue");
    let mut catalogue = Catalogue::open(&directory).unwrap();
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
    source.spec = support::resolved_request("prepared").spec;
    source.status.phase = Phase::Completed;
    source = catalogue.update(source).unwrap();
    let (second, _) = catalogue.advance_seeds().unwrap().unwrap();
    assert_eq!(second.spec.seed, Some(124));
    assert!(matches!(second.spec.inputs, InputSource::Resolved { .. }));
    drop(catalogue);
    let mut catalogue = Catalogue::open(&directory).unwrap();
    assert!(!catalogue.submit_seeds(request).unwrap().1);
    let (third, _) = catalogue.advance_seeds().unwrap().unwrap();
    assert_eq!(third.spec.seed, Some(125));
    assert_ne!(third.metadata.id, second.metadata.id);
    assert!(catalogue.advance_seeds().unwrap().is_none());
    let range = catalogue.seed_range(&source.metadata.id).unwrap();
    assert_eq!(range.members.len(), 3);
    catalogue.stop_seeds(&source.metadata.id).unwrap();
    drop(catalogue);
    let mut catalogue = Catalogue::open(&directory).unwrap();
    assert_eq!(catalogue.stopped_seed_members().unwrap().len(), 2);
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
    drop(catalogue);
    assert_eq!(Catalogue::open(&directory).unwrap().list().count(), 0);
}

#[test]
fn deletion_rejects_new_controls_but_keeps_existing_receipt_retries() {
    use seedbox_api::{ControlCommand, ControlRequest};
    let temp = tempfile::tempdir().unwrap();
    let mut catalogue = Catalogue::open(&temp.path().join("catalogue")).unwrap();
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
    let temp = tempfile::tempdir().unwrap();
    let mut catalogue = Catalogue::open(&temp.path().join("catalogue")).unwrap();
    let (mut original, _) = catalogue
        .submit(support::resolved_request("original"))
        .unwrap();
    for phase in [Phase::Preparing, Phase::Running, Phase::Completed] {
        original.status.phase = phase;
        original = catalogue.update(original).unwrap();
    }
    let mut edited = support::resolved_request("edited-timeline");
    edited.spec = original.spec.clone();
    edited
        .metadata
        .annotations
        .insert("derived-from".into(), original.metadata.id.to_string());
    edited.spec.program.push(seedbox_api::ProgramInput {
        timeout_ns: seedbox_engine_protocol::input_program::default_timeout_ns(),
        key: "authored-1".into(),
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
    let temp = tempfile::tempdir().unwrap();
    let mut catalogue = Catalogue::open(&temp.path().join("catalogue")).unwrap();
    for action in [
        seedbox_control::Action::Stop {
            machine: "unknown".into(),
        },
        seedbox_control::Action::Throttle {
            machine: "oracle".into(),
            handle: "slow".into(),
            factor_permille: 0,
        },
    ] {
        let mut invalid = request();
        invalid.spec.program.push(seedbox_api::ProgramInput {
            timeout_ns: seedbox_engine_protocol::input_program::default_timeout_ns(),
            key: "authored-1".into(),
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
    submission.spec.program.push(seedbox_api::ProgramInput {
        timeout_ns: seedbox_engine_protocol::input_program::default_timeout_ns(),
        key: "heal-removed-prior-action".into(),
        at_ns: 1,
        action: serde_json::json!({"action":"heal","handle":"deleted-owner"}),
    });
    let (accepted, _) = catalogue.submit(submission).unwrap();
    assert_eq!(accepted.spec.program[0].key, "heal-removed-prior-action");
}

mod seed_ranges {
    use super::*;
    use seedbox_api::{InputSource, SeedRangeRequest, Simulation, SimulationSpec, SubmitRequest};
    use std::path::Path;

    fn request(name: &str) -> SeedRangeRequest {
        SeedRangeRequest {
            metadata: Default::default(),
            options: Default::default(),
            request_id: name.into(),
            count: 3,
            spec: SimulationSpec {
                program: Vec::new(),
                start_paused: false,
                seed: Some(123),
                inputs: InputSource::Project,
                configuration: seedbox_application::config::parse(
                    "name: test\nservices:\n  oracle:\n    image: example/oracle\n",
                    Path::new("/project/compose.yaml"),
                    &Default::default(),
                )
                .unwrap(),
            },
        }
    }

    fn complete(catalogue: &mut Catalogue, mut source: Simulation) -> Simulation {
        source.status.phase = Phase::Preparing;
        source = catalogue.update(source).unwrap();
        source.spec.inputs = InputSource::Retained {
            simulation: source.metadata.id.clone(),
        };
        source.status.phase = Phase::Completed;
        catalogue.update(source).unwrap()
    }

    #[test]
    fn member_namespace_is_reserved_and_range_failure_is_durable_and_isolated() {
        let temp = tempfile::tempdir().unwrap();
        let directory = temp.path().join("catalogue");
        let mut catalogue = Catalogue::open(&directory).unwrap();
        let (source, _) = catalogue.submit_seeds(request("broken-range")).unwrap();
        let id = source.metadata.id.clone();
        let collision = SubmitRequest {
            metadata: Default::default(),
            options: Default::default(),
            request_id: format!("seeds-{id}-1"),
            spec: request("unused").spec,
        };
        assert!(matches!(
            catalogue.submit(collision.clone()),
            Err(Error::Invalid(_))
        ));
        assert!(matches!(
            catalogue.submit_seeds(SeedRangeRequest {
                metadata: Default::default(),
                options: Default::default(),
                request_id: collision.request_id.clone(),
                spec: collision.spec.clone(),
                count: 3,
            }),
            Err(Error::Invalid(_))
        ));
        // Model a conflicting retained entry from before namespace reservation.
        let mut legacy = collision.clone();
        legacy.request_id = "legacy".into();
        let legacy = catalogue.submit(legacy).unwrap().0;
        let source = complete(&mut catalogue, source);
        drop(catalogue);
        let path = directory.join(format!("{}.json", legacy.metadata.id));
        let mut retained: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        retained["request"]["requestId"] = serde_json::json!(collision.request_id);
        std::fs::write(path, serde_json::to_vec(&retained).unwrap()).unwrap();
        let mut catalogue = Catalogue::open(&directory).unwrap();
        assert_eq!(source.status.phase, Phase::Completed);
        assert!(catalogue.advance_seeds().unwrap().is_none());
        assert!(catalogue
            .seed_range(&id)
            .unwrap()
            .error
            .unwrap()
            .contains("request ID"));
        let peer = catalogue.submit_seeds(request("healthy-range")).unwrap().0;
        let peer = complete(&mut catalogue, peer);
        drop(catalogue);
        let mut catalogue = Catalogue::open(&directory).unwrap();
        assert!(catalogue.seed_range(&id).unwrap().error.is_some());
        let (member, _) = catalogue.advance_seeds().unwrap().unwrap();
        assert_eq!(member.metadata.parent, None);
        assert_eq!(member.spec.seed, Some(124));
        assert_eq!(catalogue.seed_range(&id).unwrap().members.len(), 1);
        assert_eq!(
            catalogue
                .seed_range(&peer.metadata.id)
                .unwrap()
                .members
                .len(),
            2
        );
    }

    #[test]
    fn retained_execution_keeps_seed_while_prepared_inputs_allow_exploration() {
        let temp = tempfile::tempdir().unwrap();
        let mut catalogue = Catalogue::open(&temp.path().join("catalogue")).unwrap();
        let base = request("source");
        let source = catalogue
            .submit(SubmitRequest {
                metadata: Default::default(),
                options: Default::default(),
                request_id: base.request_id,
                spec: base.spec,
            })
            .unwrap()
            .0;
        let source = complete(&mut catalogue, source);
        let mut rerun = SubmitRequest {
            metadata: Default::default(),
            options: Default::default(),
            request_id: "rerun".into(),
            spec: source.spec.clone(),
        };
        rerun.spec.seed = Some(124);
        assert!(matches!(
            catalogue.submit(rerun.clone()),
            Err(Error::Invalid(_))
        ));
        rerun.spec.inputs = InputSource::Prepared {
            simulation: source.metadata.id.clone(),
        };
        assert_eq!(catalogue.submit(rerun).unwrap().0.spec.seed, Some(124));
        assert!(catalogue
            .submit(SubmitRequest {
                metadata: Default::default(),
                options: Default::default(),
                request_id: "same-seed".into(),
                spec: source.spec,
            })
            .is_ok());
    }
}
