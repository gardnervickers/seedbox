//! Pause acceptance reuses the application's server and resolved baseline.
#![cfg(all(target_os = "linux", target_arch = "x86_64"))]
use seedbox_api::{
    ComparisonStatus, ControlCommand, ControlRequest, ControlState, Phase, PlaybackRate,
    PlaybackStatus, Simulation, SimulationId, TestVerdict,
};
use seedbox_client::Client;
use std::{
    path::Path,
    time::{Duration, Instant},
};
async fn set_rate(client: &Client, id: &SimulationId, rate: PlaybackRate) -> PlaybackStatus {
    let request = ControlRequest {
        request_id: Client::request_id().unwrap(),
        command: ControlCommand::SetRate { rate },
    };
    let mut receipt = client.request_control(id, &request).await.unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        match receipt.state {
            ControlState::Acknowledged {
                status,
                error: None,
                ..
            } => {
                assert_eq!(status.rate, rate);
                return status;
            }
            ControlState::Accepted => {}
            state => panic!("playback command failed: {state:?}"),
        }
        assert!(
            Instant::now() < deadline,
            "playback command not acknowledged"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
        receipt = client
            .control_receipt(id, &request.request_id)
            .await
            .unwrap();
    }
}

pub fn verify(state: &Path, source: &Simulation) {
    let client = Client::connect_to(&state.join("server.sock")).unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let rerun = client
            .submit(&seedbox_api::SubmitRequest {
                metadata: Default::default(),
                options: seedbox_api::ExecutionOptions {
                    start_paused: true,
                    wall_time_ns: Some(600_000_000_000),
                },
                request_id: Client::request_id().unwrap(),
                spec: source.spec.clone(),
            })
            .await
            .unwrap();
        let id = &rerun.metadata.id;
        let deadline = Instant::now() + Duration::from_secs(120);
        loop {
            match client.playback(id).await {
                Ok(observation) => {
                    assert!(!observation.state.terminal, "run ended before resume");
                    assert_eq!(observation.state.rate, PlaybackRate::Paused);
                    assert_eq!(observation.state.position.completed_rounds, 0);
                    assert_eq!(observation.state.position.simulated_time_ns, 0);
                    break;
                }
                Err(seedbox_client::Error::Rejected {
                    code: seedbox_api::ErrorCode::Unavailable,
                    ..
                }) => {}
                Err(error) => panic!("playback unavailable: {error}"),
            }
            let current = client.describe(id).await.unwrap();
            assert_ne!(current.status.phase, Phase::Completed, "{current:?}");
            assert!(Instant::now() < deadline, "no live playback endpoint");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let paused = set_rate(&client, id, PlaybackRate::Paused).await;
        tokio::time::sleep(Duration::from_millis(100)).await;
        let observation = client.playback(id).await.unwrap();
        assert_eq!(observation.state.rate, PlaybackRate::Paused);
        assert_eq!(observation.state.position, paused.position);
        assert_ne!(
            client.describe(id).await.unwrap().status.phase,
            Phase::Completed
        );
        set_rate(&client, id, PlaybackRate::Unlimited).await;

        let deadline = Instant::now() + Duration::from_secs(600);
        loop {
            let current = client.describe(id).await.unwrap();
            if current.status.phase == Phase::Completed {
                assert_eq!(current.status.verdict(), TestVerdict::Passed, "{current:?}");
                break;
            }
            assert!(Instant::now() < deadline, "rerun exceeded test bound");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let comparison = client.compare(&source.metadata.id, id).await.unwrap();
        assert_eq!(comparison.status, ComparisonStatus::Match);
        assert!(comparison.outcome_matches);
        assert_eq!(comparison.witness_matches, Some(true));
        assert_eq!(comparison.full_state_matches, Some(true));
    });
}
