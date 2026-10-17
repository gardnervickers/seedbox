use super::*;
use serde_json::json;
use std::{io::BufRead, thread};

/// Run `client` against a fake controller that answers each request line with
/// the next reply from `answer`; returns the client's result and every
/// request the controller received.
fn session<T: Send + 'static>(
    answer: impl Fn(&Value) -> Option<String> + Send + 'static,
    client: impl FnOnce(&mut World) -> T + Send + 'static,
) -> (T, Vec<String>) {
    let (near, far) = UnixStream::pair().unwrap();
    let server = thread::spawn(move || {
        let mut reader = BufReader::new(far.try_clone().unwrap());
        let mut writer = far;
        let mut seen = Vec::new();
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line).unwrap_or(0) == 0 {
                break;
            }
            let request: Value = serde_json::from_str(&line).unwrap();
            seen.push(line);
            match answer(&request) {
                Some(reply) => {
                    // An unterminated reply is followed by a disconnect.
                    if writer.write_all(reply.as_bytes()).is_err() || !reply.ends_with('\n') {
                        break;
                    }
                }
                None => break,
            }
        }
        seen
    });
    let mut world = World::from_stream(near).unwrap();
    let result = client(&mut world);
    world.close();
    (result, server.join().unwrap())
}

fn reply(request: &Value, result: Value) -> String {
    let mut value = json!({
        "version": 1, "id": request["id"], "ok": true,
        "requested_at_ns": 10, "admitted_at_ns": 11, "effect_at_ns": 12, "replied_at_ns": 13,
        "result": result,
    });
    if request["op"] == "now" {
        value["result"] = json!({"now_ns": 12});
    }
    format!("{value}\n")
}

#[test]
fn requests_are_one_compact_line_with_ids_from_one() {
    let (result, seen) = session(
        |request| Some(reply(request, json!({}))),
        |world| {
            world.sleep(Duration::from_millis(1500)).unwrap();
            world.stop("n1").unwrap();
            world.crash(Machine("n2".into())).unwrap();
            world.now_ns().unwrap()
        },
    );
    assert_eq!(result, 12);
    assert_eq!(
        seen,
        [
            "{\"version\":1,\"id\":1,\"op\":\"sleep\",\"duration_ns\":1500000000}\n",
            "{\"version\":1,\"id\":2,\"op\":\"stop\",\"target\":{\"service\":\"n1\"}}\n",
            "{\"version\":1,\"id\":3,\"op\":\"crash\",\"target\":{\"machine\":\"n2\"}}\n",
            "{\"version\":1,\"id\":4,\"op\":\"now\"}\n",
        ]
    );
}

#[test]
fn every_operation_matches_the_python_wire_shape() {
    let (_, seen) = session(
        |request| {
            let result = match request["op"].as_str().unwrap() {
                "arm_trigger" => json!({"trigger_id": 3, "handle": "p7"}),
                "clock_skew" => json!({"handle": "k1", "offset_ns": -5}),
                "clock_drift" => json!({"handle": "d1", "rate_ppb": 9}),
                "connections" => json!({"connections": [], "next_after": 4}),
                _ => json!({"handle": "h1"}),
            };
            Some(reply(request, result))
        },
        |world| {
            let cut = world.partition(["n1", "n2"], ["n3"]).unwrap();
            world.heal(&cut).unwrap();
            world
                .shape(
                    ["n1"],
                    ["n2"],
                    5_000_000,
                    ShapeOptions {
                        jitter: 1,
                        bandwidth_bps: Some(8),
                        queue_bound: Some(0),
                        faults: NetworkFaults {
                            duplicate_ppm: 3,
                            ..NetworkFaults::default()
                        },
                        ..ShapeOptions::default()
                    },
                )
                .unwrap();
            world
                .shape(["n1"], ["n2"], 2, ShapeOptions::default())
                .unwrap();
            assert_eq!(world.connections("c1", "n1", 2).unwrap().next_after, 4);
            assert_eq!(
                world.clock_skew("n1", Amount::Fixed(-5)).unwrap().offset_ns,
                -5
            );
            world.clock_drift("n1", Amount::Bound(9)).unwrap();
            world.throttle("n1", "1.5GHz").unwrap();
            let trigger = world
                .arm_console_partition("c1", b"@@END ", ["c1"], ["n1"], Duration::from_secs(2))
                .unwrap();
            assert_eq!(trigger.id, 3);
            assert_eq!(trigger.partition.as_ref().unwrap().handle, "p7");
            world.cancel_trigger(&trigger).unwrap();
            world.pause_process("n1").unwrap();
            world.resume_process("n1").unwrap();
            world.restart("n1").unwrap();
            world.inspect().unwrap();
        },
    );
    let lines: Vec<Value> = seen
        .iter()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(
        lines[0],
        json!({"version":1,"id":1,"op":"partition","left":[{"service":"n1"},{"service":"n2"}],"right":[{"service":"n3"}]})
    );
    assert_eq!(
        lines[1],
        json!({"version":1,"id":2,"op":"heal","handle":"h1"})
    );
    assert_eq!(
        lines[2],
        json!({"version":1,"id":3,"op":"shape","from":[{"service":"n1"}],"to":[{"service":"n2"}],
               "latency_ns":5_000_000,"jitter_ns":1,"loss_ppm":0,"bandwidth_bps":8,"queue_bound_ns":0,
               "network_faults":{"reorder_ppm":0,"reorder_delay_ns":0,"duplicate_ppm":3,"burst_loss_ppm":0,
                                 "burst_length":0,"latency_spike_ppm":0,"latency_spike_ns":0,"tcp_reset_ppm":0}})
    );
    assert_eq!(
        lines[3],
        json!({"version":1,"id":4,"op":"shape","from":[{"service":"n1"}],"to":[{"service":"n2"}],
               "latency_ns":2,"jitter_ns":0,"loss_ppm":0})
    );
    assert_eq!(
        lines[4],
        json!({"version":1,"id":5,"op":"connections","from":{"service":"c1"},"to":{"service":"n1"},"after":2})
    );
    assert_eq!(
        lines[5],
        json!({"version":1,"id":6,"op":"clock_skew","target":{"service":"n1"},"offset_ns":-5})
    );
    assert_eq!(
        lines[6],
        json!({"version":1,"id":7,"op":"clock_drift","target":{"service":"n1"},"max_rate_ppb":9})
    );
    assert_eq!(
        lines[7],
        json!({"version":1,"id":8,"op":"throttle","target":{"service":"n1"},"speed_hz":1_500_000_000u64})
    );
    assert_eq!(
        lines[8],
        json!({"version":1,"id":9,"op":"arm_trigger","source":{"service":"c1"},
               "marker":[64,64,69,78,68,32],"match_timeout_ns":2_000_000_000u64,
               "fault":{"action":"partition","left":[{"service":"c1"}],"right":[{"service":"n1"}]}})
    );
    assert_eq!(
        lines[9],
        json!({"version":1,"id":10,"op":"cancel_trigger","trigger_id":3})
    );
    assert_eq!(lines[13]["op"], "inspect");
}

#[test]
fn refusals_keep_the_structured_error_and_the_session() {
    let (result, seen) = session(
        |request| {
            if request["id"] == 1 {
                Some(format!(
                    "{}\n",
                    json!({"version":1,"id":1,"ok":false,"requested_at_ns":1,"admitted_at_ns":2,
                           "effect_at_ns":3,"replied_at_ns":4,
                           "error":{"code":"invalid_target","message":"no such machine"}})
                ))
            } else {
                Some(reply(request, json!({})))
            }
        },
        |world| {
            let error = world.stop("nx").unwrap_err();
            let effect = world.last_effect.unwrap();
            assert_eq!(effect.replied_at_ns, 4);
            let Error::Fault { code, response, .. } = error else {
                panic!("expected a fault, got {error:?}")
            };
            assert_eq!(code, "invalid_target");
            assert_eq!(response["error"]["message"], "no such machine");
            world.stop("n1").is_ok()
        },
    );
    assert!(result);
    assert_eq!(seen.len(), 2);
}

fn broken(response: &'static str) -> Error {
    let (error, _) = session(
        move |_| Some(response.into()),
        |world| {
            let error = world.stop("n1").unwrap_err();
            assert!(world.is_closed());
            assert!(
                matches!(world.stop("n1"), Err(Error::Protocol(m)) if m == "control connection is closed")
            );
            error
        },
    );
    error
}

#[test]
fn invalid_responses_close_the_session() {
    let timing = r#""requested_at_ns":1,"admitted_at_ns":2,"effect_at_ns":3,"replied_at_ns":4"#;
    for (response, message) in [
        (
            format!("{{\"version\":2,\"id\":1,\"ok\":true,{timing},\"result\":{{}}}}\n"),
            "unsupported control response version",
        ),
        (
            format!("{{\"version\":1,\"id\":7,\"ok\":true,{timing},\"result\":{{}}}}\n"),
            "control response correlation mismatch",
        ),
        (
            format!("{{\"version\":1,\"id\":1,\"ok\":1,{timing},\"result\":{{}}}}\n"),
            "control response correlation mismatch",
        ),
        (
            "{\"version\":1,\"id\":1,\"ok\":true,\"requested_at_ns\":5,\"admitted_at_ns\":2,\"effect_at_ns\":3,\"replied_at_ns\":4,\"result\":{}}\n".into(),
            "invalid control effect timing",
        ),
        (
            format!("{{\"version\":1,\"id\":1,\"ok\":true,{timing},\"result\":[]}}\n"),
            "invalid control result",
        ),
        (
            format!("{{\"version\":1,\"id\":1,\"ok\":false,{timing},\"error\":{{\"code\":1}}}}\n"),
            "invalid control error",
        ),
        (
            format!("{{\"version\":1,\"id\":1,\"ok\":true,{timing}}}"),
            "missing or oversized control response",
        ),
        (
            format!("{}\n", " ".repeat(MAX_FRAME)),
            "missing or oversized control response",
        ),
    ] {
        let leaked: &'static str = Box::leak(response.into_boxed_str());
        match broken(leaked) {
            Error::Protocol(found) => assert_eq!(found, message, "{leaked}"),
            other => panic!("{leaked}: {other:?}"),
        }
    }
    assert!(
        matches!(broken("not json\n"), Error::Protocol(m) if m.starts_with("control transport failed"))
    );
}

#[test]
fn a_disconnected_controller_is_a_protocol_error() {
    let (error, _) = session(|_| None, |world| world.restart("n1").unwrap_err());
    assert!(matches!(error, Error::Protocol(m) if m == "missing or oversized control response"));
}

#[test]
fn results_without_their_handles_close_the_session() {
    let (error, _) = session(
        |request| Some(reply(request, json!({"handle": ""}))),
        |world| world.partition(["a"], ["b"]).unwrap_err(),
    );
    assert!(
        matches!(error, Error::Protocol(m) if m == "partition response lacks an opaque handle")
    );
    let (error, _) = session(
        |request| Some(reply(request, json!({"trigger_id": 65}))),
        |world| {
            world
                .arm_console_stop("c1", b"x", "n1", Duration::from_secs(1))
                .unwrap_err()
        },
    );
    assert!(matches!(error, Error::Protocol(m) if m == "arm response lacks a bounded trigger ID"));
    let (error, _) = session(
        |request| {
            let mut reply = reply(request, json!({}));
            reply = reply.replace("\"now_ns\":12", "\"now_ns\":11");
            Some(reply)
        },
        |world| world.now_ns().unwrap_err(),
    );
    assert!(matches!(error, Error::Protocol(m) if m == "invalid simulated time result"));
}

#[test]
fn invalid_arguments_are_refused_before_sending() {
    let (_, seen) = session(
        |request| Some(reply(request, json!({}))),
        |world| {
            for result in [
                world.stop("").map(|_| ()),
                world.partition(Vec::<&str>::new(), [""]).map(|_| ()),
                world
                    .shape(
                        ["a"],
                        ["b"],
                        5,
                        ShapeOptions {
                            jitter: 5,
                            ..Default::default()
                        },
                    )
                    .map(|_| ()),
                world
                    .shape(
                        ["a"],
                        ["b"],
                        5,
                        ShapeOptions {
                            queue_bound: Some(1),
                            ..Default::default()
                        },
                    )
                    .map(|_| ()),
                world
                    .shape(
                        ["a"],
                        ["b"],
                        5,
                        ShapeOptions {
                            faults: NetworkFaults {
                                reorder_ppm: 1,
                                ..Default::default()
                            },
                            ..Default::default()
                        },
                    )
                    .map(|_| ()),
                world.clock_skew("a", Amount::Fixed(0)).map(|_| ()),
                world.clock_skew("a", Amount::Fixed(i64::MIN)).map(|_| ()),
                world
                    .clock_drift("a", Amount::Bound(MAX_CLOCK_RATE_PPB + 1))
                    .map(|_| ()),
                world.throttle("a", "0.5Hz").map(|_| ()),
                world.throttle("a", "999Hz").map(|_| ()),
                world.throttle("a", "fast").map(|_| ()),
                world
                    .arm_console_stop("c", b"", "a", Duration::from_secs(1))
                    .map(|_| ()),
                world
                    .arm_console_stop("c", b"x", "a", Duration::ZERO)
                    .map(|_| ()),
                world.sleep(Duration::MAX).map(|_| ()),
                world.stop("x".repeat(MAX_FRAME)).map(|_| ()),
            ] {
                assert!(matches!(result, Err(Error::Invalid(_))), "{result:?}");
            }
            assert!(!world.is_closed());
            world.stop("a").unwrap();
        },
    );
    // Refused requests used no ID: the first sent request is still ID 1.
    assert_eq!(
        seen,
        ["{\"version\":1,\"id\":1,\"op\":\"stop\",\"target\":{\"service\":\"a\"}}\n"]
    );
}

#[test]
fn frequencies_parse_as_whole_hz() {
    for (text, hz) in [
        ("500MHz", 500_000_000),
        ("1.5GHz", 1_500_000_000),
        (" 2GHz ", 2_000_000_000),
        ("1.50GHz", 1_500_000_000),
        ("1000Hz", 1_000),
        ("1THz", 1_000_000_000_000),
        ("0500MHz", 500_000_000),
    ] {
        assert_eq!(speed_hz(&Speed::from(text)).unwrap(), hz, "{text}");
    }
    for text in [
        "1.Hz",
        ".5GHz",
        "1.5.0GHz",
        "5 GHz",
        "2THz",
        "1e9Hz",
        "1.0000000001kHz",
    ] {
        assert!(speed_hz(&Speed::from(text)).is_err(), "{text}");
    }
    assert!(speed_hz(&Speed::Hz(999)).is_err());
}

#[test]
fn non_ascii_names_are_escaped_like_python() {
    assert_eq!(
        ascii_json("{\"service\":\"né😀\"}"),
        "{\"service\":\"n\\u00e9\\ud83d\\ude00\"}"
    );
}
