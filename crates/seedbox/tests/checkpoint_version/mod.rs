//! A real retained-worker launch rejects an old exported envelope BEFORE it
//! can open assets, construct a native factory or boot any guest.
use super::*;
use serde_json::json;
#[test]
fn older_checkpoint_refusal_is_actionable_cli_output_before_native_restore() {
    let root = tempfile::tempdir().unwrap();
    let checkpoint = root.path().join("old-checkpoint.json");
    let old = r#"{"contract":"driver-world-v1","version":2}"#;
    fs::write(&checkpoint, old).unwrap();
    let missing = root.path().join("must-not-open-native-input");
    let artifact = json!({"artifact":missing,"sha256":"00".repeat(32),"length":1});
    let inventory = json!({
        "backend":"tcg", "policy":null, "executable":artifact,
        "kernel":artifact, "init":artifact, "command_line":artifact,
        "firmware":artifact, "runtime":artifact, "formatter":null,
        "environments":{}, "boot_archives":{}, "epoch_seconds":0,
    });
    let request = json!({
        "prepared":{
            "inventory":inventory,
            "oracle":{"name":"oracle", "memory_bytes":4096,
                "filesystem":artifact, "process":{"argv":["/test"],"environment":{},"working_directory":"/","user":"0:0"},
                "disk":null,"interface":null},
            "machines":[],"spawnable":[],"quantum_ns":10,
            "network":{"latency_ns":10,"jitter_ns":0,"loss_ppm":0}
        },
        "seed":1,"program":[],"simulated_time_ns":10,
        "output":root.path().join("output"),"machine_workers":1,
        "start_paused":false,"full_state":false,
        "host_control":root.path().join("must-not-bind-control.sock"),
        "observations":null,"replay_interventions":null,"resume_checkpoint":checkpoint,
    });
    let path = root.path().join("request.json");
    fs::write(&path, serde_json::to_vec(&request).unwrap()).unwrap();
    let result = seedbox(
        &[
            "__engine",
            "driver-world-v1",
            "--request",
            path.to_str().unwrap(),
        ],
        root.path(),
    );
    assert_eq!(result.status.code(), Some(2));
    assert!(result.stdout.is_empty());
    let error =
        serde_json::from_str::<seedbox_engine_protocol::driver_world::Checkpoint>(old).unwrap_err();
    let expected = format!("error: {error}\n");
    assert!(expected.contains("checkpoint was made by an older or incompatible engine; replay with its retained engine or run fresh"));
    assert_eq!(stderr(&result), expected);
    assert!(!missing.exists());
    assert!(!root.path().join("must-not-bind-control.sock").exists());
}
