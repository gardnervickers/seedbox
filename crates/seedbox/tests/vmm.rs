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
        assert!(world["machines"]
            .as_array()
            .unwrap()
            .iter()
            .all(|m| m.get("disk").is_none()));
        // A machine whose service mounts a named volume gets a retained disk.
        let mut prepared = request.prepared.clone();
        let hash = ContentDigest::of(b"retained input");
        prepared.images.insert(
            "server".into(),
            PreparedImage {
                manifest: hash.clone(),
                config: hash.clone(),
                layers: vec![],
                launch: seedbox::image::Launch {
                    argv: vec!["/app".into()],
                    environment: BTreeMap::new(),
                    working_dir: "/".into(),
                    user: String::new(),
                    volumes: vec![],
                    disk_volumes: vec![seedbox::config::VolumeMount {
                        source: "state".into(),
                        target: "/state".into(),
                    }],
                },
            },
        );
        let world = vmm::world(&ExecutionRequest {
            prepared: &prepared,
            ..*request
        })
        .unwrap();
        let disks: Vec<_> = world["machines"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| (m["name"].as_str().unwrap(), m.get("disk").cloned()))
            .collect();
        assert_eq!(
            disks,
            [
                ("client-machine", None),
                (
                    "server-machine",
                    Some(json!({"size_mib": vmm::VOLUME_DISK_MIB}))
                )
            ]
        );
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
        // The target's reported exit is the evidence for the unavailable status.
        raw["machines"][1]["guest_exit_code"] = json!(0);
        raw["machines"][1]["state"] = json!("Exited(0)");
        raw["machines"][1]["time_ns"] = json!(e);
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
        // A target that did not exit, or exited at another time, cannot be unavailable.
        for state in [json!("Running"), json!("Halted"), json!(null)] {
            let mut live = raw.clone();
            live["machines"][1]["guest_exit_code"] = json!(null);
            live["machines"][1]["state"] = state;
            assert!(parse(&live).is_err());
        }
        let mut other_exited = raw.clone();
        other_exited["machines"][1]["state"] = json!("Running");
        other_exited["machines"][0]["state"] = json!("Exited(1)");
        other_exited["machines"][0]["time_ns"] = json!(e);
        assert!(parse(&other_exited).is_err());
        for time in [json!(e - 1), json!(e + 1), json!(null)] {
            let mut mismatch = raw.clone();
            mismatch["machines"][1]["time_ns"] = time;
            assert!(parse(&mismatch).is_err());
        }
        // The engine's debug state is canonical Exited(u8), not an arbitrary prefix.
        for state in [
            "Exited(",
            "Exited()",
            "Exited(Running)",
            "Exited(256)",
            "Exited(-1)",
            "Exited(+0)",
            "Exited(00)",
            "Exited(0)garbage",
            "Exited(0)\n",
            "Exited( 0)",
        ] {
            let mut malformed = raw.clone();
            malformed["machines"][1]["state"] = json!(state);
            assert!(
                parse(&malformed).is_err(),
                "accepted malformed exit state: {state:?}"
            );
        }
        for code in [1u8, u8::MAX] {
            let mut exited = raw.clone();
            exited["machines"][1]["state"] = json!(format!("Exited({code})"));
            exited["machines"][1]["guest_exit_code"] = json!(code);
            assert!(parse(&exited).is_ok());
        }
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

fn machine_fault_row(
    id: u32,
    op: &str,
    t: u64,
    latency: u64,
) -> (serde_json::Value, serde_json::Value) {
    let a = t + latency;
    let reply = a + latency;
    let plan = json!({"id":id,"action":op,"machine":"server-machine","at_ns":a,"deadline_ns":a+5_000_000_000u64});
    (
        json!({"relay_id":id,"session_id":1,"request":{"version":1,"id":id,"op":op,"target":{"service":"server"}},"response":{"version":1,"id":id,"ok":true,"result":{},"requested_at_ns":t,"admitted_at_ns":a,"effect_at_ns":a,"replied_at_ns":reply},"fault_id":id,"delivered_at_ns":reply}),
        json!({"id":id,"request":plan,"requested_at_ns":a,"applied_at_ns":a,"action":op,"machine":"server-machine","status":"applied","detail":"machine lifecycle"}),
    )
}

#[test]
fn machine_lifecycle_transcripts_attribute_services_and_require_admission_effects() {
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
        let rows: Vec<_> = ["pause", "resume", "crash", "reboot"]
            .iter()
            .enumerate()
            .map(|(i, op)| machine_fault_row(i as u32 + 1, op, (i as u64 + 1) * 10_000_000, l))
            .collect();
        raw["control_history"] = json!(rows.iter().map(|(r, _)| r).collect::<Vec<_>>());
        raw["faults"] = json!(rows.iter().map(|(_, f)| f).collect::<Vec<_>>());
        let parse =
            |raw: &serde_json::Value| vmm::report(&serde_json::to_vec(raw).unwrap(), &request);
        let parsed = parse(&raw).unwrap();
        assert_eq!(parsed.reason, StopReason::Completed);
        assert_eq!(parsed.faults.len(), 4);
        for (i, op) in ["pause", "resume", "crash", "reboot"].iter().enumerate() {
            assert_eq!(parsed.faults[i].action, *op);
            assert_eq!(parsed.faults[i].service, "server");
            assert_eq!(
                parsed.faults[i].requested_at_ns,
                parsed.faults[i].applied_at_ns
            );
            let mut late = raw.clone();
            let a = late["control_history"][i]["response"]["admitted_at_ns"]
                .as_u64()
                .unwrap();
            late["control_history"][i]["response"]["effect_at_ns"] = json!(a + 1);
            late["control_history"][i]["response"]["replied_at_ns"] = json!(a + 1 + l);
            late["control_history"][i]["delivered_at_ns"] = json!(a + 1 + l);
            late["faults"][i]["applied_at_ns"] = json!(a + 1);
            assert!(parse(&late).is_err());
            for target in [
                json!({"service":"client"}),
                json!({"machine":"client-machine"}),
            ] {
                let mut controller = raw.clone();
                controller["control_history"][i]["request"]["target"] = target;
                controller["faults"][i]["request"]["machine"] = json!("client-machine");
                controller["faults"][i]["machine"] = json!("client-machine");
                assert!(parse(&controller).is_err());
            }
            let mut machine = raw.clone();
            machine["control_history"][i]["request"]["target"] =
                json!({"machine":"server-machine"});
            assert!(parse(&machine).is_ok());
        }
        // A cutoff lifecycle plan must also have applied exactly at admission.
        let mut cut = report();
        let (row, fault) = machine_fault_row(1, "crash", 10_000_000, l);
        let a = row["response"]["admitted_at_ns"].as_u64().unwrap();
        cut["control_latency_ns"] = json!(l);
        cut["outcome"] = json!("duration");
        cut["control_history"] = json!([{"relay_id":1,"session_id":1,"request":row["request"],"status":"cut_off","requested_at_ns":10_000_000,"admitted_at_ns":a,"admitted_done":true,"effect_at_ns":a,"replied_at_ns":a+l,"cut_off_at_ns":a+l-1,"fault_id":1,"plan":fault["request"],"error":null,"fatal":false}]);
        cut["faults"] = json!([fault]);
        assert_eq!(parse(&cut).unwrap().reason, StopReason::SimulatedTimeLimit);
        cut["faults"][0]["applied_at_ns"] = json!(a + 1);
        assert!(parse(&cut).is_err());
    });
}

#[test]
fn lifecycle_invalid_state_and_exited_pause_resume_are_recoverable_without_faults() {
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
        for (op, code) in [
            ("pause", "invalid_state"),
            ("resume", "invalid_state"),
            ("crash", "invalid_state"),
            ("reboot", "invalid_state"),
            ("pause", "target_unavailable"),
            ("resume", "target_unavailable"),
        ] {
            let mut raw = report();
            raw["control_latency_ns"] = json!(l);
            let (mut row, _) = machine_fault_row(1, op, 10_000_000, l);
            row["fault_id"] = json!(null);
            row["response"]["ok"] = json!(false);
            row["response"]["error"] = json!({"code":code,"message":"transition rejected"});
            raw["control_history"] = json!([row]);
            let parse =
                |raw: &serde_json::Value| vmm::report(&serde_json::to_vec(raw).unwrap(), &request);
            let parsed = parse(&raw).unwrap();
            assert_eq!(parsed.reason, StopReason::Completed);
            assert!(parsed.faults.is_empty() && parsed.detail.is_none());
            if code == "invalid_state" {
                raw["control_history"][0]["fault_id"] = json!(1);
                assert!(parse(&raw).is_err());
            }
        }
    });
}

fn unavailable_machine_fault(
    row: &mut serde_json::Value,
    fault: &mut serde_json::Value,
    e: u64,
    latency: u64,
) {
    row["response"]["ok"] = json!(false);
    row["response"]["error"] =
        json!({"code":"target_unavailable","message":"target machine unavailable"});
    row["response"]["effect_at_ns"] = json!(e);
    row["response"]["replied_at_ns"] = json!(e + latency);
    row["delivered_at_ns"] = json!(e + latency);
    fault["applied_at_ns"] = json!(null);
    fault["status"] = json!("unavailable");
}

#[test]
fn unavailable_process_fault_requires_exact_crash_evidence_in_its_window() {
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
        let parse =
            |raw: &serde_json::Value| vmm::report(&serde_json::to_vec(raw).unwrap(), &request);
        for op in ["stop", "restart"] {
            let (mut row, mut fault) = machine_fault_row(1, op, 10_000_000, l);
            let a = row["response"]["admitted_at_ns"].as_u64().unwrap();
            let e = a + 1000;
            unavailable_machine_fault(&mut row, &mut fault, e, l);
            raw["control_history"] = json!([row]);
            raw["faults"] = json!([fault]);
            // A newer running incarnation need not have a stopped clock.
            raw["machines"][1]["state"] = json!("Running");
            raw["machines"][1]["time_ns"] = json!(e + 10_000);
            raw["machines"][1]["crashed_incarnations"] =
                json!([{"incarnation":0,"crashed_at_ns":e}]);
            let parsed = parse(&raw).unwrap();
            assert_eq!(parsed.reason, StopReason::Completed);
            assert!(parsed.faults.is_empty());
            for crashes in [
                json!([]),
                json!([{"crashed_at_ns":e-1}]),
                json!([{"crashed_at_ns":e+1}]),
            ] {
                let mut bad = raw.clone();
                bad["machines"][1]["crashed_incarnations"] = crashes;
                assert!(parse(&bad).is_err());
            }
            let mut other = raw.clone();
            other["machines"][0]["crashed_incarnations"] =
                other["machines"][1]["crashed_incarnations"].take();
            assert!(parse(&other).is_err());
            for e in [a - 1, a + 5_000_000_001] {
                let mut bad = raw.clone();
                let mut row = bad["control_history"][0].take();
                let mut fault = bad["faults"][0].take();
                unavailable_machine_fault(&mut row, &mut fault, e, l);
                bad["control_history"][0] = row;
                bad["faults"][0] = fault;
                bad["machines"][1]["crashed_incarnations"] = json!([{"crashed_at_ns":e}]);
                assert!(parse(&bad).is_err());
            }
        }
    });
}

#[test]
fn unavailable_at_admission_requires_prior_crash_without_intervening_reboot() {
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
        let parse =
            |raw: &serde_json::Value| vmm::report(&serde_json::to_vec(raw).unwrap(), &request);
        for op in ["stop", "restart"] {
            let mut raw = report();
            raw["control_latency_ns"] = json!(l);
            let (crash, crash_fault) = machine_fault_row(1, "crash", 10_000_000, l);
            let crash_at = crash_fault["applied_at_ns"].clone();
            let (mut unavailable, mut unavailable_fault) = machine_fault_row(2, op, 20_000_000, l);
            let a = unavailable["response"]["admitted_at_ns"].as_u64().unwrap();
            unavailable_machine_fault(&mut unavailable, &mut unavailable_fault, a, l);
            raw["control_history"] = json!([crash, unavailable]);
            raw["faults"] = json!([crash_fault, unavailable_fault]);
            raw["machines"][1]["state"] = json!("Down");
            raw["machines"][1]["crashed_incarnations"] = json!([{"crashed_at_ns":crash_at}]);
            let parsed = parse(&raw).unwrap();
            assert_eq!(parsed.reason, StopReason::Completed);
            assert_eq!(parsed.faults.len(), 1);
            assert_eq!(parsed.faults[0].action, "crash");
            let mut late = raw.clone();
            let mut row = late["control_history"][1].take();
            let mut fault = late["faults"][1].take();
            unavailable_machine_fault(&mut row, &mut fault, a + 1, l);
            late["control_history"][1] = row;
            late["faults"][1] = fault;
            assert!(parse(&late).is_err());
            // Reboot clears admission-time evidence even if final state says Down.
            let (reboot, reboot_fault) = machine_fault_row(2, "reboot", 20_000_000, l);
            let (mut row, mut fault) = machine_fault_row(3, op, 30_000_000, l);
            let a = row["response"]["admitted_at_ns"].as_u64().unwrap();
            unavailable_machine_fault(&mut row, &mut fault, a, l);
            raw["control_history"].as_array_mut().unwrap()[1] = reboot;
            raw["control_history"].as_array_mut().unwrap().push(row);
            raw["faults"].as_array_mut().unwrap()[1] = reboot_fault;
            raw["faults"].as_array_mut().unwrap().push(fault);
            assert!(parse(&raw).is_err());
        }
    });
}

#[test]
fn counter_identity_requires_an_attributable_matching_engine_report() {
    fixture(|base| {
        let mut inputs = base.prepared.clone();
        inputs
            .profile
            .semantic_parameters
            .insert("counter".into(), "intel-0x5111c4".into());
        let request = ExecutionRequest {
            prepared: &inputs,
            ..*base
        };
        assert!(vmm::report(&serde_json::to_vec(&report()).unwrap(), &request).is_err());
        let mut raw = report();
        raw["counter_profile"] = json!("amd-zen");
        assert!(vmm::report(&serde_json::to_vec(&raw).unwrap(), &request).is_err());
        raw["counter_profile"] = json!("intel-0x5111c4");
        assert!(vmm::report(&serde_json::to_vec(&raw).unwrap(), &request).is_ok());
        inputs
            .profile
            .semantic_parameters
            .insert("counter".into(), "unknown".into());
        let request = ExecutionRequest {
            prepared: &inputs,
            ..*base
        };
        assert!(vmm::report(&serde_json::to_vec(&raw).unwrap(), &request).is_err());
    });
}
