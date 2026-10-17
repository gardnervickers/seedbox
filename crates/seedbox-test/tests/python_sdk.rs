//! Rust-owned contract cases against the unmodified production Python SDK.
//! Peer messages and expectations are literal values, not SDK-generated schemas.
#![cfg(unix)]
#[path = "support/python_sdk.rs"]
mod support;
use serde_json::{json, Value};
use support::*;

const LIFECYCLE: [&str; 4] = ["pause", "resume", "crash", "reboot"];
fn targets() -> [(Value, Value); 2] {
    [
        (json!("server"), json!({"service":"server"})),
        (machine("server-vm"), json!({"machine":"server-vm"})),
    ]
}
fn request(id: u64, op: &str, fields: Value) -> Value {
    let mut value = json!({"version":1,"id":id,"op":op});
    for (key, item) in fields.as_object().unwrap() {
        value[key] = item.clone();
    }
    value
}
fn call(sdk: &mut Sdk, op: &str, args: Value) -> Value {
    sdk.call(op, args, json!({}))
}
fn plain_reply() -> Vec<u8> {
    reply(1, json!({}), json!({}))
}
fn shape(sdk: &mut Sdk, kwargs: Value) -> Value {
    sdk.call("shape", json!([["client"], ["server"]]), kwargs)
}

#[test]
fn machine_lifecycle_targets_and_admission_effects() {
    for op in LIFECYCLE {
        for (target, wire) in targets() {
            let mut sdk = Sdk::open(reply(
                1,
                json!({}),
                json!({"effect_at_ns":20,"replied_at_ns":30}),
            ));
            let out = call(&mut sdk, op, json!([target]));
            effect(ok(&out), 10, 20, 20, 30);
            assert_eq!(out["is_last_effect"], true);
            let closed = sdk.close();
            assert_eq!(
                closed["requests"],
                json!([request(1, op, json!({"target":wire}))])
            );
        }
    }
}
#[test]
fn machine_lifecycle_errors_preserve_timing_and_connection() {
    for op in LIFECYCLE {
        for code in ["invalid_state", "target_unavailable"] {
            let mut sdk = Sdk::open(replies(&[
                (
                    1,
                    json!({}),
                    json!({"ok":false,"effect_at_ns":20,"error":{"code":code,"message":"unavailable transition"}}),
                ),
                (2, json!({"now_ns":30}), json!({})),
            ]));
            let failure = call(&mut sdk, op, json!(["server"]));
            error(&failure, "FaultError");
            assert_eq!(failure["error"]["code"], code);
            assert_eq!(failure["error"]["response"]["effect_at_ns"], 20);
            assert_eq!(ok(&call(&mut sdk, "now_ns", json!([]))), 30);
            assert_eq!(sdk.close()["requests"].as_array().unwrap().len(), 2);
        }
    }
}
#[test]
fn rejected_operations_without_effect_keep_the_connection_usable() {
    for code in [
        "invalid_state",
        "invalid_target",
        "invalid_argument",
        "target_unavailable",
    ] {
        let mut sdk = Sdk::open(replies(&[
            (
                1,
                json!({}),
                json!({"ok":false,"effect_at_ns":null,"error":{"code":code,"message":"not applied"}}),
            ),
            (2, json!({"now_ns":30}), json!({})),
        ]));
        let failure = call(&mut sdk, "stop", json!(["server"]));
        error(&failure, "FaultError");
        assert_eq!(failure["error"]["code"], code);
        assert_eq!(failure["error"]["response"]["effect_at_ns"], Value::Null);
        assert_eq!(ok(&call(&mut sdk, "now_ns", json!([]))), 30);
        assert_eq!(sdk.close()["requests"].as_array().unwrap().len(), 2);
    }
}

#[test]
fn machine_lifecycle_invalid_targets_do_not_send() {
    let mut sdk = Sdk::open(vec![]);
    for op in LIFECYCLE {
        for target in [
            json!(""),
            machine(""),
            Value::Null,
            json!("x".repeat(16384)),
        ] {
            error(&call(&mut sdk, op, json!([target])), "ValueError");
        }
    }
    assert_eq!(sdk.close()["requests"], json!([]));
}
#[test]
fn throttle_sends_permille_and_heals_its_handle() {
    let mut sdk = Sdk::open(replies(&[
        (1, json!({"handle":"p3"}), json!({})),
        (2, json!({}), json!({})),
    ]));
    let out = call(&mut sdk, "throttle", json!(["server", 2.5]));
    let slow = ok(&out);
    typed(slow, "Throttle");
    assert_eq!(slow["fields"]["handle"], "p3");
    effect(&slow["fields"]["effect"], 10, 20, 30, 40);
    let healed = call(&mut sdk, "heal", json!([reference("throttle")]));
    assert_eq!(ok(&healed)["fields"]["effect_at_ns"], 30);
    assert_eq!(
        sdk.close()["requests"],
        json!([
        {"version":1,"id":1,"op":"throttle","target":{"service":"server"},"factor_permille":2500},
        {"version":1,"id":2,"op":"heal","handle":"p3"}])
    );
    let mut invalid = Sdk::open(vec![]);
    for factor in [
        json!(0.5),
        json!(1000.5),
        json!(1.0001),
        json!(true),
        json!("x"),
        json!({"$float":"inf"}),
    ] {
        error(
            &call(&mut invalid, "throttle", json!(["server", factor])),
            "ValueError",
        );
    }
    assert_eq!(invalid.close()["requests"], json!([]));
    let mut missing = Sdk::open(plain_reply());
    error(
        &call(&mut missing, "throttle", json!([machine("server-vm"), 1])),
        "ProtocolError",
    );
    missing.close();
}
#[test]
fn process_pause_and_resume_wire_and_errors() {
    for op in ["pause_process", "resume_process"] {
        for (target, wire) in targets() {
            let mut sdk = Sdk::open(replies(&[
                (1, json!({}), json!({"effect_at_ns":25,"replied_at_ns":35})),
                (
                    2,
                    json!({}),
                    json!({"ok":false,"effect_at_ns":40,"replied_at_ns":50,"error":{"code":"invalid_state","message":"not paused"}}),
                ),
            ]));
            assert_eq!(
                ok(&call(&mut sdk, op, json!([target.clone()])))["fields"]["effect_at_ns"],
                25
            );
            let failure = call(&mut sdk, op, json!([target]));
            error(&failure, "FaultError");
            assert_eq!(failure["error"]["code"], "invalid_state");
            assert_eq!(
                sdk.close()["requests"],
                json!([
                    request(1, op, json!({"target":wire})),
                    request(2, op, json!({"target":wire}))
                ])
            );
            let mut invalid = Sdk::open(vec![]);
            error(&call(&mut invalid, op, json!([""])), "ValueError");
            invalid.close();
        }
    }
}
#[test]
fn optional_response_details_do_not_relax_required_version() {
    let mut sdk = Sdk::open(reply(
        1,
        json!({"now_ns":30,"extra_result_detail":"opaque"}),
        json!({"extra_response_detail":{"arbitrary":[1,null]}}),
    ));
    assert_eq!(ok(&call(&mut sdk, "now_ns", json!([]))), 30);
    sdk.close();
    for version in [0, 2] {
        let mut sdk = Sdk::open(reply(
            1,
            json!({"now_ns":30}),
            json!({"version":version,"extra_response_detail":"still invalid"}),
        ));
        error(&call(&mut sdk, "now_ns", json!([])), "ProtocolError");
        sdk.close();
    }
}
#[test]
fn sequential_calls_use_public_rpc_and_confirmed_effects() {
    let mut sdk = Sdk::open(replies(&[
        (1, json!({"handle":"opaque-7"}), json!({})),
        (2, json!({}), json!({})),
        (3, json!({}), json!({})),
        (4, json!({}), json!({})),
        (5, json!({}), json!({})),
        (6, json!({"now_ns":30}), json!({})),
    ]));
    let out = call(
        &mut sdk,
        "partition",
        json!([["client"], [machine("server-vm")]]),
    );
    let cut = ok(&out);
    typed(cut, "Partition");
    assert_eq!(cut["fields"]["handle"], "opaque-7");
    assert_eq!(cut["fields"]["effect"]["fields"]["effect_at_ns"], 30);
    ok(&call(&mut sdk, "heal", json!([reference("partition")])));
    ok(&call(&mut sdk, "stop", json!(["server"])));
    ok(&call(&mut sdk, "restart", json!(["server"])));
    assert_eq!(
        ok(&call(&mut sdk, "sleep", json!([0.000000001])))["fields"]["replied_at_ns"],
        40
    );
    assert_eq!(ok(&call(&mut sdk, "now_ns", json!([]))), 30);
    let closed = sdk.close();
    assert_eq!(
        closed["requests"],
        json!([
        {"version":1,"id":1,"op":"partition","left":[{"service":"client"}],"right":[{"machine":"server-vm"}]},
        {"version":1,"id":2,"op":"heal","handle":"opaque-7"},
        {"version":1,"id":3,"op":"stop","target":{"service":"server"}},
        {"version":1,"id":4,"op":"restart","target":{"service":"server"}},
        {"version":1,"id":5,"op":"sleep","duration_ns":1},
        {"version":1,"id":6,"op":"now"}])
    );
    assert_eq!(closed["closed"], true);
}
#[test]
fn shape_sends_absolute_nanosecond_policy_and_returns_healable_handle() {
    let mut sdk = Sdk::open(replies(&[
        (1, json!({"handle":"p1"}), json!({})),
        (2, json!({}), json!({})),
    ]));
    let out = sdk.call(
        "shape",
        json!([["client"], [machine("server-vm")]]),
        json!({"latency":20000000,"jitter":1000000,"loss_ppm":500000}),
    );
    assert_eq!(out["controller_is_world"], true);
    typed(ok(&out), "Shape");
    ok(&call(&mut sdk, "heal", json!([reference("shape")])));
    let closed = sdk.close();
    assert_eq!(
        closed["requests"][0],
        json!({"version":1,"id":1,"op":"shape","from":[{"service":"client"}],"to":[{"machine":"server-vm"}],"latency_ns":20000000,"jitter_ns":1000000,"loss_ppm":500000})
    );
    assert_eq!(closed["requests"][1]["handle"], "p1");
}
#[test]
fn clock_skew_sends_one_offset_kind_and_returns_healable_handle() {
    let mut sdk = Sdk::open(replies(&[
        (1, json!({"handle":"p1","offset_ns":-7}), json!({})),
        (2, json!({"handle":"p2","offset_ns":5}), json!({})),
        (3, json!({}), json!({})),
    ]));
    let seeded = sdk.call("clock_skew", json!(["server"]), json!({"max_offset":1000}));
    typed(ok(&seeded), "ClockSkew");
    assert_eq!(seeded["value"]["fields"]["handle"], "p1");
    assert_eq!(seeded["value"]["fields"]["offset_ns"], -7);
    // Keep the seeded Python object alive when the second call uses the same op.
    let fixed = sdk.exchange(json!({"action":"call","op":"clock_skew","args":[machine("db-vm")],"kwargs":{"offset":5},"save":"fixed"}));
    assert_eq!(ok(&fixed)["fields"]["offset_ns"], 5);
    ok(&call(&mut sdk, "heal", json!([reference("clock_skew")])));
    assert_eq!(
        sdk.close()["requests"],
        json!([
        {"version":1,"id":1,"op":"clock_skew","target":{"service":"server"},"max_offset_ns":1000},
        {"version":1,"id":2,"op":"clock_skew","target":{"machine":"db-vm"},"offset_ns":5},
        {"version":1,"id":3,"op":"heal","handle":"p1"}])
    );
}
#[test]
fn clock_skew_rejects_invalid_local_values() {
    let mut sdk = Sdk::open(vec![]);
    for kwargs in [
        json!({}),
        json!({"offset":1,"max_offset":1}),
        json!({"offset":0}),
        json!({"max_offset":0}),
        json!({"max_offset":-1}),
        json!({"offset":1.5}),
        json!({"offset":true}),
        json!({"offset":31536000000000001u64}),
    ] {
        error(
            &sdk.call("clock_skew", json!(["server"]), kwargs),
            "ValueError",
        );
    }
    assert_eq!(sdk.close()["requests"], json!([]));
}
#[test]
fn clock_drift_sends_one_rate_kind_and_returns_healable_handle() {
    let mut sdk = Sdk::open(replies(&[
        (1, json!({"handle":"p1","rate_ppb":-7}), json!({})),
        (2, json!({"handle":"p2","rate_ppb":5}), json!({})),
        (3, json!({}), json!({})),
    ]));
    let seeded = sdk.call("clock_drift", json!(["server"]), json!({"max_ppb":1000}));
    typed(ok(&seeded), "ClockDrift");
    assert_eq!(seeded["value"]["fields"]["handle"], "p1");
    assert_eq!(seeded["value"]["fields"]["rate_ppb"], -7);
    let fixed = sdk.exchange(json!({"action":"call","op":"clock_drift","args":[machine("db-vm")],"kwargs":{"ppb":5},"save":"fixed"}));
    assert_eq!(ok(&fixed)["fields"]["rate_ppb"], 5);
    ok(&call(&mut sdk, "heal", json!([reference("clock_drift")])));
    assert_eq!(
        sdk.close()["requests"],
        json!([
        {"version":1,"id":1,"op":"clock_drift","target":{"service":"server"},"max_rate_ppb":1000},
        {"version":1,"id":2,"op":"clock_drift","target":{"machine":"db-vm"},"rate_ppb":5},
        {"version":1,"id":3,"op":"heal","handle":"p1"}])
    );
}
#[test]
fn clock_drift_rejects_invalid_local_values() {
    let mut sdk = Sdk::open(vec![]);
    for kwargs in [
        json!({}),
        json!({"ppb":1,"max_ppb":1}),
        json!({"ppb":0}),
        json!({"max_ppb":0}),
        json!({"max_ppb":-1}),
        json!({"ppb":1.5}),
        json!({"ppb":true}),
        json!({"ppb":-100000001}),
        json!({"max_ppb":100000001}),
    ] {
        error(
            &sdk.call("clock_drift", json!(["server"]), kwargs),
            "ValueError",
        );
    }
    assert_eq!(sdk.close()["requests"], json!([]));
}
#[test]
fn connection_discovery_drop_and_heal_preserve_explicit_zero_evidence() {
    let connection = json!({"from":"server-vm","to":"client-vm","src_addr":"10.0.0.2","dst_addr":"10.0.0.1","src_port":80,"dst_port":4000,"incarnation":7});
    let page = json!({"connections":[connection],"next_after":7});
    let mut sdk = Sdk::open(replies(&[
        (1, page.clone(), json!({})),
        (2, json!({"handle":"p1"}), json!({})),
        (3, json!({"matched":0,"dropped":0}), json!({})),
    ]));
    assert_eq!(
        ok(&sdk.call(
            "connections",
            json!(["server", machine("client-vm")]),
            json!({"after":3})
        )),
        &page
    );
    ok(&call(&mut sdk, "drop_connection", json!([connection])));
    assert_eq!(
        ok(&call(
            &mut sdk,
            "heal",
            json!([reference("drop_connection")])
        )),
        &json!({"matched":0,"dropped":0})
    );
    let closed = sdk.close();
    assert_eq!(
        closed["requests"][0],
        json!({"version":1,"id":1,"op":"connections","from":{"service":"server"},"to":{"machine":"client-vm"},"after":3})
    );
    assert_eq!(closed["requests"][1]["connection"], connection);
    assert_eq!(closed["requests"][2]["handle"], "p1");
}
#[test]
fn shape_defaults_and_invalid_local_values() {
    let mut sdk = Sdk::open(reply(1, json!({"handle":"p1"}), json!({})));
    for kwargs in [
        json!({"latency":true}),
        json!({"latency":1.5}),
        json!({"latency":-1}),
        json!({"latency":10,"jitter":10}),
        json!({"latency":10,"loss_ppm":1000001}),
        json!({"latency":3600000000000u64,"jitter":1}),
        json!({"latency":10,"bandwidth_bps":0}),
        json!({"latency":10,"bandwidth_bps":true}),
        json!({"latency":10,"bandwidth_bps":1.5}),
        json!({"latency":10,"queue_bound":1}),
        json!({"latency":10,"bandwidth_bps":1,"queue_bound":-1}),
    ] {
        let out = shape(&mut sdk, kwargs);
        error(&out, "ValueError");
        assert_eq!(out["requests"], json!([]));
    }
    let out = shape(&mut sdk, json!({"latency":10}));
    ok(&out);
    assert_eq!(out["requests"][0]["jitter_ns"], 0);
    assert_eq!(out["requests"][0]["loss_ppm"], 0);
    sdk.close();
}
#[test]
fn bandwidth_shape_and_heal_preserve_queue_evidence() {
    let counts = json!({"matched":2,"queue_dropped":1,"delayed_max_ns":24000000});
    let mut sdk = Sdk::open(replies(&[
        (1, json!({"handle":"p1"}), json!({})),
        (2, json!({"queue_counts":counts}), json!({})),
    ]));
    ok(&shape(
        &mut sdk,
        json!({"latency":1000000,"bandwidth_bps":1000000,"queue_bound":12000000}),
    ));
    assert_eq!(
        ok(&call(&mut sdk, "heal", json!([reference("shape")]))),
        &json!({"queue_counts":counts})
    );
    let closed = sdk.close();
    assert_eq!(closed["requests"][0]["bandwidth_bps"], 1000000);
    assert_eq!(closed["requests"][0]["queue_bound_ns"], 12000000);
}
#[test]
fn each_network_fault_uses_shape_and_returns_heal_evidence() {
    for (kwargs, wire) in [
        (
            json!({"reorder_ppm":500000,"reorder_delay":100}),
            json!({"reorder_ppm":500000,"reorder_delay_ns":100}),
        ),
        (
            json!({"duplicate_ppm":1000000}),
            json!({"duplicate_ppm":1000000}),
        ),
        (
            json!({"burst_loss_ppm":10000,"burst_length":4}),
            json!({"burst_loss_ppm":10000,"burst_length":4}),
        ),
        (
            json!({"latency_spike_ppm":100000,"latency_spike":200}),
            json!({"latency_spike_ppm":100000,"latency_spike_ns":200}),
        ),
        (
            json!({"tcp_reset_ppm":1000000}),
            json!({"tcp_reset_ppm":1000000}),
        ),
    ] {
        let counts = json!({"matched":3,"duplicated":3});
        let mut sdk = Sdk::open(replies(&[
            (1, json!({"handle":"p1"}), json!({})),
            (2, json!({"network_fault_counts":counts}), json!({})),
        ]));
        let mut kwargs = kwargs;
        kwargs["latency"] = json!(10);
        ok(&shape(&mut sdk, kwargs));
        assert_eq!(
            ok(&call(&mut sdk, "heal", json!([reference("shape")]))),
            &json!({"network_fault_counts":counts})
        );
        let closed = sdk.close();
        for (key, value) in wire.as_object().unwrap() {
            assert_eq!(&closed["requests"][0]["network_faults"][key], value);
        }
        assert_eq!(closed["requests"][1]["handle"], "p1");
    }
}
#[test]
fn invalid_network_faults_are_rejected_before_sending() {
    let mut sdk = Sdk::open(reply(1, json!({"handle":"p1"}), json!({})));
    for mut kwargs in [
        json!({"duplicate_ppm":true}),
        json!({"duplicate_ppm":0.5}),
        json!({"duplicate_ppm":1000001}),
        json!({"reorder_ppm":1}),
        json!({"reorder_delay":-1}),
        json!({"reorder_ppm":1,"reorder_delay":3600000000000u64}),
        json!({"burst_loss_ppm":1}),
        json!({"burst_length":{"$integer":"18446744073709551616"}}),
        json!({"latency_spike_ppm":1}),
        json!({"latency_spike":true}),
        json!({"tcp_reset_ppm":-1}),
    ] {
        kwargs["latency"] = json!(10);
        let out = shape(&mut sdk, kwargs);
        error(&out, "ValueError");
        assert_eq!(out["requests"], json!([]));
    }
    let out = shape(&mut sdk, json!({"latency":10}));
    ok(&out);
    assert!(out["requests"][0].get("network_faults").is_none());
    sdk.close();
}

#[test]
fn graceful_reconnect_starts_new_ids_and_preserves_explicit_handles() {
    use std::{
        io::{Read, Write},
        os::unix::net::{UnixListener, UnixStream},
    };
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("control.sock");
    let listener = UnixListener::bind(&path).unwrap();
    listener.set_nonblocking(true).unwrap();
    let mut sdk = Sdk::new();
    let guard = sdk.guard.clone();
    let server = std::thread::spawn(move || {
        let mut requests = Vec::new();
        for result in [json!({"handle":"retained-p1"}), json!({})] {
            let (mut channel, _) = guard
                .wait_for(|| match listener.accept() {
                    Ok(channel) => Some(channel),
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => None,
                    Err(e) => panic!("Unix peer accept: {e}"),
                })
                .expect("Unix peer accept deadline");
            // BSD-derived hosts let an accepted socket inherit the listener's
            // non-blocking mode; these reads are blocking and bounded.
            channel.set_nonblocking(false).unwrap();
            fn read_byte(
                channel: &mut UnixStream,
                guard: &seedbox_test_time::HangGuard,
            ) -> Option<u8> {
                // Bound an actual request byte or EOF, not a timed absence used as ordering evidence.
                channel
                    .set_read_timeout(Some(
                        guard.remaining().expect("Unix peer five-second deadline"),
                    ))
                    .unwrap();
                let mut byte = [0];
                match channel.read(&mut byte).unwrap() {
                    0 => None,
                    1 => Some(byte[0]),
                    _ => unreachable!(),
                }
            }
            let mut bytes = Vec::new();
            loop {
                let byte = read_byte(&mut channel, &guard).expect("request before EOF");
                if byte == b'\n' {
                    break;
                }
                bytes.push(byte);
                assert!(bytes.len() <= 16384, "bounded Unix request");
            }
            requests.push(serde_json::from_slice::<Value>(&bytes).unwrap());
            // Bound the explicit SDK reply write under the same case deadline across reconnects.
            channel
                .set_write_timeout(Some(
                    guard.remaining().expect("Unix peer five-second deadline"),
                ))
                .unwrap();
            channel.write_all(&reply(1, result, json!({}))).unwrap();
            assert_eq!(
                read_byte(&mut channel, &guard),
                None,
                "SDK close produces EOF"
            );
        }
        requests
    });
    // Main test owns the Python caller; Rust peer runs concurrently. Always join
    // even on failed assertion so no detached peer outlives a failed test.
    let run = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let out = sdk.exchange(json!({"action":"connect","path":path}));
        assert!(out.get("error").is_none(), "{out}");
        ok(&call(
            &mut sdk,
            "partition",
            json!([["client"], ["server"]]),
        ));
        sdk.close();
        let out = sdk.exchange(json!({"action":"connect","path":path}));
        assert!(out.get("error").is_none(), "{out}");
        ok(&call(&mut sdk, "heal", json!([reference("partition")])));
        sdk.close();
    }));
    drop(sdk); // Kill/reap before joining on either success or failure.
    let requests = server
        .join()
        .expect("Unix peer must complete without failures");
    if let Err(panic) = run {
        std::panic::resume_unwind(panic);
    }
    assert_eq!(
        requests.iter().map(|r| r["id"].clone()).collect::<Vec<_>>(),
        [json!(1), json!(1)]
    );
    assert_eq!(
        requests.iter().map(|r| r["op"].clone()).collect::<Vec<_>>(),
        [json!("partition"), json!("heal")]
    );
    assert_eq!(requests[1]["handle"], "retained-p1");
}
#[test]
fn transport_loss_never_reconnects_or_retries_automatically() {
    let mut sdk = Sdk::open(vec![]);
    let lost = call(&mut sdk, "stop", json!(["server"]));
    error(&lost, "ProtocolError");
    assert_eq!(lost["closed"], true);
    let closed = call(&mut sdk, "restart", json!(["server"]));
    error(&closed, "ProtocolError");
    assert!(closed["error"]["message"]
        .as_str()
        .unwrap()
        .contains("closed"));
    assert_eq!(closed["requests"].as_array().unwrap().len(), 1);
    sdk.close();
}
#[test]
fn error_retains_code_and_raw_timing_without_implicit_retry() {
    let mut sdk = Sdk::open(reply(
        1,
        json!({}),
        json!({"ok":false,"error":{"code":"inactive_handle","message":"already healed"}}),
    ));
    let failure = call(&mut sdk, "heal", json!([{"$partition":"old"}]));
    error(&failure, "FaultError");
    assert_eq!(failure["error"]["code"], "inactive_handle");
    assert_eq!(failure["error"]["response"]["requested_at_ns"], 10);
    assert_eq!(sdk.close()["requests"].as_array().unwrap().len(), 1);
}
#[test]
fn invalid_peer_response_closes_connection() {
    // Preserve all 13 original list slots. The old responses[-3] replacement
    // deliberately makes slot 10 an actual [] result (reply() used `or {}`).
    let utf16 = String::from_utf8(plain_reply())
        .unwrap()
        .encode_utf16()
        .flat_map(u16::to_be_bytes)
        .collect();
    let mut oversized = vec![b'x'; 16385];
    oversized.push(b'\n');
    let responses = [
        utf16,
        vec![],
        b"{}".to_vec(),
        b"[]\n".to_vec(),
        b"not json\n".to_vec(),
        oversized,
        reply(2, json!({}), json!({})),
        reply(1, json!({}), json!({"version":true})),
        reply(1, json!({}), json!({"effect_at_ns":19})),
        reply(1, json!({}), json!({"replied_at_ns":-1})),
        reply(1, json!([]), json!({})),
        reply(1, json!({}), json!({"ok":"true"})),
        reply(1, json!({}), json!({"effect_at_ns":null})),
    ];
    assert_eq!(responses.len(), 13);
    for response in responses {
        let mut sdk = Sdk::open(response);
        let out = call(&mut sdk, "stop", json!(["server"]));
        error(&out, "ProtocolError");
        let closed = sdk.close();
        assert_eq!(closed["closed"], true);
        assert_eq!(closed["requests"].as_array().unwrap().len(), 1);
    }
}
#[test]
fn invalid_local_arguments_do_not_send_requests() {
    let mut sdk = Sdk::open(vec![]);
    for seconds in [
        json!(true),
        json!(-1),
        json!({"$float":"nan"}),
        json!({"$float":"inf"}),
        json!(1e100),
        json!(0.0000000001),
    ] {
        error(&call(&mut sdk, "sleep", json!([seconds])), "ValueError");
    }
    error(
        &call(&mut sdk, "partition", json!(["client", ["server"]])),
        "ValueError",
    );
    error(&call(&mut sdk, "stop", json!([""])), "ValueError");
    error(
        &call(&mut sdk, "stop", json!(["x".repeat(16384)])),
        "ValueError",
    );
    assert_eq!(sdk.close()["requests"], json!([]));
}
#[test]
fn inspection_exposes_protocol_result() {
    let mut sdk = Sdk::open(reply(
        1,
        json!({"services":{"server":"vm-1"},"active_handles":[]}),
        json!({}),
    ));
    assert_eq!(
        ok(&call(&mut sdk, "inspect", json!([])))["services"],
        json!({"server":"vm-1"})
    );
    assert_eq!(sdk.close()["requests"][0]["op"], "inspect");
}
#[test]
fn arm_and_cancel_literal_wire_and_fired_partition_handle() {
    let mut sdk = Sdk::open(replies(&[
        (1, json!({"trigger_id":1}), json!({})),
        (2, json!({}), json!({})),
        (3, json!({"trigger_id":2}), json!({})),
        (
            4,
            json!({"triggers":[{"id":2,"status":"fired","action":{"action":"partition"},"fault_id":999,"handle":"opaque-cut","effect_at_ns":30}]}),
            json!({}),
        ),
        (5, json!({}), json!({})),
    ]));
    let out = sdk.call(
        "arm_console_stop",
        json!(["server",{"$bytes":[65,0,66]},machine("server-vm")]),
        json!({"match_timeout":1}),
    );
    typed(ok(&out), "Trigger");
    ok(&call(
        &mut sdk,
        "cancel_trigger",
        json!([reference("arm_console_stop")]),
    ));
    ok(&sdk.call(
        "arm_console_partition",
        json!([machine("server-vm"),{"$bytes":[68,79,78,69]},["server"],["client"]]),
        json!({"match_timeout":2}),
    ));
    let partition = call(
        &mut sdk,
        "trigger_partition",
        json!([reference("arm_console_partition")]),
    );
    typed(ok(&partition), "Partition");
    assert_eq!(ok(&partition)["fields"]["handle"], "opaque-cut");
    effect(&ok(&partition)["fields"]["effect"], 10, 20, 30, 40);
    ok(&call(
        &mut sdk,
        "heal",
        json!([reference("trigger_partition")]),
    ));
    let closed = sdk.close();
    assert_eq!(
        closed["requests"][0],
        json!({"version":1,"id":1,"op":"arm_trigger","source":{"service":"server"},"marker":[65,0,66],"match_timeout_ns":1000000000,"fault":{"action":"stop","target":{"machine":"server-vm"}}})
    );
    assert_eq!(
        closed["requests"][1],
        json!({"version":1,"id":2,"op":"cancel_trigger","trigger_id":1})
    );
    assert_eq!(
        closed["requests"][2]["fault"],
        json!({"action":"partition","left":[{"service":"server"}],"right":[{"service":"client"}]})
    );
    assert_eq!(
        closed["requests"][3],
        json!({"version":1,"id":4,"op":"inspect"})
    );
    assert_eq!(closed["requests"][4]["handle"], "opaque-cut");
}
#[test]
fn trigger_partition_lookup_never_admits_or_heals_unfired_or_stop_triggers() {
    let mut records = vec![json!([])];
    for status in [
        "armed",
        "matched",
        "expired",
        "cancelled",
        "rejected",
        "cut_off",
    ] {
        records.push(json!([{"id":1,"status":status,"action":{"action":"partition"}}]));
    }
    records.push(json!([{"id":1,"status":"fired","action":{"action":"stop"},"fault_id":1}]));
    for records in records {
        let mut sdk = Sdk::open(replies(&[
            (1, json!({"trigger_id":1}), json!({})),
            (2, json!({"triggers":records}), json!({})),
            (3, json!({"now_ns":30}), json!({})),
        ]));
        ok(&sdk.call(
            "arm_console_partition",
            json!(["server",{"$bytes":[88]},["server"],["client"]]),
            json!({"match_timeout":1}),
        ));
        error(
            &call(
                &mut sdk,
                "trigger_partition",
                json!([reference("arm_console_partition")]),
            ),
            "ValueError",
        );
        assert_eq!(ok(&call(&mut sdk, "now_ns", json!([]))), 30);
        let closed = sdk.close();
        assert_eq!(closed["requests"].as_array().unwrap().len(), 3);
        assert_eq!(closed["requests"][1]["op"], "inspect");
    }
    let mut sdk = Sdk::open(vec![]);
    error(
        &call(&mut sdk, "trigger_partition", json!([1])),
        "TypeError",
    );
    assert_eq!(sdk.close()["requests"], json!([]));
}

#[test]
fn trigger_partition_rejects_malformed_handles_and_effects_without_reconstructing_ids() {
    let base = json!({"id":1,"status":"fired","action":{"action":"partition"},"fault_id":77,"handle":"opaque","effect_at_ns":30});
    let mut cases = vec![
        json!({"triggers":{}}),
        json!({"triggers":[false]}),
        json!({"triggers":[base.clone(),base.clone()]}),
    ];
    for patch in [
        json!({"handle":null}),
        json!({"handle":""}),
        json!({"handle":77}),
        json!({"effect_at_ns":null}),
        json!({"effect_at_ns":true}),
        json!({"effect_at_ns":19}),
        json!({"effect_at_ns":31}),
        json!({"action":null}),
        json!({"status":false}),
    ] {
        let mut record = base.clone();
        for (key, value) in patch.as_object().unwrap() {
            record[key] = value.clone();
        }
        cases.push(json!({"triggers":[record]}));
    }
    let mut missing_handle = base;
    missing_handle.as_object_mut().unwrap().remove("handle");
    cases.push(json!({"triggers":[missing_handle]}));
    for inspected in cases {
        let mut sdk = Sdk::open(replies(&[
            (1, json!({"trigger_id":1}), json!({})),
            (2, inspected, json!({})),
        ]));
        ok(&sdk.call(
            "arm_console_partition",
            json!(["server",{"$bytes":[88]},["server"],["client"]]),
            json!({"match_timeout":1}),
        ));
        error(
            &call(
                &mut sdk,
                "trigger_partition",
                json!([reference("arm_console_partition")]),
            ),
            "ProtocolError",
        );
        let closed = sdk.close();
        assert_eq!(closed["closed"], true);
        assert_eq!(closed["requests"].as_array().unwrap().len(), 2);
        assert_eq!(closed["requests"][1]["op"], "inspect");
    }
}

#[test]
fn invalid_markers_timeouts_and_reply_identity() {
    for (marker, timeout) in [
        (json!({"$bytes":[]}), 1),
        (json!({"$bytes":vec![120;257]}), 1),
        (json!("text"), 1),
        (json!({"$bytes":[120]}), 0),
        (json!({"$bytes":[120]}), -1),
    ] {
        let mut sdk = Sdk::open(vec![]);
        error(
            &sdk.call(
                "arm_console_stop",
                json!(["server", marker, "server"]),
                json!({"match_timeout":timeout}),
            ),
            "ValueError",
        );
        assert_eq!(sdk.close()["requests"], json!([]));
    }
    for result in [
        json!({}),
        json!({"trigger_id":true}),
        json!({"trigger_id":0}),
        json!({"trigger_id":65}),
    ] {
        let mut sdk = Sdk::open(reply(1, result, json!({})));
        error(
            &sdk.call(
                "arm_console_stop",
                json!(["server",{"$bytes":[120]},"server"]),
                json!({"match_timeout":1}),
            ),
            "ProtocolError",
        );
        sdk.close();
    }
}
