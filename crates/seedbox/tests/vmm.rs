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
            links: Vec::new(),
            name: "private".into(),
            latency_ns: 100_000,
            jitter_ns: 20_000,
            bandwidth_bps: None,
            queue_bound_ns: None,
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
fn initial_arm_launch_uses_numeric_cap_and_network_bound() {
    fixture(|base| {
        assert!(vmm::world(base).unwrap().get("launch_skew_ns").is_none());
        let mut prepared = base.prepared.clone();
        prepared.profile.backend = "seedbox-hvf".into();
        prepared
            .profile
            .semantic_parameters
            .extend(seedbox::engine::arm_parameters());
        // Old ARM profiles keep their original world config for replay.
        let request = ExecutionRequest {
            prepared: &prepared,
            ..*base
        };
        assert!(vmm::world(&request)
            .unwrap()
            .get("launch_skew_ns")
            .is_none());
        prepared.profile.semantic_parameters.extend([
            ("arm_launch_policy".into(), "seeded-barrier-v1".into()),
            ("arm_launch_max_skew_ns".into(), "1000000".into()),
        ]);
        let request = ExecutionRequest {
            prepared: &prepared,
            ..*base
        };
        assert_eq!(vmm::world(&request).unwrap()["launch_skew_ns"], 80_000);
        prepared
            .profile
            .semantic_parameters
            .remove("arm_launch_policy");
        let request = ExecutionRequest {
            prepared: &prepared,
            ..*base
        };
        assert_eq!(vmm::world(&request).unwrap()["launch_skew_ns"], 80_000);
        prepared
            .profile
            .semantic_parameters
            .insert("arm_launch_max_skew_ns".into(), "0".into());
        let request = ExecutionRequest {
            prepared: &prepared,
            ..*base
        };
        assert_eq!(vmm::world(&request).unwrap()["launch_skew_ns"], 0);
        prepared
            .profile
            .semantic_parameters
            .insert("arm_launch_max_skew_ns".into(), "NaN".into());
        let request = ExecutionRequest {
            prepared: &prepared,
            ..*base
        };
        assert!(vmm::world(&request).is_err());
    });
}

#[test]
fn current_parts_accept_text_events_and_validate_digests() {
    fixture(|request| {
        let mut raw = report();
        for machine in raw["machines"].as_array_mut().unwrap() {
            machine["state_digest_parts"] = json!({
                "ram": ContentDigest::of(b"ram"), "disk": ContentDigest::of(b"disk"),
                "events": "exc:none int:none shadow:0 nmi:0/0/0"
            });
        }
        let parsed = vmm::report(&serde_json::to_vec(&raw).unwrap(), request).unwrap();
        assert_eq!(parsed.full_state.as_ref().unwrap().len(), 2);
        raw["machines"][1]["state_digest_parts"]["disk"] = json!("invalid");
        assert!(vmm::report(&serde_json::to_vec(&raw).unwrap(), request)
            .unwrap_err()
            .to_string()
            .contains("expected a lowercase SHA-256 digest"));
    });
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
        prepared
            .profile
            .semantic_parameters
            .insert("checkpoint_at_ns".into(), "500".into());
        prepared
            .profile
            .semantic_parameters
            .insert("checkpoint_branch_seed".into(), "42".into());
        let checkpoint_world = vmm::world(&ExecutionRequest {
            prepared: &prepared,
            ..*request
        })
        .unwrap();
        let parent = checkpoint_protocol::parent_identity(
            &checkpoint_world,
            request.seed,
            prepared.profile.engine.hex(),
            "counter",
            false,
            |_| Ok(ContentDigest::of(b"retained input").hex().to_owned()),
        )
        .unwrap();
        assert_eq!(checkpoint_world["checkpoint"]["parent_identity"], parent);
        assert_eq!(checkpoint_world["machines"], world["machines"]);
        prepared.images.clear();
        let without_volume = vmm::world(&ExecutionRequest {
            prepared: &prepared,
            ..*request
        })
        .unwrap();
        assert_ne!(without_volume["checkpoint"]["parent_identity"], parent);
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

#[test]
fn baseline_links_resolve_service_names_to_prepared_machines() {
    fixture(|base| {
        assert!(vmm::world(base).unwrap()["network"].get("links").is_none());
        let mut network = base.network.clone();
        network.links.push(seedbox::config::Link {
            from: "server".into(),
            to: "client".into(),
            latency_ns: 200_000,
            jitter_ns: 10_000,
            loss_ppm: 500_000,
            bandwidth_bps: None,
            queue_bound_ns: None,
        });
        let request = ExecutionRequest {
            network: &network,
            ..*base
        };
        assert_eq!(
            vmm::world(&request).unwrap()["network"]["links"],
            json!([{"from":"server-machine","to":"client-machine","latency_ns":200_000,"jitter_ns":10_000,"loss_ppm":500_000}])
        );
        network.bandwidth_bps = Some(10_000_000);
        network.queue_bound_ns = Some(20_000_000);
        network.links[0].bandwidth_bps = Some(1_000_000);
        network.links[0].queue_bound_ns = Some(12_000_000);
        let world = vmm::world(&ExecutionRequest {
            network: &network,
            ..*base
        })
        .unwrap();
        assert_eq!(world["network"]["bandwidth_bps"], 10_000_000);
        assert_eq!(world["network"]["queue_bound_ns"], 20_000_000);
        assert_eq!(world["network"]["links"][0]["bandwidth_bps"], 1_000_000);
        assert_eq!(world["network"]["links"][0]["queue_bound_ns"], 12_000_000);
        network.links[0].from = "missing".into();
        assert!(vmm::world(&ExecutionRequest {
            network: &network,
            ..*base
        })
        .is_err());
    });
}

#[test]
fn shape_transcripts_validate_handle_timing_absolute_policy_and_directed_sides() {
    fixture(|base| {
        // Use a third machine as controller so both sides are in the failure model.
        let mut prepared = base.prepared.clone();
        let hash = prepared.kernel.clone();
        prepared
            .placement
            .insert("scenario".into(), "scenario-machine".into());
        prepared
            .guests
            .insert("scenario-machine".into(), hash.clone());
        let check = Check {
            service: "scenario".into(),
            exit_code: 0,
        };
        let control = seedbox::config::Control {
            service: "scenario".into(),
        };
        let request = ExecutionRequest {
            prepared: &prepared,
            check: &check,
            control: Some(&control),
            ..*base
        };
        let latency = vmm::world(&request).unwrap()["control"]["latency_ns"]
            .as_u64()
            .unwrap();
        let t = 10_000_000;
        let a = t + latency;
        let reply = a + latency;
        let mut raw = report();
        raw["machines"].as_array_mut().unwrap().push(json!({"name":"scenario-machine","guest_exit_code":0,"guest_init_error":null,"infrastructure_error":false,"state_digest":hash}));
        raw["control_latency_ns"] = json!(latency);
        let call = json!({"version":1,"id":1,"op":"shape","from":[{"service":"server"}],"to":[{"service":"client"}],"latency_ns":200_000,"jitter_ns":10_000,"loss_ppm":500_000});
        let plan = json!({"id":1,"at_ns":a,"deadline_ns":a+5_000_000_000,"action":"shape","handle":"p1","from":["server-machine"],"to":["client-machine"],"latency_ns":200_000,"jitter_ns":10_000,"loss_ppm":500_000});
        raw["control_history"] = json!([{"relay_id":1,"request":call,"response":{"version":1,"id":1,"ok":true,"result":{"handle":"p1"},"requested_at_ns":t,"admitted_at_ns":a,"effect_at_ns":a,"replied_at_ns":reply},"delivered_at_ns":reply}]);
        raw["faults"] = json!([{"id":1,"request":plan,"requested_at_ns":a,"applied_at_ns":a,"action":"shape","machine":null,"status":"applied","detail":"handle=p1"}]);
        let result = vmm::report(&serde_json::to_vec(&raw).unwrap(), &request).unwrap();
        assert_eq!(result.faults[0].action, "shape");
        let mut finite = raw.clone();
        for (key, value) in [
            ("bandwidth_bps", json!(1_000_000)),
            ("queue_bound_ns", json!(12_000_000)),
        ] {
            finite["control_history"][0]["request"][key] = value.clone();
            finite["faults"][0]["request"][key] = value;
        }
        assert!(vmm::report(&serde_json::to_vec(&finite).unwrap(), &request).is_ok());
        finite["faults"][0]["request"]["bandwidth_bps"] = json!(2_000_000);
        assert!(vmm::report(&serde_json::to_vec(&finite).unwrap(), &request).is_err());
        assert!(result.faults[0].service.is_empty());
        for (key, value) in [
            ("latency_ns", json!(99_999)),
            ("jitter_ns", json!(200_000)),
            ("loss_ppm", json!(1_000_001)),
            ("bandwidth_bps", json!(0)),
            ("queue_bound_ns", json!(12_000_000)),
            ("from", json!([])),
            ("to", json!([{"service":"server"}])),
        ] {
            let mut bad = raw.clone();
            bad["control_history"][0]["request"][key] = value;
            assert!(vmm::report(&serde_json::to_vec(&bad).unwrap(), &request).is_err());
        }
        for handle in [json!(""), json!(null), json!("p2")] {
            let mut bad = raw.clone();
            bad["control_history"][0]["response"]["result"]["handle"] = handle;
            assert!(vmm::report(&serde_json::to_vec(&bad).unwrap(), &request).is_err());
        }
        let mut bad = raw.clone();
        bad["control_history"][0]["response"]["effect_at_ns"] = json!(a + 1);
        assert!(vmm::report(&serde_json::to_vec(&bad).unwrap(), &request).is_err());
        // Admission followed by world cutoff retains the planned handle and policy.
        raw["outcome"] = json!("duration");
        raw["control_history"][0]["plan"] = raw["faults"][0]["request"].clone();
        raw["control_history"][0]["fault_id"] = json!(1);
        raw["control_history"][0]["response"] = json!(null);
        raw["control_history"][0]["cut_off_at_ns"] = json!(a);
        raw["control_history"][0]["status"] = json!("cut_off");
        raw["control_history"][0]["requested_at_ns"] = json!(t);
        raw["control_history"][0]["admitted_at_ns"] = json!(a);
        raw["control_history"][0]["admitted_done"] = json!(true);
        raw["control_history"][0]["delivered_at_ns"] = json!(null);
        assert!(vmm::report(&serde_json::to_vec(&raw).unwrap(), &request).is_ok());
    });
}

#[test]
fn connection_selector_transcripts_bind_exact_machine_tuple_incarnation_and_time() {
    fixture(|base| {
        let mut prepared = base.prepared.clone();
        let hash = prepared.kernel.clone();
        prepared
            .placement
            .insert("scenario".into(), "scenario-machine".into());
        prepared
            .guests
            .insert("scenario-machine".into(), hash.clone());
        let check = Check {
            service: "scenario".into(),
            exit_code: 0,
        };
        let control = seedbox::config::Control {
            service: "scenario".into(),
        };
        let request = ExecutionRequest {
            prepared: &prepared,
            check: &check,
            control: Some(&control),
            ..*base
        };
        let latency = vmm::world(&request).unwrap()["control"]["latency_ns"]
            .as_u64()
            .unwrap();
        let t = 10_000_000;
        let a = t + latency;
        let reply = a + latency;
        let connection = json!({"from":"server-machine","to":"client-machine","src_addr":"10.0.0.2","dst_addr":"10.0.0.1","src_port":80,"dst_port":4000,"incarnation":7});
        let mut raw = report();
        raw["machines"].as_array_mut().unwrap().push(json!({"name":"scenario-machine","guest_exit_code":0,"guest_init_error":null,"infrastructure_error":false,"state_digest":hash}));
        raw["control_latency_ns"] = json!(latency);
        let call = json!({"version":1,"id":1,"op":"drop_connection","connection":connection});
        let plan = json!({"id":1,"at_ns":a,"deadline_ns":a+5_000_000_000,"action":"drop_connection","handle":"p1","connection":connection});
        raw["control_history"] = json!([{"relay_id":1,"request":call,"response":{"version":1,"id":1,"ok":true,"result":{"handle":"p1"},"requested_at_ns":t,"admitted_at_ns":a,"effect_at_ns":a,"replied_at_ns":reply},"delivered_at_ns":reply}]);
        raw["faults"] = json!([{"id":1,"request":plan,"requested_at_ns":a,"applied_at_ns":a,"action":"drop_connection","machine":null,"status":"applied","detail":"armed","selection_counts":{"matched":0,"dropped":0}}]);
        let result = vmm::report(&serde_json::to_vec(&raw).unwrap(), &request).unwrap();
        assert_eq!(result.faults[0].action, "drop_connection");
        for (key, value) in [
            ("incarnation", json!(8)),
            ("incarnation", json!(0)),
            ("src_port", json!(0)),
            ("dst_addr", json!("bad")),
            ("from", json!("scenario-machine")),
            ("to", json!("server-machine")),
        ] {
            let mut bad = raw.clone();
            bad["control_history"][0]["request"]["connection"][key] = value;
            assert!(vmm::report(&serde_json::to_vec(&bad).unwrap(), &request).is_err());
        }
        let mut bad = raw.clone();
        bad["control_history"][0]["response"]["effect_at_ns"] = json!(a + 1);
        assert!(vmm::report(&serde_json::to_vec(&bad).unwrap(), &request).is_err());
        // A unavailable incarnation is a recoverable admission error with no plan.
        raw["faults"] = json!([]);
        raw["control_history"][0]["response"]["ok"] = json!(false);
        raw["control_history"][0]["response"]["error"] =
            json!({"code":"invalid_state","message":"connection incarnation unavailable"});
        assert!(vmm::report(&serde_json::to_vec(&raw).unwrap(), &request).is_ok());
        raw["control_history"][0]["request"] = json!({"version":1,"id":1,"op":"connections","from":{"service":"server"},"to":{"service":"client"},"after":0});
        raw["control_history"][0]["response"]["ok"] = json!(true);
        raw["control_history"][0]["response"]["result"] =
            json!({"connections":[connection],"next_after":7});
        assert!(vmm::report(&serde_json::to_vec(&raw).unwrap(), &request).is_ok());
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

#[test]
fn committed_trigger_reports_correlate_arms_effects_cancellation_and_cutoff() {
    fixture(|base| {
        let c = seedbox::config::Control {
            service: "client".into(),
        };
        let request = ExecutionRequest {
            control: Some(&c),
            ..*base
        };
        let latency = vmm::world(&request).unwrap()["control"]["latency_ns"]
            .as_u64()
            .unwrap();
        let arm = 100 + latency;
        let matched = arm + 2 * latency;
        let effect = matched + latency;
        let trigger = json!({"id":1,"source":"server-machine","marker":[65,66],"armed_at_ns":arm,"match_deadline_ns":arm+10_000_000,"action":{"action":"stop","machine":"server-machine"},"prefix":2,"seen":2,"resets":0,"matched_at_ns":matched,"effect_at_ns":effect,"fault_id":1,"cancelled_at_ns":null,"cut_off_at_ns":null,"status":"fired"});
        let mut raw = report();
        raw["control_latency_ns"] = json!(latency);
        raw["control_history"] = json!([{"relay_id":1,"session_id":1,"request":{"version":1,"id":1,"op":"arm_trigger","source":{"service":"server"},"marker":[65,66],"match_timeout_ns":10_000_000,"fault":{"action":"stop","target":{"service":"server"}}},"response":{"version":1,"id":1,"ok":true,"result":{"trigger_id":1},"requested_at_ns":100,"admitted_at_ns":arm,"effect_at_ns":arm,"replied_at_ns":arm+latency},"delivered_at_ns":arm+latency}]);
        raw["triggers"] = json!([trigger]);
        let plan = json!({"id":1,"at_ns":effect,"deadline_ns":effect+5_000_000_000u64,"action":"stop","machine":"server-machine"});
        raw["faults"] = json!([{"id":1,"request":plan,"requested_at_ns":effect,"applied_at_ns":effect+10,"action":"stop","machine":"server-machine","status":"applied","detail":"reaped"}]);
        let parse =
            |raw: &serde_json::Value| vmm::report(&serde_json::to_vec(raw).unwrap(), &request);
        assert_eq!(parse(&raw).unwrap().faults.len(), 1);
        // A matched trigger rejected at admission creates no second fault.
        let mut rejected = raw.clone();
        let stop_at = arm + 2 * latency;
        let stopped = stop_at + 1;
        rejected["control_history"].as_array_mut().unwrap().push(json!({"relay_id":2,"session_id":1,"fault_id":1,"request":{"version":1,"id":2,"op":"stop","target":{"service":"server"}},"response":{"version":1,"id":2,"ok":true,"result":{},"requested_at_ns":arm+latency,"admitted_at_ns":stop_at,"effect_at_ns":stopped,"replied_at_ns":stopped+latency},"delivered_at_ns":stopped+latency}));
        rejected["faults"][0]["request"]["at_ns"] = json!(stop_at);
        rejected["faults"][0]["request"]["deadline_ns"] = json!(stop_at + 5_000_000_000u64);
        rejected["faults"][0]["requested_at_ns"] = json!(stop_at);
        rejected["faults"][0]["applied_at_ns"] = json!(stopped);
        rejected["triggers"][0]["matched_at_ns"] = json!(arm + 4 * latency);
        rejected["triggers"][0]["effect_at_ns"] = json!(arm + 5 * latency);
        rejected["triggers"][0]["status"] = json!("rejected");
        rejected["triggers"][0]["fault_id"] = json!(null);
        rejected["triggers"][0]["rejection_reason"] = json!("target process is already stopped");
        assert_eq!(parse(&rejected).unwrap().faults.len(), 1);
        for (field, value) in [
            ("rejection_reason", json!(null)),
            ("rejection_reason", json!("")),
            ("fault_id", json!(2)),
            ("matched_at_ns", json!(null)),
            ("effect_at_ns", json!(request.limits.simulated_time_ns)),
            ("cancelled_at_ns", json!(arm)),
            ("cut_off_at_ns", json!(arm + 5 * latency)),
        ] {
            let mut bad = rejected.clone();
            bad["triggers"][0][field] = value;
            assert!(parse(&bad).is_err(), "accepted rejected trigger {field}");
        }
        let mut unavailable = raw.clone();
        unavailable["faults"][0]["status"] = json!("unavailable");
        unavailable["faults"][0]["applied_at_ns"] = json!(null);
        unavailable["machines"][1]["state"] = json!("Exited(0)");
        unavailable["machines"][1]["time_ns"] = json!(effect + 5);
        assert!(parse(&unavailable).is_ok());
        unavailable["machines"][1]["time_ns"] = json!(effect - 1);
        assert!(parse(&unavailable).is_err());

        // Triggered stops can become unavailable when their machine crashes,
        // even if the final report describes a later running incarnation.
        let mut crashed = unavailable.clone();
        crashed["machines"][1]["state"] = json!("Running");
        crashed["machines"][1]["time_ns"] = json!(effect + 100);
        for crash_at in [effect, effect + 5] {
            crashed["machines"][1]["crashed_incarnations"] = json!([{"crashed_at_ns":crash_at}]);
            assert!(parse(&crashed).is_ok());
        }
        for crash_at in [effect - 1, effect + 5_000_000_001] {
            crashed["machines"][1]["crashed_incarnations"] = json!([{"crashed_at_ns":crash_at}]);
            assert!(parse(&crashed).is_err());
        }
        // A crash before the effect needs a matching prior control plan.
        let mut down = crashed.clone();
        let crash_at = effect - 1;
        down["machines"][1]["crashed_incarnations"] = json!([{"crashed_at_ns":crash_at}]);
        down["triggers"][0]["fault_id"] = json!(2);
        down["faults"][0]["id"] = json!(2);
        down["faults"][0]["request"]["id"] = json!(2);
        let (mut row, crash_fault) = machine_fault_row(1, "crash", crash_at - latency, latency);
        row["relay_id"] = json!(2);
        row["request"]["id"] = json!(2);
        row["response"]["id"] = json!(2);
        down["control_history"].as_array_mut().unwrap().push(row);
        down["faults"]
            .as_array_mut()
            .unwrap()
            .insert(0, crash_fault);
        assert!(parse(&down).is_ok(), "{:?}", parse(&down));
        for op in ["pause", "reboot"] {
            let mut bad = down.clone();
            bad["control_history"][1]["request"]["op"] = json!(op);
            bad["faults"][0]["action"] = json!(op);
            bad["faults"][0]["request"]["action"] = json!(op);
            assert!(parse(&bad).is_err());
        }

        for (field, value) in [
            ("source", json!("client-machine")),
            ("matched_at_ns", json!(arm - 1)),
            ("effect_at_ns", json!(matched)),
            ("fault_id", json!(2)),
            ("prefix", json!(1)),
            ("status", json!("armed")),
            ("marker", json!([65])),
            ("rejection_reason", json!("unexpected rejection")),
        ] {
            let mut bad = raw.clone();
            bad["triggers"][0][field] = value;
            assert!(parse(&bad).is_err(), "accepted {field}");
        }
        let mut missing = raw.clone();
        missing.as_object_mut().unwrap().remove("triggers");
        assert!(parse(&missing).is_err());
        let mut duplicate = raw.clone();
        let duplicate_record = duplicate["triggers"][0].clone();
        duplicate["triggers"]
            .as_array_mut()
            .unwrap()
            .push(duplicate_record);
        assert!(parse(&duplicate).is_err());
        // Armed with no match at the terminal cutoff retains its partial prefix.
        let mut cutoff = raw.clone();
        cutoff["faults"] = json!([]);
        for field in ["matched_at_ns", "effect_at_ns", "fault_id"] {
            cutoff["triggers"][0][field] = json!(null);
        }
        cutoff["triggers"][0]["prefix"] = json!(1);
        cutoff["triggers"][0]["status"] = json!("cut_off");
        cutoff["triggers"][0]["cut_off_at_ns"] = json!(arm + 1000);
        assert!(parse(&cutoff).is_ok());
        // Arm admitted but its reply was not yet generated at cutoff.
        let mut pending = cutoff.clone();
        let row = &mut pending["control_history"][0];
        let metadata = row["response"].clone();
        row.as_object_mut().unwrap().remove("response");
        for field in ["requested_at_ns", "admitted_at_ns", "effect_at_ns"] {
            row[field] = metadata[field].clone();
        }
        row["replied_at_ns"] = json!(arm + latency);
        row["result"] = json!({"trigger_id":1});
        row["status"] = json!("cut_off");
        row["admitted_done"] = json!(true);
        row["cut_off_at_ns"] = json!(arm + 1000);
        row["delivered_at_ns"] = json!(null);
        assert!(parse(&pending).is_ok());
        // A cancellation must have a successful causally correlated request.
        let mut cancelled = cutoff.clone();
        cancelled["triggers"][0]["cut_off_at_ns"] = json!(null);
        cancelled["triggers"][0]["status"] = json!("cancelled");
        let time = arm + 2 * latency;
        let admission = time + latency;
        cancelled["triggers"][0]["cancelled_at_ns"] = json!(admission);
        assert!(parse(&cancelled).is_err());
        cancelled["control_history"].as_array_mut().unwrap().push(json!({"relay_id":2,"session_id":1,"request":{"version":1,"id":2,"op":"cancel_trigger","trigger_id":1},"response":{"version":1,"id":2,"ok":true,"result":{},"requested_at_ns":time,"admitted_at_ns":admission,"effect_at_ns":admission,"replied_at_ns":admission+latency},"delivered_at_ns":admission+latency}));
        assert!(parse(&cancelled).is_ok());
        // A regular RPC after the triggered fault shares the sequential fault IDs.
        let mut interleaved = raw.clone();
        let admission = effect + 1000 + latency;
        let plan = json!({"id":2,"at_ns":admission,"deadline_ns":admission+5_000_000_000u64,"action":"stop","machine":"server-machine"});
        interleaved["control_history"].as_array_mut().unwrap().push(json!({"relay_id":2,"session_id":1,"fault_id":2,"request":{"version":1,"id":2,"op":"stop","target":{"service":"server"}},"response":{"version":1,"id":2,"ok":true,"result":{},"requested_at_ns":effect+1000,"admitted_at_ns":admission,"effect_at_ns":admission+1,"replied_at_ns":admission+1+latency},"delivered_at_ns":admission+1+latency}));
        interleaved["faults"].as_array_mut().unwrap().push(json!({"id":2,"request":plan,"requested_at_ns":admission,"applied_at_ns":admission+1,"action":"stop","machine":"server-machine","status":"applied","detail":"reaped"}));
        assert_eq!(parse(&interleaved).unwrap().faults.len(), 2);
    });
}

#[test]
fn torn_writes_are_recorded_replayed_and_omitted_when_disabled() {
    use seedbox::{counter::CounterProfile, store::ContentStore};
    let dir = tempfile::tempdir().unwrap();
    let engine = dir.path().join("engine");
    std::fs::write(&engine, b"engine").unwrap();
    let store = ContentStore::new(dir.path().join("objects"));
    let executor = vmm::VmmExecutor::new(
        &engine,
        &store,
        None,
        512,
        WitnessMode::Exits,
        CounterProfile::Zen,
    )
    .unwrap();
    let original = executor.profile();
    // Off adds nothing; the host policy parameters #83 records stay as they are.
    assert!(!original
        .semantic_parameters
        .contains_key("disk_loss_sector_bytes"));
    assert_eq!(original.semantic_parameters["memory_mb"], "512");
    let encoded = serde_json::to_vec(&original).unwrap();
    assert!(!vmm::recorded_torn_writes(&original).unwrap());
    assert_eq!(
        serde_json::to_vec(&executor.clone().with_torn_writes(false).profile()).unwrap(),
        encoded
    );
    let enabled = executor.clone().with_torn_writes(true);
    let recorded: ExecutionProfile =
        serde_json::from_slice(&serde_json::to_vec(&enabled.profile()).unwrap()).unwrap();
    assert_eq!(
        recorded.semantic_parameters["disk_loss_sector_bytes"],
        "512"
    );
    assert_eq!(
        recorded.semantic_parameters["disk_loss_survival_probability"],
        "0.5"
    );
    assert_ne!(serde_json::to_vec(&recorded).unwrap(), encoded);
    let replay = executor
        .clone()
        .with_torn_writes(vmm::recorded_torn_writes(&recorded).unwrap());
    assert_eq!(replay.profile(), recorded);
    assert_eq!(
        replay.performance_parameters(),
        executor.performance_parameters()
    );
    assert_eq!(enabled.with_torn_writes(false).profile(), original);

    fixture(|base| {
        let original_world = vmm::world(base).unwrap();
        assert!(original_world.get("disk_loss").is_none());
        let mut prepared = base.prepared.clone();
        prepared.profile = recorded;
        let request = ExecutionRequest {
            prepared: &prepared,
            ..*base
        };
        let mut world = vmm::world(&request).unwrap();
        assert_eq!(
            world.as_object_mut().unwrap().remove("disk_loss"),
            Some(json!({
                "sector_bytes": 512,
                "survival_probability": 0.5,
            }))
        );
        assert_eq!(
            serde_json::to_vec(&world).unwrap(),
            serde_json::to_vec(&original_world).unwrap()
        );
        prepared
            .profile
            .semantic_parameters
            .insert("disk_loss_survival_probability".into(), "2.0".into());
        assert!(vmm::recorded_torn_writes(&prepared.profile)
            .unwrap_err()
            .to_string()
            .contains("unknown disk loss parameters"));
        let request = ExecutionRequest {
            prepared: &prepared,
            ..*base
        };
        assert!(vmm::world(&request).is_err());

        let mut legacy = original.clone();
        legacy.semantic_parameters.insert(
            "disk_loss".into(),
            "flush-stable/seeded-latest-sector-512-half/disk-loss-machine-incarnation/v1".into(),
        );
        let error = vmm::recorded_torn_writes(&legacy).unwrap_err().to_string();
        assert!(error.contains("legacy disk_loss semantic parameter"));
    });
}

#[test]
fn timebase_reports_validate_recorded_semantics_and_accept_legacy_counter_reports() {
    fixture(|base| {
        let legacy = vmm::report(&serde_json::to_vec(&report()).unwrap(), base).unwrap();
        assert_eq!(legacy.timebase, Some(Timebase::Counter));
        for timebase in [Timebase::Counter, Timebase::Pv] {
            let mut prepared = base.prepared.clone();
            if timebase == Timebase::Pv {
                prepared
                    .profile
                    .semantic_parameters
                    .insert("timebase".into(), "pv".into());
            }
            let request = ExecutionRequest {
                prepared: &prepared,
                ..*base
            };
            let mut raw = report();
            raw["timebase"] = json!(timebase.name());
            raw["counter_profile"] = json!(null);
            let parsed = vmm::report(&serde_json::to_vec(&raw).unwrap(), &request).unwrap();
            assert_eq!(parsed.timebase, Some(timebase));
            assert_eq!(
                serde_json::to_value(parsed).unwrap()["timebase"],
                timebase.name()
            );
            for other in ["counter", "pv", "unknown"] {
                if other == timebase.name() {
                    continue;
                }
                raw["timebase"] = json!(other);
                let error = vmm::report(&serde_json::to_vec(&raw).unwrap(), &request)
                    .unwrap_err()
                    .to_string();
                assert!(error.contains(&format!("--timebase {other}")), "{error}");
            }
            raw.as_object_mut().unwrap().remove("timebase");
            assert_eq!(
                vmm::report(&serde_json::to_vec(&raw).unwrap(), &request).is_ok(),
                timebase == Timebase::Counter
            );
        }
        let mut prepared = base.prepared.clone();
        prepared
            .profile
            .semantic_parameters
            .insert("timebase".into(), "future".into());
        let error = vmm::recorded_timebase(&prepared.profile)
            .unwrap_err()
            .to_string();
        assert!(error.contains("recorded --timebase future: expected counter or pv"));
    });
}

#[test]
fn pv_reports_may_omit_unqualified_counter_profile_but_counter_reports_must_not() {
    use seedbox::counter::CounterProfile;
    fixture(|base| {
        let mut prepared = base.prepared.clone();
        prepared
            .profile
            .semantic_parameters
            .insert("counter".into(), CounterProfile::IntelSkylake.name().into());
        for timebase in [Timebase::Counter, Timebase::Pv] {
            prepared
                .profile
                .semantic_parameters
                .insert("timebase".into(), timebase.name().into());
            let request = ExecutionRequest {
                prepared: &prepared,
                ..*base
            };
            let mut raw = report();
            raw["timebase"] = json!(timebase.name());
            raw["counter_profile"] = json!(null);
            let parsed = vmm::report(&serde_json::to_vec(&raw).unwrap(), &request);
            if timebase == Timebase::Counter {
                assert!(parsed
                    .unwrap_err()
                    .to_string()
                    .contains("unqualified VMM report is missing its counter profile"));
            } else {
                assert_eq!(parsed.unwrap().timebase, Some(Timebase::Pv));
            }
            raw["counter_profile"] = json!(CounterProfile::IntelSkylake.name());
            assert!(vmm::report(&serde_json::to_vec(&raw).unwrap(), &request).is_ok());
            raw["counter_profile"] = json!(CounterProfile::Zen.name());
            assert!(vmm::report(&serde_json::to_vec(&raw).unwrap(), &request)
                .unwrap_err()
                .to_string()
                .contains("different branch counter profile"));
        }
    });
}

#[test]
fn deferred_tx_report_must_match_semantic_parameter_and_legacy_reports_mean_off() {
    fixture(|base| {
        for enabled in [false, true] {
            let mut prepared = base.prepared.clone();
            if enabled {
                prepared
                    .profile
                    .semantic_parameters
                    .insert("deferred_tx".into(), "true".into());
            }
            let request = ExecutionRequest {
                prepared: &prepared,
                ..*base
            };
            for reported in [None, Some(false), Some(true)] {
                let mut raw = report();
                if let Some(reported) = reported {
                    raw["deferred_tx"] = json!(reported);
                }
                let result = vmm::report(&serde_json::to_vec(&raw).unwrap(), &request);
                assert_eq!(result.is_ok(), reported.unwrap_or(false) == enabled);
            }
        }
    });
}

#[test]
fn checkpoint_semantics_bind_parent_and_report_and_fail_closed() {
    fixture(|base| {
        assert!(vmm::world(base).unwrap().get("checkpoint").is_none());
        let mut inputs = base.prepared.clone();
        inputs
            .profile
            .semantic_parameters
            .insert("checkpoint_at_ns".into(), "500".into());
        assert!(vmm::recorded_checkpoint(&inputs.profile).is_err());
        inputs
            .profile
            .semantic_parameters
            .insert("checkpoint_branch_seed".into(), "42".into());
        let request = ExecutionRequest {
            prepared: &inputs,
            ..*base
        };
        let checkpoint = vmm::world(&request).unwrap()["checkpoint"].clone();
        assert_eq!(checkpoint["at_ns"], 500);
        assert_eq!(checkpoint["branch_seed"], 42);
        assert_eq!(
            checkpoint["branch_identity"],
            checkpoint_protocol::branch_identity(
                checkpoint["parent_identity"].as_str().unwrap(),
                500,
                42
            )
        );
        let mut raw = report();
        assert!(vmm::report(&serde_json::to_vec(&raw).unwrap(), &request).is_err());
        raw["checkpoint"] = checkpoint;
        assert!(vmm::report(&serde_json::to_vec(&raw).unwrap(), &request).is_ok());
        raw["checkpoint"]["branch_seed"] = json!(43);
        assert!(vmm::report(&serde_json::to_vec(&raw).unwrap(), &request).is_err());
        inputs
            .profile
            .semantic_parameters
            .insert("checkpoint_at_ns".into(), "0500".into());
        assert!(vmm::recorded_checkpoint(&inputs.profile).is_err());
    });
}

// Run the engine's pure protocol tests on Darwin as well as Linux.
#[rustfmt::skip]
#[path = "../../seedbox-engine-core/src/checkpoint.rs"]
mod checkpoint_protocol;

#[test]
fn checkpoint_timer_and_tx_modes_bind_cli_and_engine_metadata() {
    fixture(|base| {
        for timebase in ["counter", "pv"] {
            let mut inputs = Vec::new();
            let mut worlds = Vec::new();
            for (page_timer, deferred_tx) in
                [(false, false), (true, false), (false, true), (true, true)]
            {
                let mut prepared = base.prepared.clone();
                prepared.profile.semantic_parameters.extend([
                    ("checkpoint_at_ns".into(), "500".into()),
                    ("checkpoint_branch_seed".into(), "42".into()),
                    ("timebase".into(), timebase.into()),
                ]);
                for (key, enabled) in [("page_timer", page_timer), ("deferred_tx", deferred_tx)] {
                    if enabled {
                        prepared
                            .profile
                            .semantic_parameters
                            .insert(key.into(), "true".into());
                    }
                }
                let world = vmm::world(&ExecutionRequest {
                    prepared: &prepared,
                    ..*base
                })
                .unwrap();
                let engine_parent = checkpoint_protocol::parent_identity(
                    &world,
                    base.seed,
                    prepared.profile.engine.hex(),
                    &checkpoint_protocol::identity_timebase(timebase, deferred_tx),
                    page_timer,
                    |_| Ok(ContentDigest::of(b"retained input").hex().to_owned()),
                )
                .unwrap();
                assert_eq!(world["checkpoint"]["parent_identity"], engine_parent);
                inputs.push(prepared);
                worlds.push(world);
            }
            for field in ["parent_identity", "branch_identity"] {
                let ids: std::collections::BTreeSet<_> = worlds
                    .iter()
                    .map(|world| world["checkpoint"][field].as_str().unwrap())
                    .collect();
                assert_eq!(ids.len(), 4, "{timebase} {field}");
            }
            for (i, prepared) in inputs.iter().enumerate() {
                let request = ExecutionRequest { prepared, ..*base };
                for (j, world) in worlds.iter().enumerate() {
                    let mut raw = report();
                    raw["timebase"] = json!(timebase);
                    raw["deferred_tx"] = json!(prepared
                        .profile
                        .semantic_parameters
                        .contains_key("deferred_tx"));
                    raw["checkpoint"] = world["checkpoint"].clone();
                    let result = vmm::report(&serde_json::to_vec(&raw).unwrap(), &request);
                    if i == j {
                        assert!(result.is_ok(), "{result:?}");
                    } else {
                        assert!(result
                            .unwrap_err()
                            .to_string()
                            .contains("VMM checkpoint identity mismatch"));
                    }
                }
            }
            let mut restored_off = inputs.pop().unwrap();
            restored_off
                .profile
                .semantic_parameters
                .remove("page_timer");
            restored_off
                .profile
                .semantic_parameters
                .remove("deferred_tx");
            assert_eq!(
                vmm::world(&ExecutionRequest {
                    prepared: &restored_off,
                    ..*base
                })
                .unwrap()["checkpoint"],
                worlds[0]["checkpoint"]
            );
        }
    });
}

#[test]
fn cli_and_engine_checkpoint_parents_match_from_content_after_relocation() {
    fixture(|base| {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("input");
        std::fs::write(&input, b"retained input").unwrap();
        let mut artifacts = BTreeMap::from([(ContentDigest::of(b"retained input"), input)]);
        let mut inputs = base.prepared.clone();
        inputs
            .profile
            .semantic_parameters
            .insert("checkpoint_at_ns".into(), "500".into());
        inputs
            .profile
            .semantic_parameters
            .insert("checkpoint_branch_seed".into(), "42".into());
        let ordinary = vmm::world(&ExecutionRequest {
            prepared: &inputs,
            artifacts: &artifacts,
            ..*base
        })
        .unwrap();
        inputs
            .profile
            .semantic_parameters
            .insert("disk_loss_sector_bytes".into(), "512".into());
        inputs
            .profile
            .semantic_parameters
            .insert("disk_loss_survival_probability".into(), "0.5".into());
        let cfg = vmm::world(&ExecutionRequest {
            prepared: &inputs,
            artifacts: &artifacts,
            ..*base
        })
        .unwrap();
        assert_ne!(
            cfg["checkpoint"]["parent_identity"],
            ordinary["checkpoint"]["parent_identity"]
        );
        let parent = checkpoint_protocol::parent_identity(
            &cfg,
            base.seed,
            inputs.profile.engine.hex(),
            "counter",
            false,
            |path| {
                Ok(ContentDigest::of(&std::fs::read(path).unwrap())
                    .hex()
                    .to_owned())
            },
        )
        .unwrap();
        assert_eq!(cfg["checkpoint"]["parent_identity"], parent);
        let mut pv_inputs = inputs.clone();
        pv_inputs
            .profile
            .semantic_parameters
            .insert("timebase".into(), "pv".into());
        let pv_world = vmm::world(&ExecutionRequest {
            prepared: &pv_inputs,
            artifacts: &artifacts,
            ..*base
        })
        .unwrap();
        let pv_parent = checkpoint_protocol::parent_identity(
            &pv_world,
            base.seed,
            inputs.profile.engine.hex(),
            "pv",
            false,
            |path| {
                Ok(ContentDigest::of(&std::fs::read(path).unwrap())
                    .hex()
                    .to_owned())
            },
        )
        .unwrap();
        assert_eq!(pv_world["checkpoint"]["parent_identity"], pv_parent);
        assert_ne!(pv_parent, parent);
        let mut deferred_worlds = Vec::new();
        for (timebase, off_world) in [(Timebase::Counter, &cfg), (Timebase::Pv, &pv_world)] {
            let mut deferred_inputs = inputs.clone();
            deferred_inputs
                .profile
                .semantic_parameters
                .insert("timebase".into(), timebase.name().into());
            deferred_inputs
                .profile
                .semantic_parameters
                .insert("deferred_tx".into(), "true".into());
            let deferred_world = vmm::world(&ExecutionRequest {
                prepared: &deferred_inputs,
                artifacts: &artifacts,
                ..*base
            })
            .unwrap();
            let deferred_parent = checkpoint_protocol::parent_identity(
                &deferred_world,
                base.seed,
                inputs.profile.engine.hex(),
                &format!("{}+deferred-tx", timebase.name()),
                false,
                |path| {
                    Ok(ContentDigest::of(&std::fs::read(path).unwrap())
                        .hex()
                        .to_owned())
                },
            )
            .unwrap();
            assert_eq!(
                deferred_world["checkpoint"]["parent_identity"],
                deferred_parent
            );
            assert_ne!(
                deferred_world["checkpoint"]["parent_identity"],
                off_world["checkpoint"]["parent_identity"]
            );
            assert_ne!(
                deferred_world["checkpoint"]["branch_identity"],
                off_world["checkpoint"]["branch_identity"]
            );
            deferred_worlds.push((deferred_inputs, deferred_world));
        }
        let relocated = dir.path().join("relocated");
        std::fs::copy(artifacts.values().next().unwrap(), &relocated).unwrap();
        *artifacts.values_mut().next().unwrap() = relocated;
        for (deferred_inputs, original) in deferred_worlds {
            let moved = vmm::world(&ExecutionRequest {
                prepared: &deferred_inputs,
                artifacts: &artifacts,
                ..*base
            })
            .unwrap();
            assert_eq!(moved["checkpoint"], original["checkpoint"]);
        }
        inputs
            .profile
            .semantic_parameters
            .insert("checkpoint_at_ns".into(), "600".into());
        inputs
            .profile
            .semantic_parameters
            .insert("checkpoint_branch_seed".into(), "43".into());
        let moved = vmm::world(&ExecutionRequest {
            prepared: &inputs,
            artifacts: &artifacts,
            ..*base
        })
        .unwrap();
        assert_eq!(moved["checkpoint"]["parent_identity"], parent);
        assert_ne!(
            moved["checkpoint"]["branch_identity"],
            cfg["checkpoint"]["branch_identity"]
        );
        inputs.profile.engine = ContentDigest::of(b"another engine");
        let changed = vmm::world(&ExecutionRequest {
            prepared: &inputs,
            artifacts: &artifacts,
            ..*base
        })
        .unwrap();
        assert_ne!(changed["checkpoint"]["parent_identity"], parent);
    });
}

#[test]
fn explicit_combined_cli_is_retained_with_its_actual_version() {
    use seedbox::{counter::CounterProfile, store::ContentStore};
    let directory = tempfile::tempdir().unwrap();
    let store = ContentStore::new(directory.path().join("store"));
    let binary = Path::new(env!("CARGO_BIN_EXE_seedbox"));
    let expected = std::process::Command::new(binary)
        .arg("--version")
        .output()
        .unwrap();
    assert!(expected.status.success());
    let version = String::from_utf8(expected.stdout).unwrap();
    let executor = vmm::VmmExecutor::new_detected(
        binary,
        &store,
        None,
        512,
        WitnessMode::Exits,
        CounterProfile::Zen,
    )
    .unwrap();
    let profile = executor.profile();
    assert_eq!(
        profile.semantic_parameters["simulator_version"],
        version.trim().strip_prefix("seedbox ").unwrap()
    );
    assert_eq!(
        profile.semantic_parameters["engine_executable_kind"],
        seedbox::engine::INTERNAL_WORKER_V1
    );
    assert_eq!(
        profile.engine,
        ContentDigest::of(&std::fs::read(binary).unwrap())
    );
}
