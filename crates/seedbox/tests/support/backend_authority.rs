//! Metadata-only CLI regressions: no image engine, guest or execution report.
use super::*;
use seedbox_api::{Simulation, SimulationSpec};

fn resource(tag: &str) -> serde_json::Value {
    use seedbox_spec::definition as d;
    let project: SimulationSpec =
        serde_json::from_value(serde_json::json!({"oracle":{"image":"fixture/oracle"}})).unwrap();
    let asset = d::Asset(format!("sha256:{}", "0".repeat(64)));
    let definition = d::Definition {
        architecture: d::Architecture::X86_64,
        seed: 7,
        clock: d::Clock {
            epoch_seconds: 86400,
        },
        runtime: d::Runtime {
            kernel: asset.clone(),
            engine: asset.clone(),
            execution: serde_json::json!({"backend":tag,"semantic_parameters":{"engine":{"kind":"driver-world-v1","engine_backend":tag}}}),
        },
        machines: Default::default(),
        oracle: d::Machine {
            image: None,
            name: Some("oracle".into()),
            boot_archive: asset,
            memory_bytes: 1024 * 1024 * 1024,
            process: d::Process {
                argv: vec!["/app".into()],
                environment: Default::default(),
                working_directory: "/".into(),
                user: "".into(),
            },
            volumes: Default::default(),
            anonymous_volumes: vec![],
        },
        network: project.configuration.network,
        simulator: d::Simulator {
            blocks_per_progress_call: project.configuration.simulator.blocks_per_progress_call,
            schedule: Default::default(),
            disk: Default::default(),
            oracle_control: true,
        },
        start: d::Start::Fresh,
        overrides: vec![],
    };
    serde_json::to_value(Simulation::queued(
        seedbox_api::Metadata {
            name: "metadata-only".into(),
            labels: Default::default(),
            annotations: Default::default(),
            id: "deadbeef".parse().unwrap(),
            revision: 1,
            created_at_unix_ms: 0,
            deletion_requested_at_unix_ms: None,
            parent: None,
            checkpoint: None,
        },
        SimulationSpec::resolved(definition).unwrap(),
    ))
    .unwrap()
}
fn contradictory(top: &str, actual: &str) -> serde_json::Value {
    let mut document = resource(actual);
    document["spec"]["runtime"]["execution"]["backend"] = top.into();
    document
}
fn directory() -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix("e9a-")
        .tempdir_in("/tmp")
        .unwrap()
}
#[test]
fn backend_authority_saved_run_and_explore_reject_both_conflicting_directions() {
    for (top, actual, flag) in [
        ("driver-pv", "driver-tcg", "pv"),
        ("driver-tcg", "driver-pv", "tcg"),
    ] {
        for command in ["run", "explore"] {
            let root = directory();
            let path = root.path().join("saved.yaml");
            let bytes = serde_json::to_string(&contradictory(top, actual)).unwrap();
            fs::write(&path, &bytes).unwrap();
            let parse_error =
                serde_yaml_ng::from_str::<seedbox_api::SimulationDocument>(&bytes).unwrap_err();
            let expected = seedbox_application::Error::from(parse_error).to_string();
            let output = seedbox(
                &[command, path.to_str().unwrap(), "--backend", flag, "--json"],
                root.path(),
            );
            assert_eq!(output.status.code(), Some(2));
            assert_eq!(stderr(&output), format!("error: {expected}\n"));
            assert!(stderr(&output).contains("inconsistent Driver backend metadata"));
            assert!(
                !root.path().join(".local/state/seedbox").exists(),
                "must reject before daemon connection/submission"
            );
        }
    }
}

/// Read-only fake HTTP resource, not a fake engine. The second request is the
/// existing CLI helper's cleanup info lookup, which is refused without a PID.
fn serve_resource(root: &Path, body: String) -> std::thread::JoinHandle<Vec<String>> {
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixListener;
    let state = root.join(".local/state/seedbox");
    use std::os::unix::fs::DirBuilderExt;
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&state)
        .unwrap();
    let listener = UnixListener::bind(state.join("server.sock")).unwrap();
    listener.set_nonblocking(true).unwrap();
    std::thread::spawn(move || {
        let guard = seedbox_test_time::HangGuard::new(
            "metadata-only CLI requests must finish",
            std::time::Duration::from_secs(120),
        );
        let mut requests = Vec::new();
        while requests.len() < 2 {
            let (mut stream, _) = guard
                .wait_for(|| match listener.accept() {
                    Ok(stream) => Some(Ok(stream)),
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => None,
                    Err(error) => Some(Err(error)),
                })
                .unwrap()
                .unwrap();
            // test-time: allow(hang-guard) -- Bound the positive HTTP header read, never a guest result or absence assertion.
            stream
                .set_read_timeout(Some(guard.remaining().unwrap()))
                .unwrap();
            let mut reader = BufReader::new(&mut stream);
            let mut line = String::new();
            if reader.read_line(&mut line).unwrap() == 0 {
                continue;
            } // startup liveness connection
            requests.push(line.trim().to_owned());
            loop {
                let mut header = String::new();
                if reader.read_line(&mut header).unwrap() == 0 || header == "\r\n" {
                    break;
                }
            }
            let (status, response) = if line.starts_with("GET /v1/simulations/") {
                ("200 OK", body.as_str())
            } else {
                ("404 Not Found", "{}")
            };
            write!(stream, "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}", response.len()).unwrap();
        }
        requests
    })
}
#[test]
fn backend_authority_replay_and_rerun_reject_both_conflicting_directions() {
    for (top, actual, flag) in [
        ("driver-pv", "driver-tcg", "pv"),
        ("driver-tcg", "driver-pv", "tcg"),
    ] {
        for command in ["replay", "rerun"] {
            let root = directory();
            let body = contradictory(top, actual).to_string();
            let parse_error = serde_json::from_slice::<Simulation>(body.as_bytes()).unwrap_err();
            let expected = seedbox_client::Error::from(parse_error).to_string();
            let server = serve_resource(root.path(), body);
            let output = seedbox(
                &[command, "deadbeef", "--backend", flag, "--json"],
                root.path(),
            );
            let requests = server.join().unwrap();
            assert_eq!(output.status.code(), Some(2));
            assert_eq!(stderr(&output), format!("error: {expected}\n"));
            assert!(
                stderr(&output).contains("inconsistent Driver backend metadata"),
                "{}",
                stderr(&output)
            );
            assert!(requests
                .iter()
                .any(|request| request.starts_with("GET /v1/simulations/deadbeef ")));
            assert!(
                requests.iter().all(|request| request.starts_with("GET ")),
                "invalid retained metadata must never be submitted: {requests:?}"
            );
        }
    }
}
#[test]
fn backend_authority_matching_metadata_reconstructs_the_recorded_cpu() {
    for (tag, cpu) in [
        ("driver-pv", seedbox_spec::project::CpuBackend::Pv),
        ("driver-tcg", seedbox_spec::project::CpuBackend::Tcg),
    ] {
        let value = resource(tag);
        let simulation: Simulation = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(simulation.spec.configuration.simulator.backend, cpu);
        let saved: seedbox_api::SimulationDocument = serde_json::from_value(value).unwrap();
        assert_eq!(saved.spec.configuration.simulator.backend, cpu);
    }
}
