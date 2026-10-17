//! Hardware acceptance of server-owned pause/resume using exact retained inputs.
#![cfg(all(target_os = "linux", target_arch = "x86_64"))]

use seedbox_api::{
    ComparisonStatus, ControlCommand, ControlRequest, ControlState, Phase, PlaybackRate,
    PlaybackStatus, Simulation, SimulationId, TestVerdict,
};
use seedbox_client::Client;
use seedbox_test::process::Guarded;
use std::{
    io::{BufRead, BufReader},
    path::Path,
    process::{Child, Command, Output, Stdio},
    time::Duration,
};

fn cli(binary: &Path, state: &Path) -> Command {
    let mut command = Command::new(binary);
    command
        .env_remove("SEEDBOX_VMM")
        .env("SEEDBOX_STATE", state);
    command
}

fn simulation(output: Output) -> Simulation {
    assert!(
        output.status.success(),
        "stdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

async fn set_rate(client: &Client, id: &SimulationId, rate: PlaybackRate) -> PlaybackStatus {
    let request = ControlRequest {
        request_id: Client::request_id().unwrap(),
        command: ControlCommand::SetRate { rate },
    };
    let mut receipt = client.request_control(id, &request).await.unwrap();
    tokio::time::timeout(Duration::from_secs(120), async {
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
            receipt = client
                .control_receipt(id, &request.request_id)
                .await
                .unwrap();
        }
    })
    .await
    .expect("playback command acknowledgement")
}

#[test]
fn paused_start_and_resume_has_identical_witness_and_full_state() {
    let binary = std::env::var_os("SEEDBOX_TEST_CLI")
        .expect("harness/check.sh supplies the current release CLI");
    let reports =
        std::env::var_os("SEEDBOX_TEST_REPORT_DIR").expect("set a fresh SEEDBOX_TEST_REPORT_DIR");
    let state = Path::new(&reports).join("pause-server");
    let binary = Path::new(&binary);
    let _server = Server::start(binary, &state);
    let workload = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../harness/acceptance/workloads/key-value.yaml");
    let source = simulation(
        cli(binary, &state)
            .arg("run")
            .arg(&workload)
            .args(["--seed", "1009", "--json"])
            .guarded_output()
            .unwrap(),
    );
    assert_eq!(source.status.verdict(), TestVerdict::Passed);
    let rerun = simulation(
        cli(binary, &state)
            .arg("run")
            .arg(&workload)
            .args(["--seed", "1009", "--paused", "--detach", "--json"])
            .guarded_output()
            .unwrap(),
    );
    let client = Client::connect_to(&state.join("server.sock")).unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        tokio::time::timeout(Duration::from_secs(900), async {
            let id = &rerun.metadata.id;
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

            loop {
                let current = client.describe(id).await.unwrap();
                if current.status.phase == Phase::Completed {
                    assert_eq!(current.status.verdict(), TestVerdict::Passed, "{current:?}");
                    break;
                }
            }
            let comparison = client.compare(&source.metadata.id, id).await.unwrap();
            assert_eq!(comparison.status, ComparisonStatus::Match);
            assert!(comparison.outcome_matches);
            assert_eq!(comparison.witness_matches, Some(true));
            assert_eq!(comparison.full_state_matches, Some(true));
        })
        .await
        .expect("pause/resume acceptance completes");
    });
}

/// The fixture owns its server; retained reports outlive the process.
struct Server(Child);
impl Server {
    fn start(binary: &Path, state: &Path) -> Self {
        let mut server = Self(
            cli(binary, state)
                .args(["serve", "--startup-handshake", "--full-state"])
                .stdout(Stdio::piped())
                .guarded_spawn()
                .unwrap(),
        );
        let mut ready = String::new();
        BufReader::new(server.0.stdout.take().unwrap())
            .read_line(&mut ready)
            .unwrap();
        let ready: serde_json::Value = serde_json::from_str(&ready).unwrap();
        assert_eq!(ready["status"], "ready");
        server
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        if self.0.try_wait().ok().flatten().is_some() {
            return;
        }
        // SAFETY: this is the unreaped child owned by this fixture.
        unsafe {
            libc::kill(self.0.id() as i32, libc::SIGTERM);
        }
        self.0.wait().expect("reap owned pause-test server");
    }
}
