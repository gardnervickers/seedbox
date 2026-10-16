use super::*;
use serde_json::json;

#[test]
fn all_public_faults_translate_without_an_admission_or_alias_policy() {
    let at = Target::Name("node".into());
    let deadline = Duration::from_nanos(19);
    let connection = seedbox_control::Connection {
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
                network_faults: seedbox_control::NetworkFaults {
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
