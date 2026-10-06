use seedbox::{
    config::{Check, Limits, Network, Schedule},
    execution::*,
    store::ContentDigest,
    vmm,
};
use serde_json::json;
use std::{collections::BTreeMap, path::Path};

fn fixture(test: impl FnOnce(&ExecutionRequest<'_>)) {
    let hash = ContentDigest::of(b"retained input");
    let prepared = PreparedInputs {
        kernel: hash.clone(),
        images: BTreeMap::new(),
        placement: BTreeMap::from([
            ("server".into(), "server-machine".into()),
            ("client".into(), "client-machine".into()),
        ]),
        guests: BTreeMap::from([
            ("server-machine".into(), hash.clone()),
            ("client-machine".into(), hash.clone()),
        ]),
        profile: ExecutionProfile {
            backend: "seedbox-kvm".into(),
            engine: hash.clone(),
            witness_mode: WitnessMode::FullState,
            semantic_parameters: BTreeMap::from([("memory_mb".into(), "512".into())]),
        },
        auxiliary: BTreeMap::new(),
        provenance: BTreeMap::new(),
    };
    let artifacts = BTreeMap::from([(hash, "/verified/retained-input".into())]);
    let request = ExecutionRequest {
        seed: 1009,
        limits: &Limits {
            simulated_time_ns: 1_000_000_000,
            wall_time_ns: 5_000_000_000,
        },
        network: &Network {
            name: "private".into(),
            latency_ns: 100_000,
            jitter_ns: 20_000,
        },
        schedule: &Schedule {
            timer_jitter_ns: 50_000,
            stop_tolerance_ns: 40_000,
        },
        check: &Check {
            service: "client".into(),
            exit_code: 0,
        },
        prepared: &prepared,
        artifacts: &artifacts,
        output: Path::new("/private/output"),
        cancellation: &CancellationToken::default(),
        performance_parameters: &BTreeMap::new(),
    };
    test(&request);
}

fn report() -> serde_json::Value {
    let hash = ContentDigest::of(b"witness");
    json!({"report_version":1,"seed":1009,"outcome":"end_machine_exited","world_witness":hash,"machines":[
        {"name":"client-machine","guest_exit_code":0,"guest_init_error":null,"infrastructure_error":false,"state_digest":hash},
        {"name":"server-machine","guest_exit_code":null,"guest_init_error":null,"infrastructure_error":false,"state_digest":hash}
    ]})
}

#[test]
fn world_uses_verified_artifacts_and_explicit_placement() {
    fixture(|request| {
        let world = vmm::world(request).unwrap();
        assert_eq!(world["end_on_exit"], "client-machine");
        assert_eq!(world["kernel"], "/verified/retained-input");
        assert_eq!(world["machines"][0]["initrd"], "/verified/retained-input");
        assert_eq!(world["timer_jitter_ns"], 50_000);
        assert_eq!(world["stop_tolerance_ns"], 40_000);
        assert_eq!(world["duration_ns"], 1_000_000_000);
    });
}

#[test]
fn application_exit_255_remains_distinct_from_infrastructure_failure() {
    fixture(|request| {
        let mut raw = report();
        raw["machines"][0]["guest_exit_code"] = json!(255);
        let result = vmm::report(&serde_json::to_vec(&raw).unwrap(), request).unwrap();
        assert_eq!(result.reason, StopReason::Completed);
        assert_eq!(
            result.workload_exit.unwrap(),
            WorkloadExit {
                service: "client".into(),
                code: 255
            }
        );
        assert_eq!(result.full_state.unwrap().len(), 2);
        raw["machines"][1]["infrastructure_error"] = json!(true);
        let result = vmm::report(&serde_json::to_vec(&raw).unwrap(), request).unwrap();
        assert_eq!(result.reason, StopReason::Error);
        assert!(result.detail.unwrap().contains("server-machine"));
    });
}

#[test]
fn reports_reject_wrong_seed_missing_state_and_duplicate_machines() {
    fixture(|request| {
        let mut raw = report();
        raw["seed"] = json!(1010);
        assert!(vmm::report(&serde_json::to_vec(&raw).unwrap(), request).is_err());
        let mut raw = report();
        raw["machines"][0]["state_digest"] = json!(null);
        assert!(vmm::report(&serde_json::to_vec(&raw).unwrap(), request).is_err());
        let mut raw = report();
        raw["machines"][0] = raw["machines"][1].clone();
        assert!(vmm::report(&serde_json::to_vec(&raw).unwrap(), request).is_err());
        let mut raw = report();
        raw["outcome"] = json!("duration");
        raw["machines"][0]["guest_exit_code"] = json!(null);
        let result = vmm::report(&serde_json::to_vec(&raw).unwrap(), request).unwrap();
        assert_eq!(result.reason, StopReason::SimulatedTimeLimit);
        assert!(result.workload_exit.is_none());
    });
}
