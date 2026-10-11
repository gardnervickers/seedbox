//! Disposable process accounting exercises the actual portable supervisor.
mod support;
use serde_json::{json, Value};
use std::{
    fs,
    path::Path,
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

const EXECUTE: &str = r#"
import json,sys
from pathlib import Path
from service_process import execute
root=Path(sys.argv[1])
budget=json.loads(sys.argv[2])
try:
    execute(json.loads(sys.argv[3]),root/'attempt',root/'budget.json',budget,1,float(sys.argv[4]))
except Exception as error:
    print(type(error).__name__+': '+str(error),file=sys.stderr)
    sys.exit(2)
"#;
struct Owned(Child);
impl Drop for Owned {
    fn drop(&mut self) {
        if self.0.try_wait().unwrap().is_none() {
            self.0.kill().unwrap();
            self.0.wait().unwrap();
        }
    }
}
fn start(root: &Path, budget: &Value, command: &Value, timeout: f64) -> Owned {
    Owned(
        Command::new("python3")
            .args(["-c", EXECUTE])
            .arg(root)
            .arg(budget.to_string())
            .arg(command.to_string())
            .arg(timeout.to_string())
            .env("PYTHONPATH", support::root().join("tools/mac-tcg"))
            .env("SEEDBOX_TCG_FIXTURE_DIRECTORY", root)
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
    )
}
fn wait(mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !ready() {
        assert!(Instant::now() < deadline, "supervisor timed out");
        std::thread::sleep(Duration::from_millis(5));
    }
}
fn budget() -> Value {
    json!({"limit_seconds":600,"spent_seconds":0,"runs":[]})
}
fn read(root: &Path) -> Value {
    serde_json::from_slice(&fs::read(root.join("budget.json")).unwrap()).unwrap()
}
#[test]
fn signals_reap_the_child_and_record_interrupted_attempts_with_exact_receipts() {
    for signal in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP] {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        let mut process=start(root,&budget(),&json!(["/bin/sh","-c","printf '%s\\n' \"$$\" > \"$SEEDBOX_TCG_FIXTURE_DIRECTORY/child.pid\"; exec sleep 60"]),30.0);
        wait(|| {
            root.join("child.pid").exists()
                && fs::metadata(root.join("child.pid")).unwrap().len() > 0
        });
        let child = fs::read_to_string(root.join("child.pid"))
            .unwrap()
            .trim()
            .parse::<i32>()
            .unwrap();
        assert_eq!(unsafe { libc::kill(process.0.id() as i32, signal) }, 0);
        let mut status = None;
        wait(|| {
            status = process.0.try_wait().unwrap();
            status.is_some()
        });
        assert!(status.unwrap().success());
        assert_eq!(unsafe { libc::kill(child, 0) }, -1);
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ESRCH)
        );
        let actual = read(root);
        assert!(actual.get("pending").is_none());
        assert!(actual["spent_seconds"].as_f64().unwrap() > 0.0);
        assert_eq!(actual["runs"][0]["process_status"], "INTERRUPTED");
        assert_eq!(actual["runs"][0]["signal"], signal);
        let receipt: Value =
            serde_json::from_slice(&fs::read(root.join("attempt.attempt.json")).unwrap()).unwrap();
        assert_eq!(receipt, actual["runs"][0]);
    }
}
#[test]
fn timeouts_and_spawn_errors_are_accounted_and_pending_attempts_prevent_launch() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    let mut process = start(
        root,
        &budget(),
        &json!(["/bin/sh", "-c", "exec sleep 60"]),
        0.05,
    );
    assert!(process.0.wait().unwrap().success());
    let actual = read(root);
    assert_eq!(actual["runs"][0]["process_status"], "TIMEOUT");
    let mut process = start(root, &actual, &json!([root.join("does-not-exist")]), 1.0);
    assert_eq!(process.0.wait().unwrap().code(), Some(2));
    let mut actual = read(root);
    assert_eq!(actual["runs"].as_array().unwrap().len(), 2);
    assert_eq!(actual["runs"][1]["process_status"], "ERROR");
    actual["pending"] = json!({"path":"hard-crash"});
    let before = fs::read(root.join("budget.json")).unwrap();
    let mut process = start(root, &actual, &json!([]), 1.0);
    assert_eq!(process.0.wait().unwrap().code(), Some(2));
    assert_eq!(fs::read(root.join("budget.json")).unwrap(), before);
}
