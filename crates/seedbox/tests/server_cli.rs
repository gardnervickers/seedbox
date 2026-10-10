#![cfg(unix)]

use seedbox_client::Client;
use std::{
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};

struct ServerGuard {
    socket: PathBuf,
    pid: u32,
}

impl Drop for ServerGuard {
    fn drop(&mut self) {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let client = Client::connect_to(&self.socket).unwrap();
        if runtime
            .block_on(async { tokio::time::timeout(Duration::from_secs(2), client.info()).await })
            .is_ok_and(|result| result.is_ok_and(|info| info.process_id == self.pid))
        {
            // SAFETY: this PID was just verified by the server on our private test
            // socket. It is the daemon started by this test, not a saved PID.
            unsafe {
                libc::kill(self.pid as i32, libc::SIGTERM);
            }
            let deadline = Instant::now() + Duration::from_secs(5);
            while Instant::now() < deadline
                && runtime
                    .block_on(async {
                        tokio::time::timeout(Duration::from_millis(200), client.info()).await
                    })
                    .is_ok_and(|result| result.is_ok())
            {
                std::thread::sleep(Duration::from_millis(20));
            }
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
fn clients_share_automatic_server_and_inspect_a_retained_setup_failure() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let mut children: Vec<_> = (0..2)
        .map(|_| {
            cli(root)
                .args(["get", "simulations", "--json"])
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
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

    let compose = root.join("compose.yaml");
    std::fs::write(
        &compose,
        "name: server-test\nservices:\n  oracle:\n    image: example/oracle\n",
    )
    .unwrap();
    let result = cli(root)
        .arg("run")
        .arg(&compose)
        .arg("--json")
        .output()
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
    let checkpoints = cli(root)
        .args([
            "get",
            "checkpoints",
            simulation.metadata.id.as_str(),
            "--json",
        ])
        .output()
        .unwrap();
    assert!(
        checkpoints.status.success(),
        "{}",
        String::from_utf8_lossy(&checkpoints.stderr)
    );
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&checkpoints.stdout).unwrap(),
        serde_json::json!([])
    );
    let missing = cli(root)
        .args([
            "describe",
            "checkpoint",
            simulation.metadata.id.as_str(),
            "missing",
            "--json",
        ])
        .output()
        .unwrap();
    assert_eq!(missing.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&missing.stderr).contains("not found"));

    let described = cli(root)
        .args([
            "describe",
            "simulation",
            simulation.metadata.id.as_str(),
            "--json",
        ])
        .output()
        .unwrap();
    assert!(described.status.success());
    assert_eq!(
        serde_json::from_slice::<seedbox_api::Simulation>(&described.stdout).unwrap(),
        simulation
    );
    let watched = cli(root)
        .args([
            "get",
            "simulation",
            simulation.metadata.id.as_str(),
            "--watch",
            "--json",
        ])
        .output()
        .unwrap();
    assert!(watched.status.success());
    assert_eq!(
        serde_json::from_slice::<seedbox_api::Simulation>(&watched.stdout).unwrap(),
        simulation
    );
    let events = cli(root)
        .args(["events", simulation.metadata.id.as_str()])
        .output()
        .unwrap();
    assert!(
        events.status.success(),
        "{}",
        String::from_utf8_lossy(&events.stderr)
    );
    assert_eq!(
        serde_json::from_slice::<seedbox_api::EventDelivery>(&events.stdout).unwrap(),
        seedbox_api::EventDelivery::Closed
    );
    assert_eq!(
        runtime.block_on(client.info()).unwrap().process_id,
        info.process_id
    );
    let logs = cli(root)
        .args(["logs", simulation.metadata.id.as_str(), "oracle", "--json"])
        .output()
        .unwrap();
    assert!(
        logs.status.success(),
        "{}",
        String::from_utf8_lossy(&logs.stderr)
    );
    assert_eq!(
        serde_json::from_slice::<seedbox_api::LogDelivery>(&logs.stdout).unwrap(),
        seedbox_api::LogDelivery::Closed
    );
    let playback = cli(root)
        .args(["playback", simulation.metadata.id.as_str(), "--json"])
        .output()
        .unwrap();
    assert_eq!(playback.status.code(), Some(2));
    assert!(playback.stdout.is_empty());
    assert_eq!(String::from_utf8(playback.stderr).unwrap(), "error: server rejected request (Unavailable): simulation has no active worker control endpoint\n");
    let deleted = cli(root)
        .args([
            "delete",
            "checkpoint",
            simulation.metadata.id.as_str(),
            "missing",
        ])
        .output()
        .unwrap();
    assert_eq!(deleted.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&deleted.stderr)
        .contains("checkpoint missing has not been captured"));
    let exploration = cli(root)
        .arg("explore")
        .arg(&compose)
        .args(["--count", "3", "--detach", "--json"])
        .output()
        .unwrap();
    assert!(
        exploration.status.success(),
        "{}",
        String::from_utf8_lossy(&exploration.stderr)
    );
    let mut range: seedbox_api::SeedRangeStatus =
        serde_json::from_slice(&exploration.stdout).unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while !range.complete() {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(20));
        range = runtime.block_on(client.seed_range(&range.source)).unwrap();
    }
    assert_eq!(range.count, 3);
    assert_eq!(
        range.members.len(),
        1,
        "failed preparation must not be retried for every seed"
    );
    assert!(range.error.as_deref().unwrap().contains("prepared inputs"));
    let private = root
        .join("state/runs")
        .join(simulation.metadata.id.as_str());
    std::fs::create_dir_all(&private).unwrap();
    std::fs::write(private.join("temporary-output"), b"owned").unwrap();
    for id in [&simulation.metadata.id, &range.source] {
        for _ in 0..2 {
            let deleted = cli(root)
                .args(["delete", "simulation", id.as_str()])
                .output()
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
}
