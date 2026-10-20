#![allow(dead_code)]

use seedbox_test_time::HangGuard;
use std::{
    fs::{self, File},
    path::{Path, PathBuf},
    process::{Child, Command, Output, Stdio},
    time::Duration,
};

pub fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap()
}

struct OwnedChild(Child);
impl Drop for OwnedChild {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// File-backed capture avoids a full stdout pipe deadlocking the timeout loop.
/// All process state is child-local; no environment or cwd is changed globally.
pub fn run(command: &mut Command, seconds: u64) -> Output {
    let capture = tempfile::tempdir().unwrap();
    let stdout = capture.path().join("stdout");
    let stderr = capture.path().join("stderr");
    command
        .stdin(Stdio::null())
        .stdout(File::create(&stdout).unwrap())
        .stderr(File::create(&stderr).unwrap());
    let mut child = OwnedChild(
        seedbox_test::process::spawn(command).unwrap_or_else(|e| panic!("{command:?}: {e}")),
    );
    let guard = HangGuard::new("xtask fixture process exit", Duration::from_secs(seconds));
    let status = guard
        .wait_for(|| child.0.try_wait().transpose())
        .unwrap_or_else(|error| {
            panic!(
                "{command:?} exceeded {seconds}s: {error}\nstdout: {}\nstderr: {}",
                fs::read_to_string(&stdout).unwrap_or_default(),
                fs::read_to_string(&stderr).unwrap_or_default()
            )
        })
        .unwrap();
    Output {
        status,
        stdout: fs::read(stdout).unwrap(),
        stderr: fs::read(stderr).unwrap(),
    }
}

pub fn success(command: &mut Command, seconds: u64) -> Output {
    let output = run(command, seconds);
    assert!(
        output.status.success(),
        "{command:?}: {}",
        diagnostic(&output)
    );
    output
}

pub fn diagnostic(output: &Output) -> String {
    format!(
        "status: {}\nstdout: {}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

pub fn python(tool: &str) -> Command {
    let mut command = Command::new("python3");
    command.arg(root().join("tools/runtime").join(tool));
    command
}
