//! Public CLI interactions with an ordinary image-backed, real native Driver run.
//! This is not the host-only service fixture or a fabricated execution report.
use super::*;
use seedbox_api::{
    EventDelivery, EventKind, ExecutionTermination, LogDelivery, LogEntry, Phase, TestVerdict,
};

fn wait(service: &Service, id: &str, condition: &str) -> serde_json::Value {
    // The timeout bounds a hung public command; success requires the requested
    // actual resource/playback/simulated-time condition, never elapsed host time.
    let output = service.cli(&["wait", id, "--for", condition, "--timeout", "15m", "--json"]);
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["outcome"], "MET");
    assert_eq!(report["simulation"], id);
    assert_eq!(report["condition"], condition);
    report
}

fn control(service: &Service, id: &str, command: &str) {
    let request = seedbox_client::Client::request_id().unwrap();
    let output = service.cli(&["control", command, id, "--request-id", &request, "--json"]);
    let mut receipt: seedbox_api::ControlReceipt = serde_json::from_slice(&output.stdout).unwrap();
    let guard = HangGuard::new(
        "public CLI control must publish its acknowledged receipt",
        Duration::from_secs(60),
    );
    loop {
        assert_eq!(receipt.request.request_id, request);
        match &receipt.state {
            seedbox_api::ControlState::Acknowledged { error: None, .. } => break,
            seedbox_api::ControlState::Accepted => {}
            other => panic!("public {command} not acknowledged successfully: {other:?}"),
        }
        guard
            .remaining()
            .expect("control receipt infrastructure hang");
        let output = service.cli(&["control", "receipt", id, &request, "--json"]);
        receipt = serde_json::from_slice(&output.stdout).unwrap();
    }
}

#[test]
fn public_driver_cli_run_logs_events_wait_replay_and_shutdown() {
    let mut service = Service::new("pv");
    let oracle = r#"import json,socket
print('DRIVER_CLI_BEGIN',flush=True)
s=socket.socket(socket.AF_UNIX); s.connect('/run/seedbox/control.sock'); reader=s.makefile('rb')
def call(i,op,**args):
 s.sendall((json.dumps(dict(version=1,id=i,op=op,**args))+'\n').encode())
 reply=json.loads(reader.readline()); assert reply['version']==1 and reply['id']==i and reply['ok'],reply
 return reply
before=call(1,'now')['result']['now_ns']
call(2,'sleep',duration_ns=1000000)
after=call(3,'now')['result']['now_ns']
assert after>=before+1000000,(before,after)
print('DRIVER_CLI_DONE',flush=True)
"#;
    let project = service.path().join("seedbox.yaml");
    fs::write(&project, serde_json::to_vec(&serde_json::json!({
        "name":"driver-cli-interactions",
        "machines":{},
        "oracle":{"image":"docker.io/library/python:3.13-alpine", "command":["python3","-u","-c",oracle]}
    })).unwrap()).unwrap();
    let accepted = service.cli(&[
        "run",
        project.to_str().unwrap(),
        "--paused",
        "--detach",
        "--seed",
        "1009",
        "--wall-time",
        "15m",
        "--json",
    ]);
    let resource: seedbox_api::Simulation = serde_json::from_slice(&accepted.stdout).unwrap();
    let id = resource.metadata.id.to_string();
    // --paused stops before guest boot too. Waiting for application execution
    // here would deadlock the test itself; step/resume must admit that progress.
    wait(&service, &id, "paused");
    service.assert_profile(&id);

    let catalogue = service.cli(&["show", "--json"]);
    let catalogue: Vec<serde_json::Value> = serde_json::from_slice(&catalogue.stdout).unwrap();
    assert!(catalogue.iter().any(|row| row["metadata"]["id"] == id));
    let playback = || {
        let output = service.cli(&["show", "--view", "playback", &id, "--json"]);
        serde_json::from_slice::<seedbox_api::PlaybackObservation>(&output.stdout)
            .unwrap()
            .state
    };
    let before = playback().position.completed_rounds;
    control(&service, &id, "step");
    // A step commits one round, not a promised amount of simulated time;
    // bootstrap can legitimately make a same-instant commit.
    let guard = HangGuard::new(
        "public step must commit its admitted round",
        Duration::from_secs(900),
    );
    loop {
        guard.remaining().expect("step commit infrastructure hang");
        let state = playback();
        if !state.stepping && state.position.completed_rounds > before {
            assert_eq!(state.position.completed_rounds, before + 1);
            break;
        }
    }
    control(&service, &id, "pause");
    wait(&service, &id, "paused");
    control(&service, &id, "resume");
    // The oracle's sleep, rather than a host interval or one-round assumption,
    // establishes this workload-derived simulated-time condition.
    let progressed = wait(&service, &id, "sim-time=0.001s");
    assert!(progressed["simulatedTimeNs"].as_u64().unwrap() >= 1_000_000);
    wait(&service, &id, "completed");
    let completed = service.cli(&["show", "--view", "definition", &id, "--json"]);
    let completed: seedbox_api::Simulation = serde_json::from_slice(&completed.stdout).unwrap();
    assert_eq!(completed.status.phase, Phase::Completed);
    assert_eq!(completed.status.verdict(), TestVerdict::Passed);
    assert_eq!(
        completed.status.termination,
        Some(ExecutionTermination::OracleExited)
    );

    let logs = service.cli(&["logs", &id, "oracle"]);
    let logs = String::from_utf8(logs.stdout).unwrap();
    assert_eq!(logs.matches("DRIVER_CLI_BEGIN").count(), 1);
    assert_eq!(logs.matches("DRIVER_CLI_DONE").count(), 1);
    let structured = service.cli(&["logs", &id, "oracle", "--json"]);
    let mut bytes = Vec::new();
    for line in String::from_utf8(structured.stdout).unwrap().lines() {
        match serde_json::from_str::<LogDelivery>(line).unwrap() {
            LogDelivery::Entry {
                entry: LogEntry::Bytes { bytes: value, .. },
                ..
            } => bytes.extend(value),
            other => panic!("unexpected gap or unavailable application output: {other:?}"),
        }
    }
    assert_eq!(bytes, logs.as_bytes());
    let events = service.cli(&["events", &id]);
    let events: Vec<EventDelivery> = String::from_utf8(events.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert!(!events.is_empty());
    assert!(events
        .iter()
        .all(|event| !matches!(event, EventDelivery::Closed)));
    let followed = service.cli(&["events", &id, "--follow"]);
    let followed: Vec<EventDelivery> = String::from_utf8(followed.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert!(matches!(followed.last(), Some(EventDelivery::Closed)));
    // Delivery sequence numbers are scoped to a stream, not a cross-stream API.
    let payloads = |deliveries: &[EventDelivery]| {
        deliveries
            .iter()
            .map(|delivery| match delivery {
                EventDelivery::Event { observation, .. } => observation.clone(),
                other => panic!("unexpected non-event in retained history: {other:?}"),
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(payloads(&followed[..followed.len() - 1]), payloads(&events));
    assert!(events.iter().any(|delivery| matches!(delivery, EventDelivery::Event { observation, .. }
        if observation.simulation.as_str() == id && matches!(observation.event, EventKind::OracleExited { code: 0 }))));
    assert!(events.iter().all(|delivery| !matches!(
        delivery,
        EventDelivery::Gap { .. }
            | EventDelivery::Event {
                observation: seedbox_api::SimulationEvent {
                    event: EventKind::SourceUnavailable | EventKind::SourceGap { .. },
                    ..
                },
                ..
            }
    )));

    fs::rename(&project, service.path().join("source-removed.yaml")).unwrap();
    service.replay(&id);
    {
        let _barriers = service.cleanup_barriers();
    }
    let stopped = service.cli(&["server", "stop"]);
    assert_eq!(
        String::from_utf8(stopped.stdout).unwrap(),
        "STOPPED: local server\n"
    );
    let mut child = service.child.take().unwrap();
    let status = HangGuard::new(
        "explicit CLI shutdown must reap its owned daemon",
        Duration::from_secs(30),
    )
    .wait_for(|| {
        child
            .try_wait()
            .expect("observe owned daemon after public shutdown")
    })
    .unwrap();
    assert!(status.success());
    assert_eq!(
        fs::read_dir(service.path().join("state/sessions"))
            .unwrap()
            .count(),
        0
    );
    eprintln!("public Driver CLI interactions: run/step/pause/resume/logs/events/wait/replay/shutdown verified; native witness MATCH and owned session reaped");
    service.root.take().unwrap().close().unwrap();
}

#[test]
fn public_checkpoint_starts_a_new_run_without_source_server_or_cache() {
    let artifact = tempfile::tempdir().unwrap();
    let checkpoint = artifact.path().join("before-bug.seedbox");
    let source = Service::new("pv");
    let source_root = source.path().to_owned();
    let project = source.path().join("seedbox.yaml");
    let oracle = "print('CHECKPOINT_BEGIN', flush=True)\nx = 0\nfor i in range(2000000):\n x = (x + i) % 1000000007\nprint('CHECKPOINT_DONE', x, flush=True)\n";
    fs::write(&project, serde_json::to_vec(&serde_json::json!({
        "name": "portable-checkpoint",
        "oracle": {"image": "docker.io/library/python:3.13-alpine", "command": ["python3", "-u", "-c", oracle]}
    })).unwrap()).unwrap();
    let accepted = source.cli(&[
        "run",
        project.to_str().unwrap(),
        "--paused",
        "--detach",
        "--seed",
        "1009",
        "--wall-time",
        "15m",
        "--json",
    ]);
    let simulation: seedbox_api::Simulation = serde_json::from_slice(&accepted.stdout).unwrap();
    let id = simulation.metadata.id.as_str();
    wait(&source, id, "paused");
    let guard = HangGuard::new(
        "oracle reaches a capturable boundary before finishing",
        Duration::from_secs(900),
    );
    let captured = loop {
        guard.remaining().unwrap();
        let before = source.cli(&["show", id, "--view", "playback", "--json"]);
        let before: seedbox_api::PlaybackObservation =
            serde_json::from_slice(&before.stdout).unwrap();
        control(&source, id, "step");
        let state = loop {
            guard.remaining().unwrap();
            let output = source.cli(&["show", id, "--view", "playback", "--json"]);
            let state: seedbox_api::PlaybackObservation =
                serde_json::from_slice(&output.stdout).unwrap();
            assert!(!state.state.terminal, "oracle finished before capture");
            if !state.state.stepping
                && state.state.position.completed_rounds > before.state.position.completed_rounds
            {
                break state.state;
            }
        };
        let logs = source.cli(&["logs", id, "oracle"]);
        let logs = String::from_utf8(logs.stdout).unwrap();
        assert!(!logs.contains("CHECKPOINT_DONE"));
        if state.application_ready && logs.contains("CHECKPOINT_BEGIN") {
            break state.position;
        }
    };
    let saved = source.cli(&[
        "checkpoint",
        id,
        "-o",
        checkpoint.to_str().unwrap(),
        "--json",
    ]);
    let saved: serde_json::Value = serde_json::from_slice(&saved.stdout).unwrap();
    assert!(saved["sizeBytes"].as_u64().unwrap() > 0);
    source.cli(&["stop", id]);
    wait(&source, id, "completed");
    source.close();
    assert!(
        !source_root.exists(),
        "source state and binary cache must be gone"
    );

    let restored = Service::new("pv");
    let accepted = restored.cli(&[
        "run",
        checkpoint.to_str().unwrap(),
        "--paused",
        "--detach",
        "--wall-time",
        "15m",
        "--json",
    ]);
    let simulation: seedbox_api::Simulation = serde_json::from_slice(&accepted.stdout).unwrap();
    assert_ne!(simulation.metadata.id.as_str(), id);
    let restored_id = simulation.metadata.id.as_str();
    wait(&restored, restored_id, "paused");
    let playback = restored.cli(&["show", restored_id, "--view", "playback", "--json"]);
    let playback: seedbox_api::PlaybackObservation =
        serde_json::from_slice(&playback.stdout).unwrap();
    assert_eq!(
        playback.state.position, captured,
        "restore must begin at captured state"
    );
    control(&restored, restored_id, "resume");
    wait(&restored, restored_id, "completed");
    let completed = restored.cli(&["show", restored_id, "--json"]);
    let completed: seedbox_api::Simulation = serde_json::from_slice(&completed.stdout).unwrap();
    assert_eq!(completed.status.verdict(), TestVerdict::Passed);
    let logs = restored.cli(&["logs", restored_id, "oracle"]);
    let logs = String::from_utf8(logs.stdout).unwrap();
    assert!(
        !logs.contains("CHECKPOINT_BEGIN"),
        "prior observations are not restored"
    );
    assert!(logs.contains("CHECKPOINT_DONE"));
    // Default submission must run a checkpoint captured while paused to completion.
    let automatic = restored.cli(&[
        "run",
        checkpoint.to_str().unwrap(),
        "--wall-time",
        "15m",
        "--json",
    ]);
    let automatic: seedbox_api::Simulation = serde_json::from_slice(&automatic.stdout).unwrap();
    assert_eq!(automatic.status.verdict(), TestVerdict::Passed);
    let replayed = restored.cli(&["replay", restored_id, "--wall-time", "15m", "--json"]);
    let replayed: seedbox_api::SimulationComparison =
        serde_json::from_slice(&replayed.stdout).unwrap();
    assert_eq!(replayed.status, seedbox_api::ComparisonStatus::Match);
    restored.close();
}
