//! Hardware acceptance using exact retained inputs, with the ordinary CLI owning
//! admission, CPU/SMT leases, wall limits and cancellation.
#![cfg(all(target_os = "linux", target_arch = "x86_64"))]

use std::{
    fs,
    path::{Path, PathBuf},
    process::{Child, Command, Output},
    time::{Duration, Instant},
};

fn cli() -> Command {
    Command::new(env!("CARGO_BIN_EXE_seedbox"))
}

fn attempt(output: &Output) -> PathBuf {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    PathBuf::from(
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .find_map(|line| line.split_once(": ").map(|(_, path)| path))
            .expect("CLI outcome line"),
    )
}

fn record(run: &Path) -> serde_json::Value {
    let output = cli().args(["inspect", "--json"]).arg(run).output().unwrap();
    assert!(output.status.success());
    serde_json::from_slice(&output.stdout).unwrap()
}

struct Running(Child);
impl Drop for Running {
    fn drop(&mut self) {
        if self.0.try_wait().unwrap().is_none() {
            // Let the executor reap its engine and restore the owned host state.
            unsafe { libc::kill(self.0.id() as i32, libc::SIGTERM) };
            self.0.wait().unwrap();
        }
    }
}

#[test]
fn paused_and_resumed_replay_has_identical_witness_and_full_state() {
    assert_eq!(std::env::consts::OS, "linux");
    let source = PathBuf::from(
        std::env::var_os("SEEDBOX_HOST_PAUSE_RUN")
            .expect("retained full-state run made with the new host-control engine"),
    );
    let source_state = PathBuf::from(
        std::env::var_os("SEEDBOX_HOST_PAUSE_STATE").expect("source content store state directory"),
    );
    assert!(record(&source)["report"]["full_state"].is_object());
    let scratch = tempfile::Builder::new().tempdir_in("/tmp").unwrap();
    let bundle = scratch.path().join("input.tar");
    let state = scratch.path().join("state");
    let exported = cli()
        .arg("export")
        .arg(&source)
        .arg("--state")
        .arg(source_state)
        .arg("-o")
        .arg(&bundle)
        .output()
        .unwrap();
    assert!(
        exported.status.success(),
        "{}",
        String::from_utf8_lossy(&exported.stderr)
    );
    let imported = attempt(
        &cli()
            .arg("import")
            .arg(&bundle)
            .arg("--state")
            .arg(&state)
            .output()
            .unwrap(),
    );
    let replay = || {
        let mut command = cli();
        command
            .arg("replay")
            .arg(&imported)
            .arg("--state")
            .arg(&state);
        command
    };
    let baseline = record(&attempt(&replay().output().unwrap()));
    assert_eq!(baseline["replay"]["status"], "MATCH");
    let stdout = scratch.path().join("paused.stdout");
    let stderr = scratch.path().join("paused.stderr");
    let mut running = Running(
        replay()
            .stdout(fs::File::create(&stdout).unwrap())
            .stderr(fs::File::create(&stderr).unwrap())
            .spawn()
            .unwrap(),
    );
    let started = Instant::now();
    let paused = loop {
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "no live boundary endpoint"
        );
        assert!(
            running.0.try_wait().unwrap().is_none(),
            "{}",
            fs::read_to_string(&stderr).unwrap()
        );
        let candidate = fs::read_dir(state.join("runs"))
            .unwrap()
            .filter_map(|entry| {
                let path = entry.unwrap().path();
                path.join("output/host-control").is_file().then_some(path)
            })
            .next();
        if let Some(run) = candidate {
            let response = cli().arg("pause").arg(&run).output().unwrap();
            if response.status.success() {
                assert_eq!(
                    String::from_utf8_lossy(&response.stdout),
                    format!("PAUSED: {}\n", run.display())
                );
                break run;
            }
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    // Two cycles ensure a resumed world can reach another acknowledged boundary.
    for cycle in 0..2 {
        if cycle != 0 {
            assert!(cli().arg("pause").arg(&paused).status().unwrap().success());
        }
        std::thread::sleep(Duration::from_millis(100));
        assert!(
            running.0.try_wait().unwrap().is_none(),
            "world ended while paused"
        );
        assert!(cli().arg("resume").arg(&paused).status().unwrap().success());
        if cycle == 0 {
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    loop {
        if let Some(status) = running.0.try_wait().unwrap() {
            assert!(status.success(), "{}", fs::read_to_string(&stderr).unwrap());
            break;
        }
        assert!(
            started.elapsed() < Duration::from_secs(120),
            "replay exceeded test bound"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    let result = record(&paused);
    assert_eq!(result["replay"]["status"], "MATCH");
    assert_eq!(result["outcome"], baseline["outcome"]);
    assert_eq!(result["identity"], baseline["identity"]);
    assert_eq!(result["report"]["witness"], baseline["report"]["witness"]);
    assert_eq!(
        result["report"]["full_state"],
        baseline["report"]["full_state"]
    );
    assert!(!paused.join("output/host-control").exists());
}
