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
fn branching_requires_a_captured_checkpoint_and_preserves_source_identity() {
    use seedbox_api::{
        BoundaryPosition, ControlCommand, ControlRequest, ControlState, InputSource, PlaybackRate,
        PlaybackStatus,
    };
    use seedbox_engine_protocol::host_control::{CheckpointState, CheckpointStatus};
    let temp = tempfile::tempdir().unwrap();
    let mut catalogue = Catalogue::open(&temp.path().join("catalogue")).unwrap();
    let (mut source, _) = catalogue.submit(request()).unwrap();
    source.status.phase = Phase::Preparing;
    source = catalogue.update(source).unwrap();
    source.status.phase = Phase::Running;
    source = catalogue.update(source).unwrap();
    let id = source.metadata.id.clone();
    let mut branch = request();
    branch.request_id = "branch".into();
    branch.spec.inputs = InputSource::Checkpoint {
        simulation: id.clone(),
        checkpoint: "saved".into(),
    };
    assert!(catalogue.submit(branch.clone()).is_err());
    catalogue
        .accept_control(
            &id,
            ControlRequest {
                request_id: "saved".into(),
                command: ControlCommand::Checkpoint,
            },
        )
        .unwrap();
    assert!(catalogue.submit(branch.clone()).is_err());
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
    let mut wrong = branch.clone();
    wrong.spec.seed = Some(999);
    assert!(catalogue.submit(wrong).is_err());
    let (created, _) = catalogue.submit(branch).unwrap();
    assert_ne!(created.metadata.id, id);
    assert_eq!(created.metadata.parent, Some(id.clone()));
    assert_eq!(created.metadata.checkpoint.as_deref(), Some("saved"));
    assert_eq!(created.spec.seed, source.spec.seed);
    assert_eq!(catalogue.get(&id).unwrap(), &source);
    source.status.phase = Phase::Completed;
    source.status.termination = Some(seedbox_api::ExecutionTermination::Stopped);
    catalogue.update(source).unwrap();
    assert!(catalogue.delete_checkpoint(&id, "saved").is_err());
    catalogue.delete_checkpoint(&id, "spare").unwrap();
    catalogue.delete_checkpoint(&id, "spare").unwrap();
    assert!(catalogue.captured_checkpoint(&id, "spare").is_err());
    let checkpoints = catalogue.checkpoints(&id).unwrap();
    assert_eq!(checkpoints.len(), 1);
    assert_eq!(checkpoints[0].request.request_id, "saved");
    drop(catalogue);
    let mut reopened = Catalogue::open(&temp.path().join("catalogue")).unwrap();
    assert_eq!(reopened.checkpoints(&id).unwrap(), checkpoints);
    assert!(reopened.captured_checkpoint(&id, "spare").is_err());
    assert_eq!(
        reopened.begin_delete(&id).unwrap(),
        vec![created.metadata.id.clone(), id.clone()]
    );
    let marked = reopened.get(&id).unwrap().metadata.clone();
    assert!(marked.deletion_requested_at_unix_ms.is_some());
    reopened.begin_delete(&id).unwrap();
    assert_eq!(reopened.get(&id).unwrap().metadata, marked);
    let mut late = request();
    late.request_id = "late-branch".into();
    late.spec.inputs = InputSource::Checkpoint {
        simulation: id.clone(),
        checkpoint: "saved".into(),
    };
    assert!(reopened
        .submit(late)
        .unwrap_err()
        .to_string()
        .contains("being deleted"));
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
    source.spec.inputs = InputSource::Retained {
        simulation: source.metadata.id.clone(),
    };
    source.status.phase = Phase::Completed;
    source = catalogue.update(source).unwrap();
    let (second, _) = catalogue.advance_seeds().unwrap().unwrap();
    assert_eq!(second.spec.seed, Some(124));
    assert_eq!(
        second.spec.inputs,
        InputSource::Prepared {
            simulation: source.metadata.id.clone()
        }
    );
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
    let ids = catalogue.begin_delete(&source.metadata.id).unwrap();
    assert_eq!(ids.len(), 3);
    for id in ids {
        let mut member = catalogue.get(&id).unwrap().clone();
        if member.status.phase != Phase::Completed {
            member.status.phase = Phase::Completed;
            member.status.termination = Some(seedbox_api::ExecutionTermination::Stopped);
            catalogue.update(member).unwrap();
        }
        catalogue.delete(&id).unwrap();
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
