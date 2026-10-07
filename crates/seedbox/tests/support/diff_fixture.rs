// Included by both library unit tests and CLI integration tests (`sb` is the crate).
fn fixture() -> sb::orchestration::RunRecord {
    use sb::{config, execution::*, orchestration::*, store::ContentDigest};
    use std::collections::BTreeMap;
    let digest = ContentDigest::of(b"fixture");
    let configuration = config::parse(
        "name: diff\nservices:\n  workload: {image: fixture:v1}\nx-seedbox:\n  version: 1\n  seed: 0\n  limits: {simulated_time: 1s, wall_time: 1m}\n  check: {service: workload}\n",
        std::path::Path::new("/fixture/compose.yaml"), &BTreeMap::new(),
    ).unwrap();
    let identity = RunIdentity {
        version: 2,
        seed: 0,
        configuration,
        inputs: PreparedInputs {
            kernel: digest.clone(),
            images: BTreeMap::new(),
            placement: BTreeMap::from([("workload".into(), "workload".into())]),
            guests: BTreeMap::from([("workload".into(), digest.clone())]),
            profile: ExecutionProfile {
                backend: "seedbox-kvm".into(),
                engine: digest.clone(),
                witness_mode: WitnessMode::Exits,
                semantic_parameters: BTreeMap::from([("memory_mb".into(), "512".into())]),
            },
            auxiliary: BTreeMap::new(),
            provenance: BTreeMap::new(),
        },
    };
    RunRecord {
        run_id: ContentDigest::of(&serde_json::to_vec(&identity).unwrap()),
        identity,
        attempt: "fixture".into(),
        performance_parameters: BTreeMap::new(),
        allow_engine_change: false,
        log_policy: LogPolicy::default(),
        omitted_logs: BTreeMap::new(),
        outcome: Outcome::Pass,
        report: Some(ExecutionReport {
            reason: StopReason::Completed,
            workload_exit: Some(WorkloadExit {
                service: "workload".into(),
                code: 0,
            }),
            witness: Some(digest),
            full_state: None,
            faults: vec![],
            detail: None,
            engine_qualification: None,
        }),
        error: None,
        logs: BTreeMap::new(),
        replay_of: None,
        replay: None,
    }
}

fn raw_report() -> serde_json::Value {
    serde_json::json!({
        "report_version": 1, "seed": 0, "outcome": "end_machine_exited",
        "world_witness": sb::store::ContentDigest::of(b"fixture"),
        "machines": [{"name":"workload", "witness":sb::store::ContentDigest::of(b"machine"),
            "state":"Exited(0)", "time_ns":100, "branches":90, "guest_exit_code":0,
            "guest_init_error":null, "infrastructure_error":false, "run_ms":10}]
    })
}

// The current engine emits textual events alongside hashed state parts.
fn current_report(full_state: bool) -> serde_json::Value {
    let mut raw = raw_report();
    let mut server = raw["machines"][0].clone();
    server["name"] = "server".into();
    server["state"] = "Halted".into();
    server["guest_exit_code"] = serde_json::Value::Null;
    raw["machines"].as_array_mut().unwrap().push(server);
    for machine in raw["machines"].as_array_mut().unwrap() {
        machine["pending_io"] = serde_json::Value::Null;
        machine["state_digest"] = if full_state {
            serde_json::to_value(sb::store::ContentDigest::of(b"full state")).unwrap()
        } else {
            serde_json::Value::Null
        };
        machine["state_digest_parts"] = if full_state {
            let mut parts = serde_json::Map::new();
            for part in ["regs", "xsave", "xcrs", "msrs", "time_page", "ram", "pending_io", "disk"] {
                parts.insert(part.into(), serde_json::to_value(sb::store::ContentDigest::of(part.as_bytes())).unwrap());
            }
            parts.insert("events".into(), "exc:none int:none shadow:0 nmi:0/0/0".into());
            parts.into()
        } else {
            serde_json::Value::Null
        };
    }
    raw
}

fn write_fixture(
    path: &std::path::Path,
    record: &mut sb::orchestration::RunRecord,
    raw: Option<&serde_json::Value>,
    trace: Option<&[u8]>,
) {
    use sb::store::ContentDigest;
    std::fs::create_dir_all(path.join("output/machines")).unwrap();
    for (name, bytes) in [
        ("vmm.json", raw.map(|v| serde_json::to_vec(v).unwrap())),
        ("machines/workload.trace", trace.map(|b| b.to_vec())),
    ] {
        if let Some(bytes) = bytes {
            std::fs::write(path.join("output").join(name), &bytes).unwrap();
            record.logs.insert(name.into(), ContentDigest::of(&bytes));
        }
    }
    record.run_id = ContentDigest::of(&serde_json::to_vec(&record.identity).unwrap());
    let manifest = serde_json::json!({"sha256": ContentDigest::of(&serde_json::to_vec(record).unwrap()), "record":record});
    std::fs::write(
        path.join("manifest.json"),
        serde_json::to_vec(&manifest).unwrap(),
    )
    .unwrap();
}

fn ram_pair() -> (serde_json::Value, serde_json::Value) {
    let a = current_report(true);
    let mut b = a.clone();
    for key in ["state_digest", "state_digest_parts/ram"] {
        *b.pointer_mut(&format!("/machines/1/{key}")).unwrap() =
            serde_json::to_value(sb::store::ContentDigest::of(b"changed RAM")).unwrap();
    }
    (a, b)
}

fn write_pages(path: &std::path::Path, record: &mut sb::orchestration::RunRecord, pfns: &[usize]) {
    // The fixture records 512 MiB, or 131072 guest pages.
    let mut bytes = vec![0; 1024 * 1024];
    for pfn in pfns {
        bytes[pfn * 8..pfn * 8 + 8].copy_from_slice(&0x0123456789abcdefu64.to_le_bytes());
    }
    std::fs::write(path.join("output/machines/server.pages"), &bytes).unwrap();
    record.logs.insert("machines/server.pages".into(), sb::store::ContentDigest::of(&bytes));
}
