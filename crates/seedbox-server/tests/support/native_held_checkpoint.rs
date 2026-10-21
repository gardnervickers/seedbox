//! Public held-throttle checkpoint continuation on the normal retained worker.
#[path = "native_held_checkpoint/authority.rs"]
mod authority;
use super::*;
use seedbox_api::{
    CheckpointState, ControlCommand, ControlState, EventDelivery, EventKind, FaultStatus,
    PlaybackRate,
};
const KEY: &str = "held-throttle";

async fn fault_events(
    session: &NativeSession,
    id: &seedbox_api::SimulationId,
) -> Vec<(u32, FaultStatus)> {
    let mut stream = session.client.events_with(id, false).await.unwrap();
    let mut faults = vec![];
    while let Some(delivery) = stream.next().await {
        match delivery.unwrap() {
            EventDelivery::Event { observation, .. } => match observation.event {
                EventKind::FaultChanged {
                    id,
                    input_key,
                    status,
                    ..
                } if input_key.as_deref() == Some(KEY) => faults.push((id, status)),
                EventKind::SourceGap { .. } | EventKind::SourceUnavailable => {
                    panic!("fault observations unavailable")
                }
                _ => {}
            },
            EventDelivery::Gap { .. } => panic!("fault event stream gap"),
            EventDelivery::Closed => {}
        }
    }
    faults
}
async fn assert_unapplied(session: &NativeSession, id: &seedbox_api::SimulationId) {
    assert!(!fault_events(session, id)
        .await
        .iter()
        .any(|(_, status)| *status == FaultStatus::Applied));
    let world = session.client.world(id).await.unwrap();
    let oracle = world
        .machines
        .iter()
        .find(|machine| machine.name == "oracle")
        .unwrap();
    assert_eq!(
        oracle.throttle_permille.unwrap_or(1000),
        1000,
        "held throttle already took effect"
    );
}

pub async fn native_held_checkpoint(backend: CpuBackend) {
    native_case(move |owner| Box::pin(async move {
        let session = owner.session(backend);
        let mut request = submission(backend, session.root.path(), &session.store, 2_000_000, 123);
        // Choose a live-workload fault point from this backend's actual Driver
        // readiness/end window, never an assumed architecture boot duration.

        let calibration = session.submit(request.clone(), 1).await;
        session.worker(&calibration).await;
        session.complete(&calibration, false, 1, 123).await;
        let finish = session.client.describe(&calibration).await.unwrap().status.simulated_time_ns;
        let mut stream = session.client.events_with(&calibration, false).await.unwrap();
        let mut ready = None;
        while let Some(delivery) = stream.next().await {
            if let EventDelivery::Event { observation, .. } = delivery.unwrap() {
                if matches!(observation.event, EventKind::MachineReady { ref machine, .. } if machine == "oracle") {
                    ready = Some(observation.simulated_time_ns.unwrap());
                }
            }
        }
        let ready = ready.expect("real application readiness event");
        let at = ready + (finish - ready) / 2;
        assert!(at > ready && at < finish);
        eprintln!("held calibration backend={} ready_ns={ready} final_ns={finish} planned_fault_ns={at}", label(backend));

        request.spec.program.push(seedbox_spec::definition::Override {
            id: KEY.into(), at_ns: at, timeout_ns: 1_000_000_000,
            action: serde_json::to_value(seedbox_engine_protocol::control::Action::Throttle {
                handle: "held-scale".into(), machine: "oracle".into(), factor_permille: 2000,
            }).unwrap(),
        });

        let baseline = session.submit(request.clone(), 1).await;

        session.worker(&baseline).await;
        let uninterrupted = session.complete(&baseline, false, 1, 123).await.unwrap();

        let baseline_events = fault_events(session, &baseline).await;
        assert_eq!(baseline_events.iter().filter(|(_, status)| *status == FaultStatus::Applied).count(), 1);
        let mut source_request = request;
        source_request.options.start_paused = true;
        let source = session.submit(source_request, 1).await;

        session.worker(&source).await;

        session.control(&source, "paused-before-arm", ControlCommand::SetRate { rate: PlaybackRate::Paused }).await;

        session.control(&source, "arm", ControlCommand::PauseBeforeNextFault { rate: PlaybackRate::Unlimited }).await;

        let mut previous = None;
        let held = loop {
            let state = session.client.playback(&source).await.unwrap().state;
            assert!(!state.terminal, "source finished before held fault");
            let mark = (state.position, state.rate, state.application_ready, state.stepping);
            if previous.as_ref() != Some(&mark) {
                eprintln!("held wait state={state:?}");
                previous = Some(mark);
            }
            check_bound(&session.client, &source, backend, state.position.simulated_time_ns, "its held fault stop").await;
            // Identify the actual admitted, paused, unapplied hold, rather than
            // comparing the reported boundary with a requested timestamp.
            if state.rate == PlaybackRate::Paused && !state.stepping && state.application_ready {
                let events = fault_events(session, &source).await;
                if events.iter().filter(|(_, status)| *status == FaultStatus::Requested).count() == 1 {
                    assert!(!events.iter().any(|(_, status)| *status == FaultStatus::Applied));
                    break state;
                }
            }
            playback_changed(session, &source, state).await;
        };

        assert!(held.position.completed_rounds > 0 && !held.stepping);
        let bytes = session.output(&source).await;
        assert_eq!(bytes.windows(6).filter(|part| *part == b"START=").count(), 1);
        assert!(!bytes.windows(4).any(|part| part == b"END="));
        assert_unapplied(session, &source).await;
        let source_events = fault_events(session, &source).await;
        let requested: Vec<_> = source_events.iter().filter(|(_, status)| *status == FaultStatus::Requested).map(|(id, _)| *id).collect();
        assert_eq!(requested.len(), 1);
        let fault_id = requested[0];
        let captured = session.control(&source, "held", ControlCommand::Checkpoint).await;
        let position = match &captured.state {
            ControlState::Acknowledged { checkpoint: Some(c), error: None, .. } => { assert_eq!(c.state, CheckpointState::Captured); c.position },
            other => panic!("held capture refused: {other:?}"),
        };
        assert_eq!(position, held.position);
        assert_unapplied(session, &source).await;

        let mut exported = session.client.checkpoint_definition(&source, "held").await.unwrap();

        session.client.stop(&source).await.unwrap();
        session.complete(&source, true, 1, 123).await;
        session.client.delete(&source).await.unwrap();
        assert!(!session.execution_root.join("runs").join(source.as_str()).exists());

        session.workers.store(4, Ordering::SeqCst);
        exported.options.machine_workers = Some(4);
        let restored = session.client.submit_definition(&serde_json::to_value(exported).unwrap(), true).await.unwrap().metadata.id;
        session.submitted += 1;
        assert_ne!(source, restored);

        session.worker(&restored).await;

        let launch = session.launch_request(&restored);
        let resume = launch.resume_checkpoint.as_ref().expect("actual resume required");
        assert!(resume.is_file());
        let saved: wire::Checkpoint = serde_json::from_reader(fs::File::open(resume).unwrap()).unwrap();
        authority::assert_saved_pending(&saved, fault_id);
        assert_eq!(saved.state.world.input_program.next, 1);
        assert_eq!(saved.state.world.input_program.admissions.len(), 1);
        assert_eq!(saved.state.world.input_program.admissions[0].plan.id, fault_id);
        session.control(&restored, "verify-paused-restore", ControlCommand::SetRate { rate: PlaybackRate::Paused }).await;

        let paused = session.client.playback(&restored).await.unwrap().state;
        assert_eq!(paused.position, position);
        assert_eq!(paused.rate, PlaybackRate::Paused);
        assert!(!paused.stepping && !paused.terminal);
        assert_unapplied(session, &restored).await;
        eprintln!("native held checkpoint: backend={} fault_id={fault_id} position={position:?} restored_paused=true applied_before_release=0", label(backend));
        session.control(&restored, "continue", ControlCommand::SetRate { rate: PlaybackRate::Unlimited }).await;
        let continued = session.complete(&restored, false, 4, 123).await.unwrap();

        let events = fault_events(session, &restored).await;
        let applied: Vec<_> = events.iter().filter(|(_, status)| *status == FaultStatus::Applied).map(|(id, _)| *id).collect();
        assert_eq!(applied, [fault_id], "held fault identity changed or applied twice");
        assert!(!events.iter().any(|(_, status)| *status == FaultStatus::Requested), "held input re-admitted after import");
        let world = session.client.world(&restored).await.unwrap();
        assert_eq!(world.machines.iter().find(|m| m.name == "oracle").unwrap().throttle_permille, Some(2000));
        let bytes = session.output(&restored).await;
        assert!(!bytes.windows(6).any(|part| part == b"START="));
        assert_eq!(bytes.windows(4).filter(|part| *part == b"END=").count(), 1);
        assert_eq!(continued, uninterrupted);
        assert_eq!(session.client.compare(&baseline, &restored).await.unwrap().witness_matches, Some(true));
        eprintln!("native held continuation: backend={} same_fault={fault_id} applied_after_release=1 raw_equal=true API_witness_equal=true witness={continued}", label(backend));
    })).await;
}
