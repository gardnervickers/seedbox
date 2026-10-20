//! Rust-owned portable controls against the unchanged MinIO Python programs.
//! Nix provisions the guest's exact wheel lock; synthetic transports forbid sockets.
use seedbox_test_time::HangGuard;
use serde_json::{json, Value};
use std::{
    fs,
    path::Path,
    process::{Child, Command, Stdio},
    time::Duration,
};

const A: &str = "61636b6e6f776c65646765642d76616c75652d41";
const B: &str = "636f6e666c696374696e672d76616c75652d42";
// Infrastructure hang guard only, not a MinIO case deadline. Readiness uses
// the adapter's controlled simulated clock; rendezvous failure uses observed
// blocked executor slots/queued work. SDK import/client construction can be
// descheduled under parallel portable builds, without changing either result.
const ADAPTER_HANG_GUARD: Duration = Duration::from_secs(120);
const STARTUP: [&str; 4] = [
    "EndpointConnectionError",
    "ConnectionClosedError",
    "ConnectTimeoutError",
    "ReadTimeoutError",
];
struct OwnedChild(Child);
impl Drop for OwnedChild {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
fn call(mut request: Value) -> Value {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let pythonpath = std::env::var_os("SEEDBOX_MINIO_PYTHONPATH")
        .expect("MinIO controls need pinned SDK fixture; run inside nix develop");
    let dir = tempfile::tempdir().unwrap();
    request
        .as_object_mut()
        .unwrap()
        .entry("ledger")
        .or_insert(json!(dir.path().join("ledger")));
    let input = dir.path().join("request");
    let output = dir.path().join("observation");
    fs::write(&input, request.to_string()).unwrap();
    let mut child = OwnedChild(
        Command::new("python3")
            .args(["-B", "-u"])
            .arg(root.join("crates/seedbox-test/tests/support/minio.py"))
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("PYTHONPATH", pythonpath)
            .stdin(fs::File::open(input).unwrap())
            .stdout(fs::File::create(&output).unwrap())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap(),
    );
    let guard = HangGuard::new(
        "MinIO adapter process exit (not a case deadline)",
        ADAPTER_HANG_GUARD,
    );
    let status = guard.wait_for(|| child.0.try_wait().transpose())
        .unwrap_or_else(|error| panic!("MinIO adapter infrastructure hang guard expired (not a case deadline): {request}: {error}"))
        .unwrap();
    assert!(status.success(), "MinIO adapter {status}: {request}");
    assert!(
        fs::metadata(&output).unwrap().len() < 8 * 1024 * 1024,
        "bounded MinIO observation"
    );
    serde_json::from_slice(&fs::read(output).unwrap()).unwrap()
}
fn ok(out: &Value) {
    assert!(out.get("error").is_none(), "{out}");
}
fn error(out: &Value, kind: &str, message: &str) {
    assert_eq!(out["error"]["type"], kind, "{out}");
    assert!(
        out["error"]["message"].as_str().unwrap().contains(message),
        "{out}"
    );
}
fn startup() -> Vec<Value> {
    STARTUP
        .into_iter()
        .map(|s| json!(s))
        .chain([json!(500), json!(503)])
        .collect()
}
fn console(mode: &str) -> String {
    let out = call(json!({"action":"console","mode":mode}));
    ok(&out);
    out["stdout"].as_str().unwrap().into()
}
fn check(text: &str, mode: &str) -> Value {
    call(json!({"action":"check_console","text":text,"mode":mode}))
}
fn alter(text: &str, kind: &str, index: usize, fields: Value) -> String {
    let mut seen = 0;
    text.lines()
        .map(|line| {
            if let Some(body) = line.strip_prefix(&format!("{kind} ")) {
                let this = seen;
                seen += 1;
                if this == index {
                    let mut receipt: Value = serde_json::from_str(body).unwrap();
                    receipt
                        .as_object_mut()
                        .unwrap()
                        .extend(fields.as_object().unwrap().clone());
                    return format!("{kind} {receipt}");
                }
            }
            line.into()
        })
        .collect::<Vec<_>>()
        .join("\n")
}
#[test]
fn pinned_sdk_translates_startup_socket_errors() {
    for (underlying, expected) in [
        ("NewConnectionError", "EndpointConnectionError"),
        ("ProtocolError", "ConnectionClosedError"),
        ("ConnectTimeoutError", "ConnectTimeoutError"),
        ("ReadTimeoutError", "ReadTimeoutError"),
    ] {
        let out = call(json!({"action":"translation","underlying":underlying}));
        error(&out, expected, "");
        assert_eq!(out["urlopen_calls"], 1);
    }
}
#[test]
fn readiness_retries_all_startup_errors_until_slow_success() {
    for outcome in startup() {
        let out = call(json!({"action":"ready","outcome":outcome,"ready_at":29.75}));
        ok(&out);
        assert_eq!(out["now"], 29.75);
        assert_eq!(out["transport_calls"], 120);
        assert_eq!(out["sleeps"], json!(vec![0.25; 119]));
    }
}
#[test]
fn readiness_exhaustion_is_infrastructure_error_without_workload() {
    for outcome in startup() {
        let out = call(json!({"action":"main","outcome":outcome}));
        ok(&out);
        assert_eq!(out["value"], 2);
        assert_eq!(out["now"], 30.0);
        assert_eq!(out["transport_calls"], 120);
        let stderr = out["stderr"].as_str().unwrap();
        assert!(stderr
            .starts_with("infrastructure error: MinIO not ready within 30 s of simulated time;"));
        let detail = outcome
            .as_str()
            .map(str::to_owned)
            .unwrap_or_else(|| format!("HTTP {outcome}"));
        assert!(stderr.contains(&detail));
        assert!(!out["stdout"].as_str().unwrap().contains("MINIO_RESULT"));
        assert_eq!(out["create_calls"], 0);
        assert_eq!(out["workload_calls"], 0);
        assert!(out["ledger"].is_null());
    }
}
#[test]
fn readiness_counts_request_time_and_truncates_final_sleep() {
    let out = call(json!({"action":"ready","outcome":503,"request_seconds":1.9}));
    error(
        &out,
        "InfrastructureError",
        "last readiness error: HTTP 503",
    );
    assert!((out["now"].as_f64().unwrap() - 30.0).abs() < 1e-7);
    assert_eq!(out["transport_calls"], 14);
    assert!(
        (out["sleeps"]
            .as_array()
            .unwrap()
            .last()
            .unwrap()
            .as_f64()
            .unwrap()
            - 0.15)
            .abs()
            < 1e-7
    );
}
#[test]
fn readiness_cannot_succeed_after_deadline() {
    let out = call(json!({"action":"ready","outcome":503,"ready_at":30,"request_seconds":2}));
    error(
        &out,
        "InfrastructureError",
        "last readiness error: HTTP 200",
    );
    assert_eq!(out["transport_calls"], 14);
    assert_eq!(out["now"], 31.25);
}
#[test]
fn readiness_rejects_non_startup_errors_immediately() {
    for (outcome, kind, message) in [
        (
            json!(403),
            "QualificationError",
            "readiness returned HTTP 403",
        ),
        (
            json!("ValueError"),
            "ValueError",
            "unexpected transport bug",
        ),
    ] {
        let out = call(json!({"action":"ready","outcome":outcome}));
        error(&out, kind, message);
        assert_eq!(out["transport_calls"], 1);
        assert_eq!(out["sleeps"], json!([]));
    }
}
#[test]
fn put_ambiguity_never_acknowledges() {
    let out = call(json!({"action":"planted","mode":"broken","ambiguous":true}));
    error(&out, "ReadTimeoutError", "");
    assert_eq!(out["put_calls"], 1);
    assert!(out["ledger"].is_null());
}
#[test]
fn public_headers_and_current_sdk_pins() {
    let out = call(json!({"action":"headers"}));
    ok(&out);
    assert_eq!(out["versions"], json!(["1.40.45", "1.40.45"]));
    assert_eq!(out["statuses"], json!([200, 200]));
    assert_eq!(out["calls"][0][2], json!({"if-none-match":"*"}));
    assert_eq!(out["calls"][1][2], json!({"if-match":out["etag"]}));
    assert_eq!(out["connect_timeout"], 1);
    assert_eq!(out["read_timeout"], 2);
    assert_eq!(out["attempts"], 1);
}
#[test]
fn http_503_and_read_timeout_are_not_retried() {
    for timeout in [false, true] {
        let out = call(json!({"action":"put_failure","timeout":timeout}));
        error(
            &out,
            if timeout {
                "ReadTimeoutError"
            } else {
                "ClientError"
            },
            "",
        );
        assert_eq!(out["transport_calls"], 1);
    }
}
#[test]
fn fixed_preserves_ack_broken_is_detected() {
    for (mode, expected, writes) in [("fixed", 0, 2), ("broken", 1, 3)] {
        let out = call(json!({"action":"planted","mode":mode}));
        ok(&out);
        assert_eq!(out["value"], expected);
        assert_eq!(out["ledger"], A);
        let puts: Vec<_> = out["calls"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|c| c[0] == "PUT")
            .collect();
        assert_eq!(puts.len(), writes);
        if mode == "broken" {
            assert_eq!(puts.last().unwrap()[2], json!({}));
        } else {
            assert!(puts.iter().all(|p| p[2] == json!({"if-none-match":"*"})));
        }
    }
}
#[test]
fn no_ack_on_first_conflict() {
    let out = call(json!({"action":"planted","mode":"fixed","store":{"first_put_status":412}}));
    error(&out, "QualificationError", "");
    assert!(out["ledger"].is_null());
}
#[test]
fn qualification_checker_and_false_conflict_negative_control() {
    let out = call(json!({"action":"qualify"}));
    ok(&out);
    assert_eq!(out["value"], 0);
    error(
        &call(json!({"action":"qualify","store":{"overwrite_on_conflict":true}})),
        "QualificationError",
        "unexpected stored bytes",
    );
}
#[test]
fn unqualified_missing_match_semantics_are_refused() {
    error(
        &call(json!({"action":"qualify","store":{"missing_match_status":412}})),
        "QualificationError",
        "expected HTTP 404",
    );
}
#[test]
fn condition_header_omission_is_detectable() {
    error(
        &call(json!({"action":"qualify","omit_condition":true})),
        "QualificationError",
        "expected HTTP 412",
    );
}
#[test]
fn wrong_concurrent_winner_value_is_detectable() {
    error(
        &call(json!({"action":"qualify","wrong_winner":true})),
        "QualificationError",
        "unexpected stored bytes",
    );
}
#[test]
fn both_constructed_clients_rendezvous_before_transport_dispatch() {
    let out = call(json!({"action":"concurrent"}));
    ok(&out);
    let mut statuses: Vec<_> = out["value"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v[0].as_i64().unwrap())
        .collect();
    statuses.sort();
    assert_eq!(statuses, [200, 412]);
    assert_eq!(out["arrivals"], 2);
    assert_eq!(out["dispatch"], json!([[2, 2, true], [2, 2, true]]));
    assert_eq!(out["rendezvous_timeouts"], json!([2]));
    assert_eq!(out["rendezvous_stalls"], json!([]));
    assert_eq!(out["rendezvous_departures"], 2);
    let out = call(json!({"action":"concurrent","omit_barrier":true}));
    error(&out, "QualificationError", "rendezvous omitted");
    assert_eq!(out["calls"], json!([]));
}
#[test]
fn sequentialized_executor_fails_bounded_rendezvous_without_puts() {
    let out = call(json!({"action":"concurrent","sequential":true}));
    error(&out, "QualificationError", "start rendezvous failed");
    assert_eq!(out["calls"], json!([]));
    // The actual one-worker executor cannot start its queued second contender
    // while its sole running contender waits at the barrier. No clock advances.
    assert_eq!(out["rendezvous_timeouts"], json!([2]));
    assert_eq!(out["rendezvous_departures"], 0);
    assert_eq!(
        out["rendezvous_stalls"],
        json!([{
            "submitted":2,"running":1,"waiting":1,"queued":1,"worker_slots":1
        }])
    );
}
#[test]
fn unscheduled_second_client_is_not_a_rendezvous_failure() {
    // Hold the second client's setup until the first is already waiting.
    // A free worker slot is runnable progress, regardless of host scheduling.
    let out = call(json!({"action":"concurrent","stagger_setup":true}));
    ok(&out);
    assert_eq!(out["setup_waited_for_peer"], true);
    assert_eq!(out["arrivals"], 2);
    assert_eq!(out["dispatch"], json!([[2, 2, true], [2, 2, true]]));
    assert_eq!(out["rendezvous_timeouts"], json!([2]));
    assert_eq!(out["rendezvous_stalls"], json!([]));
    assert_eq!(out["rendezvous_departures"], 2);
    let mut statuses: Vec<_> = out["value"]
        .as_array()
        .unwrap()
        .iter()
        .map(|value| value[0].as_i64().unwrap())
        .collect();
    statuses.sort();
    assert_eq!(statuses, [200, 412]);
}
#[test]
fn concurrent_client_setup_failure_is_qualification_error() {
    error(
        &call(json!({"action":"concurrent","setup_failure":true})),
        "QualificationError",
        "client setup failed",
    );
}
#[test]
fn finite_transient_readiness_receipts() {
    let text = console("fixed");
    let transient = alter(
        text.lines().next().unwrap(),
        "MINIO_HTTP",
        0,
        json!({"status":503}),
    ) + "\n";
    for n in [1, 119] {
        ok(&check(&(transient.repeat(n) + &text), "fixed"));
    }
    error(
        &check(&(transient.repeat(120) + &text), "fixed"),
        "EvidenceError",
        "",
    );
    error(
        &check(
            &alter(&text, "MINIO_HTTP", 0, json!({"status":403})),
            "fixed",
        ),
        "EvidenceError",
        "",
    );
}
#[test]
fn real_client_receipts_and_missing_duplicate_or_wrong_verdict() {
    for mode in ["qualification", "broken", "fixed"] {
        let text = console(mode);
        ok(&check(&text, mode));
        for bad in [
            text[..text.find("MINIO_RESULT").unwrap()].into(),
            format!("{text}{text}"),
            alter(&text, "MINIO_RESULT", 0, json!({"passed":mode=="broken"})),
            format!("qualification/setup error: timeout\n{text}"),
            format!("infrastructure error: MinIO not ready within 30 s of simulated time\n{text}"),
        ] {
            error(&check(&bad, mode), "EvidenceError", "");
        }
    }
}
#[test]
fn cas_matrix_negatives() {
    let text = console("qualification");
    for (kind, index, changes) in [
        ("MINIO_HTTP", 2, json!({"condition":{}})),
        ("MINIO_HTTP", 4, json!({"status":200})),
        ("MINIO_HTTP", 6, json!({"condition":{"IfMatch":"invented"}})),
        ("MINIO_HTTP", 10, json!({"status":412})),
        ("MINIO_HTTP", 11, json!({"status":200})),
        ("MINIO_GET", 1, json!({"value_hex":B})),
        ("MINIO_GET", 2, json!({"etag":"invented"})),
    ] {
        error(
            &check(&alter(&text, kind, index, changes), "qualification"),
            "EvidenceError",
            "",
        );
    }
    let winner: Value = serde_json::from_str(
        text.lines()
            .filter_map(|l| l.strip_prefix("MINIO_GET "))
            .next_back()
            .unwrap(),
    )
    .unwrap();
    let loser = if winner["value_hex"] == A { B } else { A };
    error(
        &check(
            &alter(&text, "MINIO_GET", 4, json!({"value_hex":loser})),
            "qualification",
        ),
        "EvidenceError",
        "",
    );
}
#[test]
fn planted_negatives() {
    let text = console("broken");
    for (kind, index, changes) in [
        ("MINIO_HTTP", 4, json!({"condition":{"IfNoneMatch":"*"}})),
        ("MINIO_GET", 0, json!({"value_hex":A})),
        ("MINIO_RESULT", 0, json!({"acknowledged_hex":B})),
        ("MINIO_HTTP", 2, json!({"status":412})),
    ] {
        error(
            &check(&alter(&text, kind, index, changes), "broken"),
            "EvidenceError",
            "",
        );
    }
}
#[test]
fn retained_outcome_exit_memory_and_fault_negatives() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::create_dir_all(root.join("output/machines")).unwrap();
    fs::write(
        root.join("output/machines/client.console"),
        console("broken"),
    )
    .unwrap();
    let base = json!({"outcome":"FAIL","error":null,"omitted_logs":{},
        "report":{"reason":"COMPLETED","witness":"synthetic-witness","workload_exit":{"service":"client","code":1}},
        "identity":{"inputs":{"profile":{"semantic_parameters":{"memory_mb":"2048"}},"placement":{"client":"client","minio":"minio"}}}});
    let world = json!({"machines":[{"name":"minio"},{"name":"client"}],"faults":[]});
    fs::write(
        root.join("manifest.json"),
        json!({"record":base}).to_string(),
    )
    .unwrap();
    fs::write(root.join("output/vmm.json"), world.to_string()).unwrap();
    let verify = || call(json!({"action":"verify","root":root,"mode":"broken"}));
    ok(&verify());
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
    ] {
        let mut record = base.clone();
        *record.pointer_mut(pointer).unwrap() = value;
        fs::write(
            root.join("manifest.json"),
            json!({"record":record}).to_string(),
        )
        .unwrap();
        error(&verify(), "EvidenceError", "");
    }
    fs::write(
        root.join("manifest.json"),
        json!({"record":base}).to_string(),
    )
    .unwrap();
    let mut world = world;
    world["faults"] = json!([{"action":"partition"}]);
    fs::write(root.join("output/vmm.json"), world.to_string()).unwrap();
    error(&verify(), "EvidenceError", "");
}
#[test]
fn staging_and_generation_refuse_corruption_and_overwrite() {
    use sha2::{Digest, Sha256};
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let binary = root.join("binary");
    let wheels = root.join("wheels");
    fs::create_dir(&wheels).unwrap();
    let bytes = b"tiny-test-input";
    fs::write(&binary, bytes).unwrap();
    fs::write(wheels.join("test.whl"), bytes).unwrap();
    let pin = json!({"size":bytes.len(),"sha256":hex::encode(Sha256::digest(bytes))});
    let mut wheel_pin = pin.clone();
    wheel_pin["filename"] = json!("test.whl");
    let pins = json!({"minio":pin,"wheels":[wheel_pin]});
    let generated = root.join("projects");
    let invoke = |action: &str, target: &Path| {
        call(json!({"action":action,"root":target,"binary":binary,"wheels":wheels,"pins":pins}))
    };
    let out = invoke("generate", &generated);
    ok(&out);
    assert_eq!(out["value"], json!(generated));
    for mode in ["qualification", "broken", "fixed"] {
        let project = generated.join(mode);
        assert_eq!(fs::read(project.join("minio/minio")).unwrap(), bytes);
        let compose: Value =
            serde_json::from_slice(&fs::read(project.join("compose.yaml")).unwrap()).unwrap();
        assert_eq!(compose["services"]["client"]["command"][0], mode);
        error(&invoke("stage", &project), "FileExistsError", "");
    }
    error(&invoke("generate", &generated), "FileExistsError", "");
    fs::write(&binary, b"evil-test-input").unwrap();
    let rejected = root.join("rejected");
    error(&invoke("stage", &rejected), "ValueError", "hash mismatch");
    assert!(!rejected.exists());
}
