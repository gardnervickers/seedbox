//! Exercise the offline comparison executable without a native backend.
use serde_json::json;
use std::{fs, path::Path, process::Command};

#[test]
fn backend_identity_is_checked_before_witness_equality() {
    let directory = tempfile::tempdir().unwrap();
    let report = json!({"backend":"hvf", "arch":"arm64", "timebase":"pv", "format":1, "seed":1, "kernel_sha256":"k", "initrd_sha256":"i", "witness":"same"});
    let left = directory.path().join("left.json");
    let right = directory.path().join("right.json");
    fs::write(&left, serde_json::to_vec(&report).unwrap()).unwrap();
    for (backend, expected) in [
        (Some("hvf"), "MATCH: same"),
        (Some("kvm"), "NOT_COMPARABLE: backend: hvf != kvm"),
        (None, "NOT_COMPARABLE: backend missing"),
    ] {
        let mut other = report.clone();
        if let Some(backend) = backend {
            other["backend"] = backend.into();
        } else {
            other.as_object_mut().unwrap().remove("backend");
        }
        fs::write(&right, serde_json::to_vec(&other).unwrap()).unwrap();
        let output = Command::new("python3")
            .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("compare.py"))
            .arg(&left)
            .arg(&right)
            .output()
            .unwrap();
        assert_eq!(
            output.status.code(),
            Some(if backend == Some("hvf") { 0 } else { 1 })
        );
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(text.contains(expected), "{text}");
        if backend != Some("hvf") {
            assert!(output.stdout.is_empty());
        }
    }
}
