//! Portable negative controls of real Python guest programs, not VM execution.
use seedbox_test_time::HangGuard;
use serde_json::{json, Value};
use std::{
    fs,
    path::Path,
    process::{Child, Command, Stdio},
    time::Duration,
};

struct OwnedChild(Child);
impl Drop for OwnedChild {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
fn exercise(request: Value) -> Value {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("request");
    let output = dir.path().join("observation");
    fs::write(&input, request.to_string()).unwrap();
    let mut child = OwnedChild(
        Command::new("python3")
            .args(["-B", "-u"])
            .arg(root.join("crates/seedbox-test/tests/support/guest_oracles.py"))
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .stdin(fs::File::open(input).unwrap())
            .stdout(fs::File::create(&output).unwrap())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap(),
    );
    let guard = HangGuard::new("guest oracle adapter process exit", Duration::from_secs(30));
    let status = guard
        .wait_for(|| child.0.try_wait().transpose())
        .expect("guest oracle adapter exceeded five seconds")
        .unwrap();
    assert!(status.success(), "guest oracle adapter: {status}");
    let bytes = fs::read(output).unwrap();
    assert!(bytes.len() < 256 * 1024, "bounded guest oracle observation");
    serde_json::from_slice(&bytes).unwrap()
}
fn race(atomic: &str) -> Value {
    exercise(json!({"program":"race", "responses":[
        {"returncode":0,"stdout":"RACE mode=racy counter=7 expected=10 lost=3\n"},
        {"returncode":0,"stdout":atomic}]}))
}
#[test]
fn atomic_control_is_checked_inside_real_race_driver() {
    let good = race("RACE mode=atomic counter=10 expected=10 lost=0\n");
    assert!(good.get("error").is_none(), "{good}");
    assert_eq!(
        good["calls"],
        json!([
        {"argv":["/race","3000000"],"kwargs":{"capture_output":true,"text":true,"check":true}},
        {"argv":["/race","3000000","atomic"],"kwargs":{"capture_output":true,"text":true,"check":true}}])
    );
    assert_eq!(good["stdout"], "RACE mode=racy counter=7 expected=10 lost=3\nRACE mode=atomic counter=10 expected=10 lost=0\n");
    let bad = race("RACE mode=atomic counter=9 expected=10 lost=1\n");
    assert_eq!(bad["error"]["type"], "RuntimeError");
    assert_eq!(
        bad["error"]["message"],
        "invalid counter or atomic control lost updates"
    );
}
#[test]
fn recovers_persisted_acknowledgements() {
    let result = exercise(json!({"program":"volume","broken":false}));
    assert!(result.get("error").is_none(), "{result}");
    assert_eq!(result["value"], 0);
    assert_eq!(result["stdout"], "SERVER_READY generation=1 recovered=0 broken=false\nSERVER_READY generation=2 recovered=20 broken=false\nRESULT pass acknowledged_and_recovered=20\n");
    assert_eq!(
        result["calls"],
        json!([["crash", "server"], ["sleep", 0.5], ["reboot", "server"]])
    );
}
#[test]
fn rejects_acknowledgements_kept_only_in_memory() {
    let result = exercise(json!({"program":"volume","broken":true}));
    assert_eq!(result["error"]["type"], "RuntimeError");
    assert!(
        result["error"]["message"]
            .as_str()
            .unwrap()
            .contains("lost or changed"),
        "{result}"
    );
    assert_eq!(
        result["calls"],
        json!([["crash", "server"], ["sleep", 0.5], ["reboot", "server"]])
    );
}
