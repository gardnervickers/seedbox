//! Host-only report comparability checks against the actual ARM report CLI.
//! compare.py remains the production report comparator, not a Python test runner.
use serde_json::{json, Value};
use std::{
    ffi::OsStr,
    fs::{self, File},
    io,
    path::{Path, PathBuf},
    process::{Child, Command, Output, Stdio},
    thread,
    time::{Duration, Instant},
};

struct OwnedChild(Child);
impl Drop for OwnedChild {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

// File-backed capture avoids pipe-capacity deadlocks while enforcing a deadline.
// Only this test's child is owned; the comparator starts no descendants.
fn run(command: &mut Command) -> io::Result<Output> {
    let capture = tempfile::tempdir()?;
    let stdout = capture.path().join("stdout");
    let stderr = capture.path().join("stderr");
    let mut child = OwnedChild(
        command
            .stdin(Stdio::null())
            .stdout(File::create(&stdout)?)
            .stderr(File::create(&stderr)?)
            .spawn()?,
    );
    let start = Instant::now();
    let status = loop {
        if let Some(status) = child.0.try_wait()? {
            break status;
        }
        if start.elapsed() >= Duration::from_secs(10) {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("{command:?} exceeded 10s"),
            ));
        }
        thread::sleep(Duration::from_millis(10));
    };
    Ok(Output {
        status,
        stdout: fs::read(stdout)?,
        stderr: fs::read(stderr)?,
    })
}

struct Reports {
    directory: tempfile::TempDir,
    left: PathBuf,
    right: PathBuf,
    report: Value,
}
impl Reports {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let left = directory.path().join("a.json");
        let right = directory.path().join("b.json");
        let report = json!({
            "backend": "hvf", "arch": "arm64", "timebase": "pv", "format": 1,
            "seed": 1, "kernel_sha256": "k", "initrd_sha256": "i", "witness": "same",
        });
        fs::write(&left, serde_json::to_vec(&report).unwrap()).unwrap();
        Self {
            directory,
            left,
            right,
            report,
        }
    }

    fn compare(&self, interpreter: &OsStr) -> io::Result<Output> {
        run(Command::new(interpreter)
            .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("../seedbox-arm64/compare.py"))
            .arg(&self.left)
            .arg(&self.right))
    }
}

#[test]
fn backend_comparability() {
    let reports = Reports::new();
    // Original case order: equal backend, different backend, absent backend.
    for (backend, status, stdout, stderr) in [
        (Some("hvf"), 0, "MATCH: same\n", ""),
        (Some("kvm"), 1, "", "NOT_COMPARABLE: backend: hvf != kvm\n"),
        (
            None,
            1,
            "",
            "NOT_COMPARABLE: backend missing; regenerate reports with backend metadata\n",
        ),
    ] {
        let mut other = reports.report.clone();
        match backend {
            Some(backend) => other["backend"] = json!(backend),
            None => {
                other.as_object_mut().unwrap().remove("backend");
            }
        }
        fs::write(&reports.right, serde_json::to_vec(&other).unwrap()).unwrap();
        let output = reports.compare(OsStr::new("python3")).unwrap();
        assert_eq!(
            output.status.code(),
            Some(status),
            "{backend:?}: {output:?}"
        );
        // Exact streams strengthen the original combined-stream substring check,
        // retaining empty stdout for every non-comparable backend.
        assert_eq!(output.stdout, stdout.as_bytes(), "{backend:?}: {output:?}");
        assert_eq!(output.stderr, stderr.as_bytes(), "{backend:?}: {output:?}");
    }
    let path = reports.directory.path().to_path_buf();
    drop(reports);
    assert!(!path.exists());
}

#[test]
fn interpreter_invocation_failure_is_not_a_comparison_result() {
    let reports = Reports::new();
    fs::write(&reports.right, serde_json::to_vec(&reports.report).unwrap()).unwrap();
    let absent_interpreter = reports.directory.path().join("missing-python");
    let error = reports.compare(absent_interpreter.as_os_str()).unwrap_err();
    // Spawn failure has no comparator exit status or streams; it cannot become
    // an expected NOT_COMPARABLE exit 1 (or a successful witness match).
    assert_eq!(error.kind(), io::ErrorKind::NotFound);
    assert_eq!(
        fs::read(&reports.left).unwrap(),
        fs::read(&reports.right).unwrap()
    );
}
