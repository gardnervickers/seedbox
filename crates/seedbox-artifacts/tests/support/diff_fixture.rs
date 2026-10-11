// Included by both library unit tests and CLI integration tests (`sb` is the crate).
fn fixture() -> sb::record::RunRecord {
    use sb::{config, execution::*, record::*, store::ContentDigest};
    use std::collections::BTreeMap;
    let digest = ContentDigest::of(b"fixture");
    let configuration = config::Simulation {
        name: "diff".into(), source: "/fixture/compose.yaml".into(),
        services: BTreeMap::from([("workload".into(), config::Service {
            source: config::ImageSource::Registry { reference: "fixture:v1".into() },
            platform: "linux/amd64".into(), entrypoint: None, command: None,
            environment: BTreeMap::new(), working_dir: None, user: None,
            configs: vec![], volumes: vec![], network: "default".into(),
        })]), configs: BTreeMap::new(),
        network: config::Network { name: "default".into(), latency_ns: 1_000_000,
            jitter_ns: 0, bandwidth_bps: None, queue_bound_ns: None, links: vec![] },
        simulator: Default::default(),
        schedule: config::Schedule::default(), control: None,
        limits: config::Limits { simulated_time_ns: 1_000_000_000, wall_time_ns: 60_000_000_000 },
        check: config::Check { service: "workload".into(), exit_code: 0 },
        seed: Some(0), wall_clock_epoch: None,
        machine: config::MachineOptions::default(), disk: config::DiskOptions::default(),
    };
    let identity = RunIdentity {
        program: Vec::new(),
            interventions: None,
        version: 2,
        seed: 0,
        configuration,
        inputs: PreparedInputs {
            wall_clock_epoch: None,
            kernel: digest.clone(),
            images: BTreeMap::new(),
            placement: BTreeMap::from([("workload".into(), "workload".into())]),
            guests: BTreeMap::from([("workload".into(), digest.clone())]),
            profile: ExecutionProfile {
                backend: Backend::Kvm,
                engine: digest.clone(),
                witness_mode: WitnessMode::Exits,
                semantic_parameters: SemanticParameters::new(512),
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
            host_wait_ns: 0,
            simulated_time_ns: None,
            reason: StopReason::Completed,
            workload_exit: Some(WorkloadExit {
                service: "workload".into(),
                code: 0,
            }),
            witness: Some(digest),
            full_state: None,
            faults: vec![],
            detail: None,
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
            "state":"Exited(0)", "time_ns":100, "guest_exit_code":0,
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
    record: &mut sb::record::RunRecord,
    raw: Option<&serde_json::Value>,
    trace: Option<&[u8]>,
) {
    write_fixture_logs(path, record, raw, &[("machines/workload.trace", trace)]);
}

/// Encode engine witness-log entries (time_ns, records, witness prefix).
#[allow(dead_code)]
fn witness_log_bytes(entries: &[[u64; 3]]) -> Vec<u8> {
    entries.iter().flatten().flat_map(|v| v.to_le_bytes()).collect()
}

fn write_fixture_logs(
    path: &std::path::Path,
    record: &mut sb::record::RunRecord,
    raw: Option<&serde_json::Value>,
    logs: &[(&str, Option<&[u8]>)],
) {
    use sb::store::ContentDigest;
    std::fs::create_dir_all(path.join("output/machines")).unwrap();
    let raw = raw.map(|v| serde_json::to_vec(v).unwrap());
    let logs = std::iter::once(("vmm.json", raw.as_deref())).chain(logs.iter().copied());
    for (name, bytes) in logs {
        if let Some(bytes) = bytes {
            std::fs::write(path.join("output").join(name), bytes).unwrap();
            record.logs.insert(name.into(), ContentDigest::of(bytes));
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

fn write_pages(path: &std::path::Path, record: &mut sb::record::RunRecord, pfns: &[usize]) {
    // The fixture records 512 MiB, or 131072 guest pages.
    let mut bytes = vec![0; 1024 * 1024];
    for pfn in pfns {
        bytes[pfn * 8..pfn * 8 + 8].copy_from_slice(&0x0123456789abcdefu64.to_le_bytes());
    }
    std::fs::write(path.join("output/machines/server.pages"), &bytes).unwrap();
    record.logs.insert("machines/server.pages".into(), sb::store::ContentDigest::of(&bytes));
}
