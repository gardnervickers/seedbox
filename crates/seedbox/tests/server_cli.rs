#![cfg(unix)]

use seedbox_client::Client;
use seedbox_test::process::Guarded;
use std::{
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::Duration,
};

struct ServerGuard {
    socket: PathBuf,
    pid: u32,
}

impl Drop for ServerGuard {
    fn drop(&mut self) {
        let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        else {
            return;
        };
        let Ok(client) = Client::connect_to(&self.socket) else {
            return;
        };
        if runtime
            .block_on(client.info())
            .is_ok_and(|info| info.process_id == self.pid)
        {
            // SAFETY: this PID was just verified by the server on our private test
            // socket. It is the daemon started by this test, not a saved PID.
            unsafe {
                libc::kill(self.pid as i32, libc::SIGTERM);
            }
            runtime
                .block_on(async {
                    tokio::time::timeout(Duration::from_secs(30), async {
                        while client.info().await.is_ok() {}
                    })
                    .await
                })
                .expect("private server closes its listener after SIGTERM");
        }
    }
}

fn cli(root: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_seedbox"));
    command
        .env_clear()
        .env("HOME", root)
        .env("PATH", "/usr/bin:/bin")
        .env("SEEDBOX_STATE", root.join("state"))
        .env("SEEDBOX_KERNEL", root.join("missing-kernel"));
    command
}

#[test]
fn clients_share_explicit_server_and_inspect_a_retained_setup_failure() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let mut server = cli(root)
        .args(["server", "start", "--foreground", "--startup-handshake"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .guarded_spawn()
        .unwrap();
    let mut ready = String::new();
    std::io::BufRead::read_line(
        &mut std::io::BufReader::new(server.stdout.take().unwrap()),
        &mut ready,
    )
    .unwrap();
    assert!(ready.contains("ready"), "{ready}");
    let mut children: Vec<_> = (0..2)
        .map(|_| {
            cli(root)
                .args(["show", "--json"])
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .guarded_spawn()
                .unwrap()
        })
        .collect();
    let first = children.remove(0).wait_with_output().unwrap();
    let client = Client::connect_to(&root.join("state/server.sock")).unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let info = runtime.block_on(client.info()).unwrap();
    let _server = ServerGuard {
        socket: root.join("state/server.sock"),
        pid: info.process_id,
    };
    let second = children.remove(0).wait_with_output().unwrap();
    for result in [first, second] {
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&result.stdout).unwrap(),
            serde_json::json!([])
        );
    }

    let compose = root.join("seedbox.yaml");
    std::fs::write(
        &compose,
        "name: server-test\nmachines: {}\noracle:\n  image: example/oracle\n",
    )
    .unwrap();
    let result = cli(root)
        .arg("run")
        .arg(&compose)
        .args(["--paused", "--json"])
        .guarded_output()
        .unwrap();
    assert_eq!(
        result.status.code(),
        Some(2),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let simulation: seedbox_api::Simulation = serde_json::from_slice(&result.stdout)
        .unwrap_or_else(|error| {
            panic!(
                "{error}: stderr={}",
                String::from_utf8_lossy(&result.stderr)
            )
        });
    assert_eq!(simulation.status.phase, seedbox_api::Phase::Completed);
    assert_eq!(
        simulation.status.verdict(),
        seedbox_api::TestVerdict::Incomplete
    );
    assert!(simulation.spec.seed.is_some());
    assert!(simulation.options.start_paused);
    let described = cli(root)
        .args([
            "show",
            "--view",
            "definition",
            simulation.metadata.id.as_str(),
            "--json",
        ])
        .guarded_output()
        .unwrap();
    assert!(described.status.success());
    assert_eq!(
        serde_json::from_slice::<seedbox_api::Simulation>(&described.stdout).unwrap(),
        simulation
    );
    let watched = cli(root)
        .args(["show", simulation.metadata.id.as_str(), "--watch", "--json"])
        .guarded_output()
        .unwrap();
    assert!(watched.status.success());
    assert_eq!(
        serde_json::from_slice::<seedbox_api::Simulation>(&watched.stdout).unwrap(),
        simulation
    );
    let events = cli(root)
        .args(["events", simulation.metadata.id.as_str()])
        .guarded_output()
        .unwrap();
    assert!(
        events.status.success(),
        "{}",
        String::from_utf8_lossy(&events.stderr)
    );
    // A completed simulation's retained events print in order, then the
    // command exits without following.
    let deliveries: Vec<seedbox_api::EventDelivery> = String::from_utf8_lossy(&events.stdout)
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert!(!deliveries.is_empty());
    assert!(deliveries
        .iter()
        .all(|d| matches!(d, seedbox_api::EventDelivery::Event { .. })));
    assert_eq!(
        runtime.block_on(client.info()).unwrap().process_id,
        info.process_id
    );
    let logs = cli(root)
        .args(["logs", simulation.metadata.id.as_str(), "oracle", "--json"])
        .guarded_output()
        .unwrap();
    assert!(
        logs.status.success(),
        "{}",
        String::from_utf8_lossy(&logs.stderr)
    );
    // Nothing was retained for a setup failure; without --follow the command
    // prints nothing and exits rather than waiting.
    assert!(logs.stdout.is_empty());
    let playback = cli(root)
        .args([
            "show",
            "--view",
            "playback",
            simulation.metadata.id.as_str(),
            "--json",
        ])
        .guarded_output()
        .unwrap();
    assert_eq!(playback.status.code(), Some(2));
    assert!(playback.stdout.is_empty());
    assert_eq!(String::from_utf8(playback.stderr).unwrap(), "error: server rejected request (Unavailable): simulation has no active worker control endpoint\n");
    let exploration = cli(root)
        .arg("run")
        .arg(&compose)
        .args(["--count", "3", "--detach", "--json"])
        .guarded_output()
        .unwrap();
    assert!(
        exploration.status.success(),
        "{}",
        String::from_utf8_lossy(&exploration.stderr)
    );
    let mut range: seedbox_api::SeedRangeStatus =
        serde_json::from_slice(&exploration.stdout).unwrap();
    runtime
        .block_on(async {
            tokio::time::timeout(Duration::from_secs(120), async {
                while !range.complete() {
                    range = client.seed_range(&range.source).await.unwrap();
                }
            })
            .await
        })
        .expect("seed range publishes its completed preparation failure");
    assert_eq!(range.count, 3);
    assert_eq!(
        range.members.len(),
        1,
        "failed preparation must not be retried for every seed"
    );
    assert!(range.error.as_deref().unwrap().contains("prepared inputs"));
    let session = std::fs::read_dir(root.join("state/sessions"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| path.is_dir())
        .unwrap();
    let private = session.join("runs").join(simulation.metadata.id.as_str());
    std::fs::create_dir_all(&private).unwrap();
    std::fs::write(private.join("temporary-output"), b"owned").unwrap();
    for id in [&simulation.metadata.id, &range.source] {
        for _ in 0..2 {
            let deleted = cli(root)
                .args(["delete", id.as_str()])
                .guarded_output()
                .unwrap();
            assert!(
                deleted.status.success(),
                "{}",
                String::from_utf8_lossy(&deleted.stderr)
            );
        }
        assert!(runtime.block_on(client.describe(id)).is_err());
    }
    assert!(!private.exists());
    assert!(runtime
        .block_on(client.list(None))
        .unwrap()
        .items
        .is_empty());
    let stopped = cli(root).args(["server", "stop"]).guarded_output().unwrap();
    assert!(
        stopped.status.success(),
        "{}",
        String::from_utf8_lossy(&stopped.stderr)
    );
    server.wait().unwrap();
    assert!(!session.exists());
    assert!(!root.join("state/catalogue").exists());
    drop(_server);
}

#[test]
fn noninteractive_commands_do_not_start_a_server() {
    let temp = tempfile::tempdir().unwrap();
    let result = cli(temp.path())
        .args(["show", "--json"])
        .guarded_output()
        .unwrap();
    assert_eq!(result.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&result.stderr).contains("start `seedbox server start`"));
    assert!(!temp.path().join("state/server.sock").exists());
}

#[test]
fn background_server_start_and_client_startup_options_share_state() {
    let root = tempfile::tempdir().unwrap();
    let state = root.path().join("explicit-state");
    let started = cli(root.path())
        .args(["server", "start", "--full-state", "--state"])
        .arg(&state)
        .guarded_output()
        .unwrap();
    assert!(
        started.status.success(),
        "{}",
        String::from_utf8_lossy(&started.stderr)
    );
    let client = Client::connect_to(&state.join("server.sock")).unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let info = runtime.block_on(client.info()).unwrap();
    let _server = ServerGuard {
        socket: state.join("server.sock"),
        pid: info.process_id,
    };
    let shown = cli(root.path())
        .args(["show", "--json", "--state"])
        .arg(&state)
        .guarded_output()
        .unwrap();
    assert!(
        shown.status.success(),
        "{}",
        String::from_utf8_lossy(&shown.stderr)
    );
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&shown.stdout).unwrap(),
        serde_json::json!([])
    );
    assert!(
        !root.path().join("state/server.sock").exists(),
        "explicit --state must override SEEDBOX_STATE"
    );
    let stopped = cli(root.path())
        .args(["server", "stop", "--state"])
        .arg(&state)
        .guarded_output()
        .unwrap();
    assert!(
        stopped.status.success(),
        "{}",
        String::from_utf8_lossy(&stopped.stderr)
    );
    assert!(runtime.block_on(client.info()).is_err());
}

#[test]
fn status_reports_a_stopped_server_without_creating_state() {
    let root = tempfile::tempdir().unwrap();
    let status = cli(root.path())
        .args(["server", "status"])
        .guarded_output()
        .unwrap();
    assert!(
        status.status.success(),
        "{}",
        String::from_utf8_lossy(&status.stderr)
    );
    assert_eq!(status.stdout, b"STOPPED: local server\n");
    assert!(status.stderr.is_empty());
    let json = cli(root.path())
        .args(["server", "status", "--json"])
        .guarded_output()
        .unwrap();
    assert!(json.status.success());
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&json.stdout).unwrap(),
        serde_json::json!({"server": null, "capacity": null})
    );
    assert!(!root.path().join("state").exists());
}
