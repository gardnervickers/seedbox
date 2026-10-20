use super::*;
use serde_json::json;

#[test]
fn all_public_faults_translate_without_an_admission_or_alias_policy() {
    let at = Target::Name("node".into());
    let deadline = Duration::from_nanos(19);
    let connection = seedbox_engine_protocol::control::Connection {
        from: "node".into(),
        to: "peer".into(),
        src_addr: "10.0.0.1".parse().unwrap(),
        dst_addr: "10.0.0.2".parse().unwrap(),
        src_port: 123,
        dst_port: 456,
        incarnation: 7,
    };
    let rows = [
        (
            json!({"action":"stop","machine":"node"}),
            Action::Stop {
                target: at.clone(),
                deadline,
            },
            None,
        ),
        (
            json!({"action":"restart","machine":"node"}),
            Action::Restart {
                target: at.clone(),
                deadline,
            },
            None,
        ),
        (
            json!({"action":"pause_process","machine":"node"}),
            Action::PauseProcess {
                target: at.clone(),
                deadline,
            },
            None,
        ),
        (
            json!({"action":"resume_process","machine":"node"}),
            Action::ResumeProcess {
                target: at.clone(),
                deadline,
            },
            None,
        ),
        (
            json!({"action":"pause","machine":"node"}),
            Action::Pause(at.clone()),
            None,
        ),
        (
            json!({"action":"resume","machine":"node"}),
            Action::Resume(at.clone()),
            None,
        ),
        (
            json!({"action":"crash","machine":"node"}),
            Action::Crash(at.clone()),
            None,
        ),
        (
            json!({"action":"reboot","machine":"node"}),
            Action::Reboot(at.clone()),
            None,
        ),
        (
            json!({"action":"heal","handle":"p100"}),
            Action::Heal(HealTarget::Handle(FaultHandle::named("p100").unwrap())),
            None,
        ),
        (
            json!({"action":"partition","handle":"p100","left":["node"],"right":["peer"]}),
            Action::Partition {
                left: vec![at.clone()],
                right: vec![Target::Name("peer".into())],
            },
            Some("p100"),
        ),
        (
            json!({"action":"drop_connection","handle":"caller","connection":connection}),
            Action::DropConnection { connection },
            Some("caller"),
        ),
        (
            json!({"action":"shape","handle":"shape","from":["peer"],"to":["node"],"latency_ns":100,"jitter_ns":20,"loss_ppm":40,"bandwidth_bps":900,"queue_bound_ns":1000,"network_faults":{"duplicate_ppm":60}}),
            Action::Shape {
                from: vec![Target::Name("peer".into())],
                to: vec![at.clone()],
                policy: NetworkPolicy {
                    latency_ns: 100,
                    jitter_ns: 20,
                    loss_ppm: 40,
                    bandwidth_bps: Some(900),
                    queue_bound_ns: Some(1000),
                },
                network_faults: seedbox_engine_protocol::control::NetworkFaults {
                    duplicate_ppm: 60,
                    ..Default::default()
                },
            },
            Some("shape"),
        ),
        (
            json!({"action":"throttle","handle":"throttle","machine":"node","factor_permille":1200}),
            Action::Throttle {
                target: at.clone(),
                config: ExecutionConfig::new(1200).unwrap(),
            },
            Some("throttle"),
        ),
        (
            json!({"action":"clock_drift","handle":"drift","machine":"node","rate_ppb":-500}),
            Action::Drift {
                target: at.clone(),
                rate: ClockRate::new(-500).unwrap(),
            },
            Some("drift"),
        ),
        (
            json!({"action":"clock_skew","handle":"skew","machine":"node","offset_ns":-700}),
            Action::StepRealtime {
                target: at,
                nanoseconds: NonZeroI64::new(-700).unwrap(),
            },
            Some("skew"),
        ),
    ];
    for (json, expected, alias) in rows {
        let public: Public = serde_json::from_value(json).unwrap();
        let (actual, handle) = translate(&public, 19).unwrap();
        assert_eq!(actual, expected, "{}", public.name());
        assert_eq!(
            handle.as_ref().map(ToString::to_string).as_deref(),
            alias,
            "{}",
            public.name()
        );
    }
}
#[test]
fn clock_bounds_remain_bounds_for_driver_admission_and_invalid_arguments_are_refused() {
    for (json, expected) in [
        (
            json!({"action":"clock_drift","handle":"draw","machine":"node","max_rate_ppb":10000}),
            Action::DriftBounded {
                target: Target::Name("node".into()),
                max_rate_ppb: 10000,
            },
        ),
        (
            json!({"action":"clock_skew","handle":"draw","machine":"node","max_offset_ns":20000}),
            Action::StepRealtimeBounded {
                target: Target::Name("node".into()),
                max_offset_ns: 20000,
            },
        ),
    ] {
        let public: Public = serde_json::from_value(json).unwrap();
        for _ in 0..4 {
            assert_eq!(
                translate(&public, 19).unwrap(),
                (expected.clone(), Some(FaultHandle::named("draw").unwrap()))
            );
        }
    }
    for json in [
        json!({"action":"clock_drift","handle":"draw","machine":"node","max_rate_ppb":0}),
        json!({"action":"clock_skew","handle":"draw","machine":"node","offset_ns":0}),
        json!({"action":"clock_drift","handle":"draw","machine":"node","rate_ppb":5,"max_rate_ppb":7}),
        json!({"action":"clock_skew","handle":"draw","machine":"node"}),
        json!({"action":"throttle","handle":"draw","machine":"node","factor_permille":18446744073709551615u64}),
        json!({"action":"heal","handle":""}),
    ] {
        assert!(translate(&serde_json::from_value(json).unwrap(), 19).is_err());
    }
}

#[test]
fn preview_validation_and_execution_compile_the_same_targets_for_every_public_action() {
    use crate::driver::fixture::owned::network_fixture::{net, setup};
    use crate::driver::fixture::owned::{advance, key, spec};
    use crate::driver::network_test_frames as frames;
    use crate::driver::{FaultTarget, PreparedOutcome};
    use crate::pv::Response;
    use std::collections::BTreeSet;

    let names: Vec<String> = (0..3).map(|i| format!("fixture-{i}")).collect();
    let rows = [
        json!({"action":"stop","machine":"fixture-1"}),
        json!({"action":"restart","machine":"fixture-1"}),
        json!({"action":"pause_process","machine":"fixture-1"}),
        json!({"action":"resume_process","machine":"fixture-1"}),
        json!({"action":"pause","machine":"fixture-1"}),
        json!({"action":"resume","machine":"fixture-1"}),
        json!({"action":"crash","machine":"fixture-1"}),
        json!({"action":"reboot","machine":"fixture-1"}),
        json!({"action":"partition","handle":"p","left":["fixture-2"],"right":["fixture-1","fixture-0"]}),
        json!({"action":"shape","handle":"p","from":["fixture-2"],"to":["fixture-1","fixture-0"],"latency_ns":20,"jitter_ns":0,"loss_ppm":0}),
        json!({"action":"throttle","handle":"p","machine":"fixture-1","factor_permille":1500}),
        json!({"action":"clock_drift","handle":"p","machine":"fixture-1","rate_ppb":-50}),
        json!({"action":"clock_drift","handle":"p","machine":"fixture-1","max_rate_ppb":50}),
        json!({"action":"clock_skew","handle":"p","machine":"fixture-1","offset_ns":-50}),
        json!({"action":"clock_skew","handle":"p","machine":"fixture-1","max_offset_ns":50}),
        json!({"action":"heal","handle":"prelude"}),
        json!({"action":"drop_connection","handle":"p","connection":{
            "from":"fixture-1","to":"fixture-2","src_addr":"10.0.0.2","dst_addr":"10.0.0.3",
            "src_port":4000,"dst_port":80,"incarnation":1
        }}),
    ];
    for value in rows {
        let public: Public = serde_json::from_value(value).unwrap();
        let specs = (0..3)
            .map(|i| {
                let mut sends = vec![(0, Response::Ready)];
                if i == 1 {
                    sends.push((
                        1,
                        Response::TransmitFrame {
                            bytes: frames::tcp(1, 2, (4000, 80), 7, 0, 2),
                        },
                    ));
                }
                spec(i, true, sends)
            })
            .collect();
        let mut driver = net(specs, setup(100), 1);
        advance(&mut driver, 30);
        let prelude = match &public {
            Public::Restart { .. } => Some(json!({"action":"stop","machine":"fixture-1"})),
            Public::ResumeProcess { .. } => {
                Some(json!({"action":"pause_process","machine":"fixture-1"}))
            }
            Public::Resume { .. } => Some(json!({"action":"pause","machine":"fixture-1"})),
            Public::Reboot { .. } => Some(json!({"action":"crash","machine":"fixture-1"})),
            Public::Heal { .. } => Some(
                json!({"action":"throttle","handle":"prelude","machine":"fixture-1","factor_permille":1500}),
            ),
            _ => None,
        };
        let submit = |driver: &mut crate::driver::Driver<_>, name: &str, public: &Public| {
            let (action, handle) = translate(public, 100).unwrap();
            driver
                .with_barrier(|b| {
                    let prepared = match handle {
                        Some(handle) => b.prepare_input_with_handle(key(name), action, handle),
                        None => b.prepare_input(key(name), action),
                    }?;
                    assert!(
                        matches!(prepared.outcome(), PreparedOutcome::Admitted(_)),
                        "{public:?}: {:?}",
                        prepared.outcome()
                    );
                    b.commit_input(&prepared)?;
                    Ok::<_, crate::driver::Error<std::io::Error>>(prepared)
                })
                .unwrap()
        };
        if let Some(prelude) = prelude {
            submit(
                &mut driver,
                "prelude",
                &serde_json::from_value(prelude).unwrap(),
            );
            advance(&mut driver, 60);
        }
        let mut preview = action_targets(&public, &names, 10).unwrap();
        if matches!(public, Public::Heal { .. }) {
            // A heal's pure target set comes from the observed original fault.
            assert!(preview.machines.is_empty() && preview.links.is_empty());
            preview = action_targets(&serde_json::from_value(json!({"action":"throttle","handle":"prelude","machine":"fixture-1","factor_permille":1500})).unwrap(), &names, 10).unwrap();
        }
        let prepared = submit(&mut driver, "test", &public);
        let PreparedOutcome::Admitted(record) = prepared.outcome() else {
            unreachable!()
        };
        let mut machines = BTreeSet::new();
        let mut links = BTreeSet::new();
        match &record.target {
            FaultTarget::Machine { machine, .. } => {
                machines.insert(names[machine.0].clone());
            }
            FaultTarget::Links { left, right } => {
                for a in left {
                    for b in right {
                        links.insert((names[a.0].clone(), names[b.0].clone()));
                        links.insert((names[b.0].clone(), names[a.0].clone()));
                    }
                }
            }
            FaultTarget::DirectedLinks { from, to } => {
                for a in from {
                    for b in to {
                        links.insert((names[a.0].clone(), names[b.0].clone()));
                    }
                }
            }
            FaultTarget::Connection { connection } => {
                links.insert((connection.from.clone(), connection.to.clone()));
            }
        }
        for (a, b) in &links {
            machines.insert(a.clone());
            machines.insert(b.clone());
        }
        assert_eq!(
            preview,
            ActionTargets {
                machines: machines.into_iter().collect(),
                links: links.into_iter().collect()
            },
            "{}",
            public.name()
        );
    }
}

#[test]
fn validation_and_admission_reject_the_same_invalid_target_shapes() {
    use crate::driver::fixture::owned::network_fixture::{initial, net, setup};
    use crate::driver::fixture::owned::{advance, key};
    use crate::driver::PreparedOutcome;
    let names: Vec<String> = (0..3).map(|i| format!("fixture-{i}")).collect();
    let rows = [
        json!({"action":"pause","machine":"missing"}),
        json!({"action":"partition","handle":"p","left":[],"right":["fixture-1"]}),
        json!({"action":"partition","handle":"p","left":["fixture-1","fixture-1"],"right":["fixture-2"]}),
        json!({"action":"partition","handle":"p","left":["fixture-1"],"right":["fixture-1"]}),
        json!({"action":"partition","handle":"p","left":["fixture-1"],"right":["missing"]}),
        json!({"action":"shape","handle":"p","from":["fixture-1"],"to":["fixture-2"],"latency_ns":9,"jitter_ns":0,"loss_ppm":0}),
        json!({"action":"shape","handle":"p","from":["fixture-1"],"to":["fixture-1"],"latency_ns":20,"jitter_ns":0,"loss_ppm":0}),
        json!({"action":"shape","handle":"p","from":["fixture-1"],"to":["fixture-2"],"latency_ns":20,"jitter_ns":20,"loss_ppm":0}),
        json!({"action":"shape","handle":"p","from":["fixture-1"],"to":["fixture-2"],"latency_ns":20,"jitter_ns":0,"loss_ppm":1000001}),
        json!({"action":"drop_connection","handle":"p","connection":{
            "from":"fixture-1","to":"fixture-2","src_addr":"10.0.0.2","dst_addr":"10.0.0.3",
            "src_port":0,"dst_port":80,"incarnation":1
        }}),
        json!({"action":"drop_connection","handle":"p","connection":{
            "from":"missing","to":"fixture-2","src_addr":"10.0.0.2","dst_addr":"10.0.0.3",
            "src_port":4000,"dst_port":80,"incarnation":1
        }}),
    ];
    let mut driver = net(initial(), setup(100), 1);
    advance(&mut driver, 30);
    for value in rows {
        let public: Public = serde_json::from_value(value).unwrap();
        assert!(action_targets(&public, &names, 10).is_err(), "{public:?}");
        let (action, handle) = translate(&public, 100).unwrap();
        let result = driver.with_barrier(|b| match handle {
            Some(handle) => b.prepare_input_with_handle(key("invalid"), action, handle),
            None => b.prepare_input(key("invalid"), action),
        });
        assert!(
            match result {
                Err(_) => true,
                Ok(prepared) => matches!(prepared.outcome(), PreparedOutcome::Rejected(_)),
            },
            "{public:?}"
        );
    }
}
