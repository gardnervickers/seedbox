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
        control: None,
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

#[test]
fn runtime_requests_resolve_placement_and_reports_validate_rpc_causality() {
    fixture(|base| {
        let c = seedbox::config::Control {
            service: "client".into(),
        };
        let request = ExecutionRequest {
            control: Some(&c),
            ..*base
        };
        let w = vmm::world(&request).unwrap();
        assert_eq!(w["control"]["machine"], "client-machine");
        let latency = w["control"]["latency_ns"].as_u64().unwrap();
        let t = 10_000_000;
        let a = t + latency;
        let e = a + 1000;
        let reply = e + latency;
        let call = json!({"version":1,"id":1,"op":"stop","target":{"service":"server"}});
        let plan = json!({"id":1,"at_ns":a,"deadline_ns":a+5_000_000_000,"action":"stop","machine":"server-machine"});
        let mut raw = report();
        raw["control_latency_ns"] = json!(latency);
        raw["control_history"] = json!([{"relay_id":1,"request":call,"response":{"version":1,"id":1,"ok":true,"result":{},"requested_at_ns":t,"admitted_at_ns":a,"effect_at_ns":e,"replied_at_ns":reply},"delivered_at_ns":reply}]);
        raw["faults"] = json!([{"id":1,"request":plan,"requested_at_ns":a,"applied_at_ns":e,"action":"stop","machine":"server-machine","status":"applied","detail":"reaped"}]);
        assert_eq!(
            vmm::report(&serde_json::to_vec(&raw).unwrap(), &request)
                .unwrap()
                .faults
                .len(),
            1
        );
        for field in ["relay_id", "delivered_at_ns"] {
            let mut bad = raw.clone();
            bad["control_history"][0][field] = json!(0);
            assert!(vmm::report(&serde_json::to_vec(&bad).unwrap(), &request).is_err());
        }
        let mut mismatch = raw.clone();
        mismatch["faults"][0]["applied_at_ns"] = json!(e + 1);
        assert!(vmm::report(&serde_json::to_vec(&mismatch).unwrap(), &request).is_err());
        let mut bad = raw.clone();
        bad["control_history"][0]["response"]["admitted_at_ns"] = json!(a + 1);
        assert!(vmm::report(&serde_json::to_vec(&bad).unwrap(), &request).is_err());
        let mut bad = raw.clone();
        bad["faults"][0]["request"]["machine"] = json!("client-machine");
        assert!(vmm::report(&serde_json::to_vec(&bad).unwrap(), &request).is_err());
        let mut bad = raw.clone();
        bad["control_history"][0]["request"]["id"] = json!(2);
        bad["control_history"][0]["response"]["id"] = json!(2);
        assert!(vmm::report(&serde_json::to_vec(&bad).unwrap(), &request).is_err());
    });
}

#[test]
fn runtime_report_rejects_impossible_sleep_and_now_metadata() {
    fixture(|base| {
        let c = seedbox::config::Control {
            service: "client".into(),
        };
        let request = ExecutionRequest {
            control: Some(&c),
            ..*base
        };
        let l = vmm::world(&request).unwrap()["control"]["latency_ns"]
            .as_u64()
            .unwrap();
        for op in ["now", "sleep"] {
            let t = 10_000_000;
            let a = t + l;
            let duration = 1000;
            let e = if op == "sleep" { a + duration } else { a };
            let reply = e + l;
            let mut call = json!({"version":1,"id":1,"op":op});
            if op == "sleep" {
                call["duration_ns"] = json!(duration);
            }
            let mut raw = report();
            raw["control_latency_ns"] = json!(l);
            raw["control_history"] = json!([{"relay_id":1,"request":call,"response":{"version":1,"id":1,"ok":true,"result":if op=="now"{json!({"now_ns":e})}else{json!({})},"requested_at_ns":t,"admitted_at_ns":a,"effect_at_ns":e,"replied_at_ns":reply},"delivered_at_ns":reply}]);
            assert!(vmm::report(&serde_json::to_vec(&raw).unwrap(), &request).is_ok());
            raw["control_history"][0]["response"]["effect_at_ns"] = json!(e + 1);
            raw["control_history"][0]["response"]["replied_at_ns"] = json!(reply + 1);
            raw["control_history"][0]["delivered_at_ns"] = json!(reply + 1);
            assert!(vmm::report(&serde_json::to_vec(&raw).unwrap(), &request).is_err());
        }
    });
}

#[test]
fn pending_rpc_at_world_cutoff_preserves_timeout_and_workload_failure() {
    fixture(|base| {
        let control = seedbox::config::Control {
            service: "client".into(),
        };
        let request = ExecutionRequest {
            control: Some(&control),
            ..*base
        };
        let l = vmm::world(&request).unwrap()["control"]["latency_ns"]
            .as_u64()
            .unwrap();
        let t = 10_000_000;
        let a = t + l;
        let plan = json!({"id":1,"at_ns":a,"deadline_ns":a+5_000_000_000u64,"action":"stop","machine":"server-machine"});
        let mut raw = report();
        raw["control_latency_ns"] = json!(l);
        raw["control_history"] = json!([{"relay_id":1,"request":{"version":1,"id":1,"op":"stop","target":{"service":"server"}},"status":"cut_off","requested_at_ns":t,"admitted_at_ns":a,"admitted_done":true,"effect_at_ns":null,"replied_at_ns":null,"cut_off_at_ns":1_000_000_000,"fault_id":1,"plan":plan}]);
        raw["faults"] = json!([{"id":1,"request":plan,"requested_at_ns":a,"applied_at_ns":null,"action":"stop","machine":"server-machine","status":"requested","detail":"awaiting readiness"}]);
        raw["outcome"] = json!("duration");
        assert_eq!(
            vmm::report(&serde_json::to_vec(&raw).unwrap(), &request)
                .unwrap()
                .reason,
            StopReason::SimulatedTimeLimit
        );
        raw["outcome"] = json!("end_machine_exited");
        raw["machines"][0]["guest_exit_code"] = json!(1);
        assert_eq!(
            vmm::report(&serde_json::to_vec(&raw).unwrap(), &request)
                .unwrap()
                .reason,
            StopReason::Completed
        );
        let mut rejected = raw.clone();
        rejected["fault_error"] = json!("request from non-controller machine");
        rejected["control_history"].as_array_mut().unwrap().insert(
            0,
            json!({"status":"rejected","detail":"request from non-controller machine"}),
        );
        let retained = vmm::report(&serde_json::to_vec(&rejected).unwrap(), &request).unwrap();
        assert_eq!(retained.reason, StopReason::Error);
        assert_eq!(
            retained.detail.as_deref(),
            Some("request from non-controller machine")
        );
        assert!(retained.witness.is_some());
        raw["control_history"][0]["plan"]["machine"] = json!("client-machine");
        assert!(vmm::report(&serde_json::to_vec(&raw).unwrap(), &request).is_err());
    });
}

#[test]
fn process_target_exit_after_admission_is_a_recoverable_unavailable_fault() {
    fixture(|base| {
        let control = seedbox::config::Control {
            service: "client".into(),
        };
        let request = ExecutionRequest {
            control: Some(&control),
            ..*base
        };
        let l = vmm::world(&request).unwrap()["control"]["latency_ns"]
            .as_u64()
            .unwrap();
        let t = 10_000_000;
        let a = t + l;
        let e = a + 1000;
        let reply = e + l;
        let call = json!({"version":1,"id":1,"op":"stop","target":{"service":"server"}});
        let plan = json!({"id":1,"at_ns":a,"deadline_ns":a+5_000_000_000u64,"action":"stop","machine":"server-machine"});
        let error = json!({"code":"target_unavailable","message":"target VM exited before the process effect"});
        let mut raw = report();
        raw["control_latency_ns"] = json!(l);
        raw["control_history"] = json!([{"relay_id":1,"session_id":1,"request":call,"response":{"version":1,"id":1,"ok":false,"error":error,"requested_at_ns":t,"admitted_at_ns":a,"effect_at_ns":e,"replied_at_ns":reply},"fault_id":1,"delivered_at_ns":reply}]);
        raw["faults"] = json!([{"id":1,"request":plan,"requested_at_ns":a,"applied_at_ns":null,"action":"stop","machine":"server-machine","status":"unavailable","detail":"target VM exited before the process effect"}]);
        raw["machines"][0]["guest_exit_code"] = json!(1);
        let parse =
            |raw: &serde_json::Value| vmm::report(&serde_json::to_vec(raw).unwrap(), &request);
        let accepted = parse(&raw).unwrap();
        assert_eq!(accepted.reason, StopReason::Completed);
        assert_eq!(accepted.workload_exit.unwrap().code, 1);
        assert!(accepted.faults.is_empty() && accepted.detail.is_none());
        // A reply cut off by the world limit retains the same unavailable fault.
        let mut cut = raw.clone();
        cut["outcome"] = json!("duration");
        cut["control_history"] = json!([{"relay_id":1,"session_id":1,"request":call,"status":"cut_off","requested_at_ns":t,"admitted_at_ns":a,"admitted_done":true,"effect_at_ns":e,"replied_at_ns":reply,"cut_off_at_ns":reply-1,"fault_id":1,"plan":plan,"error":error["message"],"fatal":false}]);
        assert_eq!(parse(&cut).unwrap().reason, StopReason::SimulatedTimeLimit);
        let mut fatal = cut.clone();
        fatal["control_history"][0]["fatal"] = json!(true);
        assert!(parse(&fatal).is_err());
        // Only a target_unavailable reply after admission permits the status.
        let mut other = raw.clone();
        other["control_history"][0]["response"]["error"]["code"] = json!("invalid_state");
        assert!(parse(&other).is_err());
        let mut applied = raw.clone();
        applied["faults"][0]["status"] = json!("applied");
        applied["faults"][0]["applied_at_ns"] = json!(e);
        assert!(parse(&applied).is_err());
        let mut failed = raw.clone();
        failed["faults"][0]["status"] = json!("failed");
        assert!(parse(&failed).is_err());
        let mut late = raw.clone();
        let late_effect = a + 5_000_000_001u64;
        late["control_history"][0]["response"]["effect_at_ns"] = json!(late_effect);
        late["control_history"][0]["response"]["replied_at_ns"] = json!(late_effect + l);
        late["control_history"][0]["delivered_at_ns"] = json!(late_effect + l);
        assert!(parse(&late).is_err());
        let mut partition = raw.clone();
        partition["control_history"][0]["request"] =
            json!({"version":1,"id":1,"op":"heal","handle":"p1"});
        partition["control_history"][0]["response"]["effect_at_ns"] = json!(a);
        partition["control_history"][0]["response"]["replied_at_ns"] = json!(a + l);
        partition["control_history"][0]["delivered_at_ns"] = json!(a + l);
        assert!(parse(&partition).is_err());
    });
}

#[test]
fn in_flight_reply_at_world_cutoff_keeps_outcome_even_when_other_clock_is_later() {
    fixture(|base| {
        let control = seedbox::config::Control {
            service: "client".into(),
        };
        let request = ExecutionRequest {
            control: Some(&control),
            ..*base
        };
        let l = vmm::world(&request).unwrap()["control"]["latency_ns"]
            .as_u64()
            .unwrap();
        let t = 10_000_000;
        let a = t + l;
        let reply = a + l;
        for (outcome, reason, detail) in [
            ("end_machine_exited", StopReason::Completed, None),
            (
                "fault_failed",
                StopReason::Error,
                Some("transport delivery failed"),
            ),
        ] {
            let mut raw = report();
            raw["outcome"] = json!(outcome);
            raw["fault_error"] = json!(detail);
            raw["control_latency_ns"] = json!(l);
            raw["control_history"] = json!([{"relay_id":1,"session_id":1,"request":{"version":1,"id":1,"op":"now"},"response":{"version":1,"id":1,"ok":true,"result":{"now_ns":a},"requested_at_ns":t,"admitted_at_ns":a,"effect_at_ns":a,"replied_at_ns":reply},"status":"cut_off","cut_off_at_ns":reply+1000}]);
            let parsed = vmm::report(&serde_json::to_vec(&raw).unwrap(), &request).unwrap();
            assert_eq!(parsed.reason, reason);
            assert_eq!(parsed.detail.as_deref(), detail);
            assert!(parsed.witness.is_some());
        }
    });
}

#[test]
fn transcript_correlates_global_relay_and_connection_local_ids() {
    fixture(|base| {
        let control = seedbox::config::Control {
            service: "client".into(),
        };
        let request = ExecutionRequest {
            control: Some(&control),
            ..*base
        };
        let l = vmm::world(&request).unwrap()["control"]["latency_ns"]
            .as_u64()
            .unwrap();
        let mut raw = report();
        raw["control_latency_ns"] = json!(l);
        let rows: Vec<_> = [1,3].into_iter().enumerate().map(|(index,session)| {
            let t = (index as u64 + 1)*10_000_000; let a=t+l; let reply=a+l;
            json!({"relay_id":index+1,"session_id":session,"request":{"version":1,"id":1,"op":"now"},"response":{"version":1,"id":1,"ok":true,"result":{"now_ns":a},"requested_at_ns":t,"admitted_at_ns":a,"effect_at_ns":a,"replied_at_ns":reply},"delivered_at_ns":reply})
        }).collect();
        raw["control_history"] = json!(rows);
        assert!(vmm::report(&serde_json::to_vec(&raw).unwrap(), &request).is_ok());
        raw["control_history"][1]["session_id"] = json!(1);
        assert!(vmm::report(&serde_json::to_vec(&raw).unwrap(), &request).is_err());
    });
}

#[test]
fn typed_network_plan_preserves_targets_without_attributing_check_service() {
    fixture(|base| {
        let control = seedbox::config::Control {
            service: "client".into(),
        };
        let mut prepared = base.prepared.clone();
        prepared
            .guests
            .insert("peer-machine".into(), prepared.kernel.clone());
        prepared
            .placement
            .insert("peer".into(), "peer-machine".into());
        let request = ExecutionRequest {
            control: Some(&control),
            prepared: &prepared,
            ..*base
        };
        let l = vmm::world(&request).unwrap()["control"]["latency_ns"]
            .as_u64()
            .unwrap();
        let t = 10_000_000;
        let a = t + l;
        let reply = a + l;
        let plan = json!({"id":1,"action":"partition","at_ns":a,"deadline_ns":a+5_000_000_000u64,"handle":"p1","left":["server-machine"],"right":["peer-machine"]});
        let mut raw = report();
        raw["control_latency_ns"] = json!(l);
        let mut peer = raw["machines"][1].clone();
        peer["name"] = json!("peer-machine");
        raw["machines"].as_array_mut().unwrap().push(peer);
        raw["control_history"] = json!([{"relay_id":1,"request":{"version":1,"id":1,"op":"partition","left":[{"service":"server"}],"right":[{"service":"peer"}]},"response":{"version":1,"id":1,"ok":true,"result":{"handle":"p1"},"requested_at_ns":t,"admitted_at_ns":a,"effect_at_ns":a,"replied_at_ns":reply},"delivered_at_ns":reply}]);
        raw["faults"] = json!([{"request":plan,"id":1,"requested_at_ns":a,"applied_at_ns":a,"action":"partition","machine":null,"status":"applied","detail":"partition p1"}]);
        let parsed = vmm::report(&serde_json::to_vec(&raw).unwrap(), &request).unwrap();
        assert!(parsed.faults[0].service.is_empty());
        let mut empty_handle = raw.clone();
        empty_handle["control_history"][0]["response"]["result"]["handle"] = json!("");
        empty_handle["faults"][0]["request"]["handle"] = json!("");
        assert!(vmm::report(&serde_json::to_vec(&empty_handle).unwrap(), &request).is_err());
        let mut pending = raw.clone();
        pending["faults"][0]["status"] = json!("requested");
        pending["faults"][0]["applied_at_ns"] = serde_json::Value::Null;
        assert!(vmm::report(&serde_json::to_vec(&pending).unwrap(), &request).is_err());
        let mut controller_target = raw.clone();
        controller_target["control_history"][0]["request"]["left"][0]["service"] = json!("client");
        controller_target["faults"][0]["request"]["left"][0] = json!("client-machine");
        assert!(vmm::report(&serde_json::to_vec(&controller_target).unwrap(), &request).is_err());
        raw["faults"][0]["request"]["deadline_ns"] = json!("bad");
        assert!(vmm::report(&serde_json::to_vec(&raw).unwrap(), &request).is_err());
    });
}
