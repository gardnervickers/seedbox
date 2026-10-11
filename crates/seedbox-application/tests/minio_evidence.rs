//! Independent Rust receipts test the example's retained-evidence checker.
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};
fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap()
}
fn http(
    operation: &str,
    key: Value,
    status: u16,
    condition: Value,
    etag: Value,
    value: Value,
) -> (String, Value) {
    (
        "MINIO_HTTP".into(),
        json!({"operation":operation,"key":key,"status":status,"condition":condition,"etag":etag,"value_hex":value}),
    )
}
fn put(key: &str, status: u16, condition: Value, etag: Value, value: &str) -> (String, Value) {
    http(
        "put_object",
        json!(key),
        status,
        condition,
        etag,
        json!(value),
    )
}
fn get(key: &str, status: u16) -> (String, Value) {
    http(
        "get_object",
        json!(key),
        status,
        json!({}),
        Value::Null,
        Value::Null,
    )
}
fn value(key: &str, etag: &str, value: &str) -> (String, Value) {
    (
        "MINIO_GET".into(),
        json!({"key":key,"etag":etag,"value_hex":value}),
    )
}
fn receipts(mode: &str) -> Vec<(String, Value)> {
    let a = hex::encode(b"acknowledged-value-A");
    let b = hex::encode(b"conflicting-value-B");
    let mut lines = vec![
        http(
            "list_buckets",
            Value::Null,
            200,
            json!({}),
            Value::Null,
            Value::Null,
        ),
        http(
            "create_bucket",
            Value::Null,
            200,
            json!({}),
            Value::Null,
            Value::Null,
        ),
    ];
    if mode == "qualification" {
        lines.extend([
            put(
                "matrix",
                200,
                json!({"IfNoneMatch":"*"}),
                json!("opaque-a"),
                &a,
            ),
            get("matrix", 200),
            value("matrix", "opaque-a", &a),
            put("matrix", 412, json!({"IfNoneMatch":"*"}), Value::Null, &b),
            get("matrix", 200),
            value("matrix", "opaque-a", &a),
            put(
                "matrix",
                200,
                json!({"IfMatch":"opaque-a"}),
                json!("opaque-b"),
                &b,
            ),
            get("matrix", 200),
            value("matrix", "opaque-b", &b),
            put(
                "matrix",
                412,
                json!({"IfMatch":"opaque-a"}),
                Value::Null,
                &a,
            ),
            get("matrix", 200),
            value("matrix", "opaque-b", &b),
            put(
                "missing",
                404,
                json!({"IfMatch":"opaque-a"}),
                Value::Null,
                &a,
            ),
            get("missing", 404),
            put(
                "concurrent",
                200,
                json!({"IfNoneMatch":"*"}),
                json!("opaque-w"),
                &a,
            ),
            put(
                "concurrent",
                412,
                json!({"IfNoneMatch":"*"}),
                Value::Null,
                &b,
            ),
            get("concurrent", 200),
            value("concurrent", "opaque-w", &a),
            ("MINIO_RESULT".into(), json!({"mode":mode,"passed":true})),
        ]);
    } else {
        lines.extend([
            put(
                "slot",
                200,
                json!({"IfNoneMatch":"*"}),
                json!("opaque-a"),
                &a,
            ),
            put("slot", 412, json!({"IfNoneMatch":"*"}), Value::Null, &b),
        ]);
        if mode == "broken" {
            lines.push(put("slot", 200, json!({}), json!("opaque-b"), &b));
        }
        lines.extend([get("slot",200),value("slot",if mode=="broken"{"opaque-b"}else{"opaque-a"},if mode=="broken"{&b}else{&a}),("MINIO_RESULT".into(),json!({"mode":mode,"passed":mode!="broken","acknowledged_hex":a,"stored_hex":if mode=="broken"{&b}else{&a}}))]);
    }
    lines
}
fn console(lines: &[(String, Value)]) -> String {
    lines
        .iter()
        .map(|(kind, value)| format!("{kind} {value}\n"))
        .collect()
}
fn alter(lines: &[(String, Value)], kind: &str, index: usize, key: &str, value: Value) -> String {
    let mut lines = lines.to_vec();
    let row = lines
        .iter_mut()
        .filter(|(label, _)| label == kind)
        .nth(index)
        .unwrap();
    row.1[key] = value;
    console(&lines)
}
fn check(text: &str, mode: &str) -> Output {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("console");
    fs::write(&path, text).unwrap();
    Command::new("python3").args(["-c","import sys; from pathlib import Path; sys.path.insert(0,sys.argv[1]); import verify; verify.check_console(Path(sys.argv[2]).read_text(),sys.argv[3])"]).arg(root().join("examples/minio")).arg(path).arg(mode).output().unwrap()
}
#[test]
fn checker_requires_one_correct_verdict_and_bounded_readiness_evidence() {
    for mode in ["qualification", "broken", "fixed"] {
        let lines = receipts(mode);
        let text = console(&lines);
        let output = check(&text, mode);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        for bad in [
            text[..text.find("MINIO_RESULT").unwrap()].to_owned(),
            text.clone() + &text,
            alter(&lines, "MINIO_RESULT", 0, "passed", json!(mode == "broken")),
            "qualification/setup error: timeout\n".to_owned() + &text,
            "infrastructure error: timeout\n".to_owned() + &text,
        ] {
            assert!(!check(&bad, mode).status.success());
        }
    }
    let lines = receipts("fixed");
    let text = console(&lines);
    let mut transient = lines[0].clone();
    transient.1["status"] = 503.into();
    let transient = console(&[transient]);
    assert!(check(&(transient.repeat(119) + &text), "fixed")
        .status
        .success());
    assert!(!check(&(transient.repeat(120) + &text), "fixed")
        .status
        .success());
    assert!(!check(
        &alter(&lines, "MINIO_HTTP", 0, "status", json!(403)),
        "fixed"
    )
    .status
    .success());
}
#[test]
fn checker_rejects_wrong_conditions_statuses_etags_bytes_and_concurrent_winners() {
    let lines = receipts("qualification");
    for (kind, index, key, value) in [
        ("MINIO_HTTP", 2, "condition", json!({})),
        ("MINIO_HTTP", 4, "status", json!(200)),
        ("MINIO_HTTP", 6, "condition", json!({"IfMatch":"invented"})),
        ("MINIO_HTTP", 10, "status", json!(412)),
        ("MINIO_HTTP", 11, "status", json!(200)),
        (
            "MINIO_GET",
            1,
            "value_hex",
            json!(hex::encode(b"conflicting-value-B")),
        ),
        ("MINIO_GET", 2, "etag", json!("invented")),
        (
            "MINIO_GET",
            4,
            "value_hex",
            json!(hex::encode(b"conflicting-value-B")),
        ),
    ] {
        assert!(
            !check(&alter(&lines, kind, index, key, value), "qualification")
                .status
                .success(),
            "accepted {kind} {index} {key}"
        );
    }
    let lines = receipts("broken");
    for (kind, index, key, value) in [
        ("MINIO_HTTP", 4, "condition", json!({"IfNoneMatch":"*"})),
        (
            "MINIO_GET",
            0,
            "value_hex",
            json!(hex::encode(b"acknowledged-value-A")),
        ),
        (
            "MINIO_RESULT",
            0,
            "acknowledged_hex",
            json!(hex::encode(b"conflicting-value-B")),
        ),
        ("MINIO_HTTP", 2, "status", json!(412)),
    ] {
        assert!(!check(&alter(&lines, kind, index, key, value), "broken")
            .status
            .success());
    }
}
#[test]
fn retained_record_requires_completed_oracle_exit_memory_placement_and_matching_replay() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path();
    fs::create_dir_all(path.join("output/machines")).unwrap();
    fs::write(
        path.join("output/machines/client.console"),
        console(&receipts("broken")),
    )
    .unwrap();
    let base = json!({"outcome":"FAIL","error":null,"omitted_logs":{},"report":{"reason":"COMPLETED","witness":"synthetic-witness","workload_exit":{"service":"client","code":1}},"identity":{"inputs":{"profile":{"semantic_parameters":{"memory_mb":"2048"}},"placement":{"client":"client","minio":"minio"}}},"replay_of":"original","replay":{"status":"MATCH","outcome_matches":true,"witness_matches":true,"fault_history_matches":true,"full_state_matches":null}});
    let world = json!({"machines":[{"name":"minio"},{"name":"client"}],"faults":[]});
    let verify = |record: &Value, world: &Value| {
        fs::write(
            path.join("manifest.json"),
            json!({"record":record}).to_string(),
        )
        .unwrap();
        fs::write(path.join("output/vmm.json"), world.to_string()).unwrap();
        Command::new("python3")
            .arg(root().join("examples/minio/verify.py"))
            .arg(path)
            .args(["--mode", "broken", "--replay"])
            .output()
            .unwrap()
    };
    assert!(verify(&base, &world).status.success());
    for (pointer, value) in [
        ("/outcome", json!("ERROR")),
        ("/report/workload_exit/code", json!(2)),
        ("/report/reason", json!("WALL_TIME_LIMIT")),
        (
            "/identity/inputs/profile/semantic_parameters/memory_mb",
            json!("512"),
        ),
        ("/identity/inputs/placement/client", json!("minio")),
        ("/omitted_logs", json!({"client.console":"cap"})),
        ("/replay", Value::Null),
        ("/replay/status", json!("MISMATCH")),
        ("/replay/witness_matches", json!(false)),
        ("/replay/outcome_matches", json!(false)),
        ("/replay/fault_history_matches", json!(false)),
    ] {
        let mut bad = base.clone();
        *bad.pointer_mut(pointer).unwrap() = value;
        assert!(!verify(&bad, &world).status.success(), "accepted {pointer}");
    }
    let mut bad_world = world;
    bad_world["faults"] = json!([{"action":"partition"}]);
    assert!(!verify(&base, &bad_world).status.success());
}
#[test]
fn staging_and_generation_verify_all_bytes_before_mutation_and_refuse_overwrites() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path();
    let fixture = path.join("example");
    fs::create_dir_all(fixture.join("minio")).unwrap();
    fs::create_dir(fixture.join("client")).unwrap();
    let source = root().join("examples/minio");
    for name in [
        "generate.py",
        "stage_inputs.py",
        "compose.yaml",
        "compose.broken.yaml",
        "compose.fixed.yaml",
    ] {
        fs::copy(source.join(name), fixture.join(name)).unwrap();
    }
    for context in ["minio", "client"] {
        fs::copy(
            source.join(context).join("Dockerfile"),
            fixture.join(context).join("Dockerfile"),
        )
        .unwrap();
    }
    let binary = path.join("binary");
    let wheels = path.join("wheels");
    fs::create_dir(&wheels).unwrap();
    let bytes = b"tiny-test-input";
    fs::write(&binary, bytes).unwrap();
    fs::write(wheels.join("fixture.whl"), bytes).unwrap();
    let pin = json!({"size":bytes.len(),"sha256":hex::encode(Sha256::digest(bytes))});
    let mut wheel_pin = pin.clone();
    wheel_pin["filename"] = "fixture.whl".into();
    fs::write(
        fixture.join("inputs.lock.json"),
        json!({"minio":pin,"wheels":[wheel_pin]}).to_string(),
    )
    .unwrap();
    let projects = path.join("projects");
    let generate = || {
        Command::new("python3")
            .arg(fixture.join("generate.py"))
            .arg(&projects)
            .arg("--minio")
            .arg(&binary)
            .arg("--wheel-directory")
            .arg(&wheels)
            .output()
            .unwrap()
    };
    let output = generate();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!generate().status.success());
    let stage = |destination: &Path| {
        Command::new("python3").args(["-c","import sys; from pathlib import Path; sys.path.insert(0,sys.argv[1]); import stage_inputs; stage_inputs.stage(Path(sys.argv[2]),Path(sys.argv[3]),Path(sys.argv[4]))"]).arg(&fixture).arg(destination).arg(&binary).arg(&wheels).output().unwrap()
    };
    for mode in ["qualification", "broken", "fixed"] {
        let project = projects.join(mode);
        assert_eq!(fs::read(project.join("minio/minio")).unwrap(), bytes);
        let compose: Value =
            serde_json::from_slice(&fs::read(project.join("compose.yaml")).unwrap()).unwrap();
        assert_eq!(compose["services"]["client"]["command"][0], mode);
        assert!(!stage(&project).status.success());
    }
    fs::write(&binary, b"evil-test-input").unwrap();
    let rejected = path.join("rejected");
    let output = stage(&rejected);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("hash mismatch"));
    assert!(!rejected.exists());
}
