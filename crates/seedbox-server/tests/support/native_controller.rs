//! Real prepared-fixture oracle over its guest socket and the public service.
use super::*;
use seedbox_api::{CheckpointState, ControlCommand, ControlState, InputSource, PlaybackRate};
const ORACLE: &str = "control-leader";
const SEED: u64 = 123;
const SLEEP_NS: u64 = 20_000_000;

fn request(session: &NativeSession) -> SubmitRequest {
    let binary = fs::read(
        std::env::var_os("SEEDBOX_PV_PREPARED_FIXTURE")
            .expect("own-source machinePvTestFixtures/bin/prepared-fixture"),
    )
    .unwrap();
    let mut definition = seedbox_machine::test_support::prepared_image::definition(&binary, false);
    definition
        .application
        .launch
        .argv
        .push("--control-probe".into());
    let filesystem = session.root.path().join("controller.tar");
    fs::write(&filesystem, definition.application.filesystem.as_bytes()).unwrap();
    fixture_submission(
        DriverBackend::Pv,
        session.root.path(),
        &session.store,
        SEED,
        NativeFixture {
            name: ORACLE,
            filesystem,
            launch: definition.application.launch,
            control: true,
            request_id: "controller".into(),
            extra_machines: vec![],
        },
    )
}
async fn output(session: &NativeSession, id: &seedbox_api::SimulationId) -> Vec<u8> {
    let mut logs = session.client.logs_with(id, ORACLE, false).await.unwrap();
    let mut bytes = vec![];
    while let Some(delivery) = logs.next().await {
        if let seedbox_api::LogDelivery::Entry {
            entry: seedbox_api::LogEntry::Bytes { bytes: data, .. },
            ..
        } = delivery.unwrap()
        {
            bytes.extend(data);
        }
    }
    bytes
}
fn replies(bytes: &[u8]) -> Vec<serde_json::Value> {
    std::str::from_utf8(bytes)
        .unwrap()
        .lines()
        .filter_map(|line| {
            line.strip_prefix("ORACLE_REPLY ")
                .map(|json| serde_json::from_str(json).unwrap())
        })
        .collect()
}
fn number(reply: &serde_json::Value, name: &str) -> u64 {
    reply[name].as_u64().unwrap()
}
fn verify_replies(replies: &[serde_json::Value], ids: &[u64], launch: &wire::Request) {
    assert_eq!(launch.prepared.oracle.name, ORACLE);
    let policy = launch.prepared.inventory.backend_policy().unwrap();
    assert_eq!(policy.controller.as_deref(), Some(ORACLE));
    let latency = launch
        .prepared
        .network
        .latency_ns
        .checked_mul(2)
        .unwrap()
        .checked_add(policy.stop_tolerance_ns.checked_mul(2).unwrap())
        .unwrap()
        .checked_add(1_000_000)
        .unwrap();
    assert_eq!(
        replies.len(),
        ids.len(),
        "missing or duplicated guest replies"
    );
    for (reply, &id) in replies.iter().zip(ids) {
        assert_eq!(number(reply, "version"), 1);
        assert_eq!(number(reply, "id"), id);
        assert_eq!(reply["ok"], true, "guest control error: {reply}");
        let requested = number(reply, "requested_at_ns");
        let admitted = number(reply, "admitted_at_ns");
        let replied = number(reply, "replied_at_ns");
        assert_eq!(admitted.checked_sub(requested), Some(latency));
        let operation_time = if id == 2 { SLEEP_NS } else { 0 };
        assert_eq!(
            replied.checked_sub(admitted),
            Some(operation_time + latency)
        );
        if id != 2 {
            assert_eq!(number(&reply["result"], "now_ns"), admitted);
        }
        eprintln!("native oracle reply: oracle={ORACLE} id={id} latency_ns={latency} {reply}");
    }
}
fn occurrences(bytes: &[u8], marker: &[u8]) -> usize {
    bytes
        .windows(marker.len())
        .filter(|part| *part == marker)
        .count()
}

pub async fn pv_controller_repeat() {
    // test-time: allow(hang-guard) -- Outer HANG_GUARD retains the documented 1200s loaded-host safety budget; check_bound independently limits simulated progress and exact evidence decides success.
    tokio::time::timeout(HANG_GUARD, async {
        let mut session = NativeSession::new(DriverBackend::Pv);
        let request = request(&session);
        let first = session.submit(request.clone(), 1).await;
        session.worker(&first).await;
        let first_witness = session.complete(&first, false, 1, SEED).await.unwrap();
        let first_output = output(&session, &first).await;
        let first_replies = replies(&first_output);
        verify_replies(&first_replies, &[1,2,3], &session.launch_request(&first));
        assert_eq!(occurrences(&first_output, b"ORACLE_CONTROL_DONE"), 1);
        let second = session.submit(request, 4).await;
        assert_ne!(first, second);
        session.worker(&second).await;
        let second_witness = session.complete(&second, false, 4, SEED).await.unwrap();
        let second_output = output(&session, &second).await;
        let second_replies = replies(&second_output);
        verify_replies(&second_replies, &[1,2,3], &session.launch_request(&second));
        assert_eq!(occurrences(&second_output, b"ORACLE_CONTROL_DONE"), 1);
        assert_eq!(first_replies, second_replies);
        assert_eq!(first_witness, second_witness);
        let compared = session.client.compare(&first, &second).await.unwrap();
        assert_eq!(compared.status, ComparisonStatus::Match);
        assert_eq!(compared.witness_matches, Some(true));
        eprintln!("native controller repeat: distinct_ids=true actual_workers=1/4 raw_equal=true replies_equal=true witness={first_witness}");
        session.close().await;
    }).await.expect("native controller repeat hit the host hang guard without finishing");
}

pub async fn pv_controller_checkpoint() {
    // test-time: allow(hang-guard) -- Outer HANG_GUARD retains the documented 1200s loaded-host safety budget; check_bound independently limits simulated progress and exact evidence decides success.
    tokio::time::timeout(HANG_GUARD, async {
        let mut session = NativeSession::new(DriverBackend::Pv);
        let request = request(&session);
        let baseline = session.submit(request.clone(), 1).await;
        session.worker(&baseline).await;
        let uninterrupted = session.complete(&baseline, false, 1, SEED).await.unwrap();
        let baseline_output = output(&session, &baseline).await;
        let baseline_replies = replies(&baseline_output);
        verify_replies(&baseline_replies, &[1,2,3], &session.launch_request(&baseline));
        let admitted = number(&baseline_replies[1], "admitted_at_ns");
        let target = admitted + SLEEP_NS / 2;
        let wake = admitted + SLEEP_NS;
        let mut capture_request = request;
        capture_request.options.start_paused = true;
        let source = session.submit(capture_request, 1).await;
        session.worker(&source).await;
        session.control(&source, "source-paused", ControlCommand::SetRate { rate: PlaybackRate::Paused }).await;
        let partial = loop {
            let before = session.client.playback(&source).await.unwrap().state.position.completed_rounds;
            session.control(&source, &format!("step-{before}"), ControlCommand::Step).await;
            let position = loop {
                let state = session.client.playback(&source).await.unwrap().state;
                assert!(!state.terminal, "source ended before pending-call capture");
                check_bound(
                    &session.client,
                    &source,
                    DriverBackend::Pv,
                    state.position.simulated_time_ns,
                    "its pending-call capture point",
                )
                .await;
                if !state.stepping && state.position.completed_rounds > before { break state; }
                tokio::time::sleep(Duration::from_millis(5)).await;
            };
            let bytes = output(&session, &source).await;
            let partial = replies(&bytes);
            assert!(partial.len() <= 1, "sleep already replied before capture");
            assert_eq!(occurrences(&bytes, b"ORACLE_CONTROL_DONE"), 0);
            if position.position.simulated_time_ns >= target {
                assert!(position.application_ready);
                assert!(position.position.simulated_time_ns < wake);
                assert_eq!(occurrences(&bytes, b"ORACLE_SLEEP_SENT"), 1);
                assert_eq!(partial, baseline_replies[..1]);
                break partial;
            }
        };
        assert_eq!(partial.len(), 1);
        let captured = session.control(&source, "saved", ControlCommand::Checkpoint).await;
        let position = match &captured.state {
            ControlState::Acknowledged { checkpoint: Some(c), error: None, .. } => {
                assert_eq!(c.state, CheckpointState::Captured); c.position
            },
            other => panic!("capture failed: {other:?}"),
        };
        assert!(position.completed_rounds > 0);
        assert!(position.simulated_time_ns >= target && position.simulated_time_ns < wake);
        let exported = session.client.checkpoint_definition(&source, "saved").await.unwrap();
        let InputSource::Resolved { definition } = &exported.spec.inputs else { panic!("resolved export required") };
        assert!(definition.simulator.oracle_control);
        let seedbox_spec::definition::Start::Checkpoint { assets, .. } = &definition.start else { panic!("checkpoint export required") };
        assert!(!assets.is_empty());
        session.client.stop(&source).await.unwrap();
        session.complete(&source, true, 1, SEED).await;
        session.client.delete_checkpoint(&source, "saved").await.unwrap();
        session.client.delete(&source).await.unwrap();
        assert!(!session.execution_root.join("runs").join(source.as_str()).exists());
        session.workers.store(4, Ordering::SeqCst);
        let restored = session.client.submit_definition(&serde_json::to_value(exported).unwrap(), true).await.unwrap().metadata.id;
        session.submitted += 1;
        assert_ne!(restored, source);
        session.worker(&restored).await;
        let launch = session.launch_request(&restored);
        assert!(launch.resume_checkpoint.as_ref().unwrap().is_file());
        assert_eq!(launch.prepared.oracle.name, ORACLE);
        assert_eq!(launch.prepared.inventory.backend_policy().unwrap().controller.as_deref(), Some(ORACLE));
        session.control(&restored, "verify-paused-restore", ControlCommand::SetRate { rate: PlaybackRate::Paused }).await;
        let paused = session.client.playback(&restored).await.unwrap().state;
        assert_eq!(paused.rate, PlaybackRate::Paused);
        assert!(!paused.stepping && !paused.terminal);
        assert_eq!(paused.position, position);
        eprintln!("native controller capture: oracle={ORACLE} before_wake=true pending_sleep_id=2 captured={position:?} restored={:?}", paused.position);
        session.control(&restored, "continue", ControlCommand::SetRate { rate: PlaybackRate::Unlimited }).await;
        let continued = session.complete(&restored, false, 4, SEED).await.unwrap();
        let continued_output = output(&session, &restored).await;
        let continued_replies = replies(&continued_output);
        verify_replies(&continued_replies, &[2,3], &launch);
        assert_eq!(continued_replies, baseline_replies[1..]);
        assert_eq!(occurrences(&continued_output, b"ORACLE_SLEEP_SENT"), 0, "controller restarted or call replayed");
        assert_eq!(occurrences(&continued_output, b"ORACLE_CONTROL_DONE"), 1);
        assert_eq!(uninterrupted, continued);
        let compared = session.client.compare(&baseline, &restored).await.unwrap();
        assert_eq!(compared.witness_matches, Some(true));
        eprintln!("native controller continuation: same_oracle=true no_duplicate_calls=true raw_equal=true witness={continued}");
        session.close().await;
    }).await.expect("native controller checkpoint hit the host hang guard without finishing");
}
