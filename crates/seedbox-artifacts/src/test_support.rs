//! Opt-in run-record and report fixtures for this crate's integration tests.
// Feature-gated rather than cfg(test): integration tests are separate crates.
use crate as sb;

pub fn fixture() -> sb::record::RunRecord {
    use sb::{config, execution::*, record::*, store::ContentDigest};
    use std::collections::BTreeMap;
    let digest = ContentDigest::of(b"fixture");
    let configuration = config::Simulation {
        name: "diff".into(),
        source: "/fixture/seedbox.yaml".into(),
        services: BTreeMap::from([(
            "workload".into(),
            config::Service {
                memory_mb: None,
                source: config::ImageSource::Registry {
                    reference: "fixture:v1".into(),
                },
                platform: "linux/amd64".into(),
                entrypoint: None,
                command: None,
                environment: BTreeMap::new(),
                working_dir: None,
                user: None,
                configs: vec![],
                volumes: vec![],
                network: "default".into(),
            },
        )]),
        configs: BTreeMap::new(),
        network: config::Network {
            name: "default".into(),
            latency_ns: 1_000_000,
            jitter_ns: 0,
            bandwidth_bps: None,
            queue_bound_ns: None,
            links: vec![],
        },
        simulator: Default::default(),
        schedule: config::Schedule::default(),
        control: None,
        limits: config::Limits {
            simulated_time_ns: 1_000_000_000,
            wall_time_ns: 60_000_000_000,
        },
        check: config::Check {
            service: "workload".into(),
            exit_code: 0,
        },
        seed: Some(0),
        wall_clock_epoch: None,
        machine: config::MachineOptions::default(),
        disk: config::DiskOptions::default(),
    };
    let identity = RunIdentity {
        program: Vec::new(),
        version: 2,
        seed: 0,
        configuration,
        inputs: PreparedInputs {
            machine_memory_mb: BTreeMap::from([("workload".into(), 512)]),
            wall_clock_epoch: None,
            images: BTreeMap::new(),
            placement: BTreeMap::from([("workload".into(), "workload".into())]),
            guests: BTreeMap::from([("workload".into(), digest.clone())]),
            profile: ExecutionProfile {
                backend: "fixture".into(),
                witness_mode: WitnessMode::Exits,
                semantic_parameters: SemanticParameters::new(),
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
    }
}

pub fn raw_report() -> serde_json::Value {
    serde_json::json!({
        "report_version": 1, "seed": 0, "outcome": "end_machine_exited",
        "world_witness": sb::store::ContentDigest::of(b"fixture"),
        "machines": [{"name":"workload", "witness":sb::store::ContentDigest::of(b"machine"),
            "state":"Exited(0)", "time_ns":100, "guest_exit_code":0,
            "guest_init_error":null, "infrastructure_error":false, "run_ms":10}]
    })
}

// The current engine emits textual events alongside hashed state parts.
pub fn current_report(full_state: bool) -> serde_json::Value {
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
            for part in [
                "regs",
                "xsave",
                "xcrs",
                "msrs",
                "time_page",
                "ram",
                "pending_io",
                "disk",
            ] {
                parts.insert(
                    part.into(),
                    serde_json::to_value(sb::store::ContentDigest::of(part.as_bytes())).unwrap(),
                );
            }
            parts.insert(
                "events".into(),
                "exc:none int:none shadow:0 nmi:0/0/0".into(),
            );
            parts.into()
        } else {
            serde_json::Value::Null
        };
    }
    raw
}

pub fn write_fixture(
    path: &std::path::Path,
    record: &mut sb::record::RunRecord,
    raw: Option<&serde_json::Value>,
) {
    write_fixture_logs(path, record, raw, &[]);
}

pub fn write_fixture_logs(
    path: &std::path::Path,
    record: &mut sb::record::RunRecord,
    raw: Option<&serde_json::Value>,
    logs: &[(&str, Option<&[u8]>)],
) {
    use sb::store::ContentDigest;
    std::fs::create_dir_all(path.join("output/machines/machines")).unwrap();
    // Synthetic fixture descriptions are encoded into the CURRENT retained
    // formats here.
    let state = raw.and_then(|raw| {
        use std::collections::BTreeMap;
        if let Some(report) = record.report.as_mut() {
            report.witness = raw
                .get("world_witness")
                .map(|value| serde_json::from_value(value.clone()).unwrap());
        }
        let machines = raw["machines"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|machine| {
                let digest = machine
                    .get("state_digest")
                    .filter(|value| !value.is_null())?;
                let name = machine["name"].as_str().unwrap();
                let regions = vec![sb::evidence::RamRegion {
                    base: 0,
                    length: 512 * 1024 * 1024,
                }];
                let mut encoded = Vec::new();
                encoded.extend(0_u64.to_le_bytes());
                encoded.extend((512_u64 * 1024 * 1024).to_le_bytes());
                let mut parts: BTreeMap<String, sb::store::ContentDigest> = machine
                    .get("state_digest_parts")
                    .and_then(|value| value.as_object())
                    .into_iter()
                    .flatten()
                    .map(|(name, value)| {
                        let value = value.as_str().unwrap();
                        (
                            name.clone(),
                            sb::store::ContentDigest::try_from(value.to_owned())
                                .unwrap_or_else(|_| sb::store::ContentDigest::of(value.as_bytes())),
                        )
                    })
                    .collect();
                parts
                    .entry("ram".into())
                    .or_insert_with(|| sb::store::ContentDigest::of(b"fixture RAM"));
                parts.insert("ram_regions".into(), sb::store::ContentDigest::of(&encoded));
                let disposition = if let Some(code) = machine["guest_exit_code"].as_i64() {
                    sb::evidence::StateDisposition::Exited { code: code as i32 }
                } else {
                    sb::evidence::StateDisposition::Shutdown
                };
                Some((
                    name.to_owned(),
                    sb::evidence::StateCapture {
                        boot: 0,
                        disposition,
                        digest: serde_json::from_value(digest.clone()).unwrap(),
                        parts,
                        regions,
                        page_log: format!("machines/{name}.pages"),
                    },
                ))
            })
            .collect::<BTreeMap<_, _>>();
        if let Some(report) = record.report.as_mut() {
            report.full_state = (!machines.is_empty()).then(|| {
                machines
                    .iter()
                    .map(|(name, state)| (name.clone(), state.digest.clone()))
                    .collect()
            });
        }
        (!machines.is_empty()).then(|| {
            serde_json::to_vec(&sb::evidence::StateReport {
                version: 1,
                machines,
            })
            .unwrap()
        })
    });
    let report_log = format!(
        "{}/{}",
        sb::evidence::MACHINE_OUTPUT_DIR,
        sb::evidence::STATE_REPORT
    );
    let logs = std::iter::once((report_log.as_str(), state.as_deref())).chain(logs.iter().copied());
    for (name, bytes) in logs {
        if let Some(bytes) = bytes {
            let destination = path.join("output").join(name);
            std::fs::create_dir_all(destination.parent().unwrap()).unwrap();
            std::fs::write(destination, bytes).unwrap();
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

pub fn ram_pair() -> (serde_json::Value, serde_json::Value) {
    let a = current_report(true);
    let mut b = a.clone();
    for key in ["state_digest", "state_digest_parts/ram"] {
        *b.pointer_mut(&format!("/machines/1/{key}")).unwrap() =
            serde_json::to_value(sb::store::ContentDigest::of(b"changed RAM")).unwrap();
    }
    (a, b)
}

pub fn write_pages(path: &std::path::Path, record: &mut sb::record::RunRecord, pfns: &[usize]) {
    // The fixture records 512 MiB, or 131072 guest pages.
    let mut bytes = vec![0; 1024 * 1024];
    for pfn in pfns {
        bytes[pfn * 8..pfn * 8 + 8].copy_from_slice(&0x0123456789abcdefu64.to_le_bytes());
    }
    std::fs::create_dir_all(path.join("output/machines/machines")).unwrap();
    std::fs::write(path.join("output/machines/machines/server.pages"), &bytes).unwrap();
    record.logs.insert(
        "machines/machines/server.pages".into(),
        sb::store::ContentDigest::of(&bytes),
    );
}
