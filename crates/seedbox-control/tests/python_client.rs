//! The shipped Python guest client speaks to a private Rust protocol peer.
//! Fixtures, responses and expectations live here; Python only calls its API.
use serde_json::{json, Value};
use std::{
    fs,
    io::{BufRead, BufReader, Write},
    os::unix::net::UnixListener,
    path::Path,
    process::Command,
    time::{Duration, Instant},
};

const DRIVER: &str = r#"
import dataclasses, json, sys
from seedbox import World, Machine
saved = {}
def decode(value):
    if isinstance(value, list): return [decode(item) for item in value]
    if isinstance(value, dict):
        if '$machine' in value: return Machine(value['$machine'])
        if '$bytes' in value: return bytes(value['$bytes'])
        if '$ref' in value: return saved[value['$ref']]
        if '$float' in value: return float(value['$float'])
        return {key: decode(item) for key, item in value.items()}
    return value
with open(sys.argv[2], encoding='utf-8') as source:
    calls = json.load(source)
world = World.connect(sys.argv[1])
results = []
try:
    for call in calls:
        if call.get('reconnect'):
            world.close()
            world = World.connect(sys.argv[1])
        try:
            value = getattr(world, call['method'])(*decode(call.get('args', [])), **decode(call.get('kwargs', {})))
            if 'save' in call: saved[call['save']] = value
            if call.get('partition'): saved[call['partition']] = value.partition
            results.append({'value': dataclasses.asdict(value) if dataclasses.is_dataclass(value) else value,
                            'last_effect': dataclasses.asdict(world.last_effect) if world.last_effect else None})
        except Exception as error:
            results.append({'error': type(error).__name__, 'message': str(error),
                            'code': getattr(error, 'code', None), 'response': getattr(error, 'response', None)})
finally:
    world.close()
print(json.dumps(results))
"#;

fn reply(id: usize, result: Value) -> Value {
    json!({"version":1,"id":id,"ok":true,"result":result,
        "requested_at_ns":10,"admitted_at_ns":20,"effect_at_ns":30,"replied_at_ns":40})
}
fn frame(response: &Value) -> Vec<u8> {
    format!("{response}\n").into_bytes()
}
fn call(method: &str, args: Value) -> Value {
    json!({"method":method,"args":args})
}

fn exercise(calls: Value, sessions: Vec<Vec<Vec<u8>>>) -> (Value, Vec<Value>) {
    let directory = tempfile::tempdir_in("/tmp").unwrap();
    let input = directory.path().join("calls.json");
    fs::write(&input, calls.to_string()).unwrap();
    let socket = directory.path().join("control.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    listener.set_nonblocking(true).unwrap();
    let server = std::thread::spawn(move || {
        let mut requests = vec![];
        for responses in sessions {
            let deadline = Instant::now() + Duration::from_secs(5);
            let mut channel = loop {
                match listener.accept() {
                    Ok((channel, _)) => break channel,
                    Err(error)
                        if error.kind() == std::io::ErrorKind::WouldBlock
                            && Instant::now() < deadline =>
                    {
                        std::thread::sleep(Duration::from_millis(2))
                    }
                    Err(error) => panic!("client did not connect: {error}"),
                }
            };
            channel.set_nonblocking(false).unwrap();
            if !responses.is_empty() {
                channel
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
            }
            let mut reader = BufReader::new(channel.try_clone().unwrap());
            for response in responses {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                requests.push(serde_json::from_str(&line).unwrap());
                channel.write_all(&response).unwrap();
                if !response.ends_with(b"\n") {
                    channel.shutdown(std::net::Shutdown::Write).unwrap();
                }
                // An empty response represents transport loss after this request.
                if response.is_empty() {
                    break;
                }
            }
            if let Ok(mut trailing) = reader.fill_buf().map(|bytes| bytes.to_vec()) {
                while !trailing.is_empty() {
                    let mut line = String::new();
                    reader.read_line(&mut line).unwrap();
                    if line.is_empty() {
                        break;
                    }
                    requests.push(serde_json::from_str(&line).unwrap());
                    trailing = reader.fill_buf().unwrap().to_vec();
                }
            }
        }
        requests
    });
    let output = Command::new("python3")
        .args(["-c", DRIVER])
        .arg(socket)
        .arg(input)
        .env(
            "PYTHONPATH",
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../python"),
        )
        .env("PYTHONDONTWRITEBYTECODE", "1")
        .output()
        .unwrap();
    let requests = server.join().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    (serde_json::from_slice(&output.stdout).unwrap(), requests)
}

#[test]
fn lifecycle_targets_timing_errors_and_sequential_ids_use_the_public_wire() {
    for method in [
        "pause",
        "resume",
        "crash",
        "reboot",
        "pause_process",
        "resume_process",
        "stop",
        "restart",
    ] {
        for target in [json!("server"), json!({"$machine":"server-vm"})] {
            let mut success = reply(1, json!({}));
            success["effect_at_ns"] = 20.into();
            success["replied_at_ns"] = 30.into();
            let mut failure = reply(2, json!({}));
            failure["ok"] = false.into();
            failure["error"] = json!({"code":"invalid_state","message":"unavailable transition"});
            let (values, requests) = exercise(
                json!([
                    call(method, json!([target])),
                    call(method, json!([target])),
                    call("now_ns", json!([]))
                ]),
                vec![vec![
                    frame(&success),
                    frame(&failure),
                    frame(&reply(3, json!({"now_ns":30}))),
                ]],
            );
            assert_eq!(values[0]["value"]["effect_at_ns"], 20);
            assert_eq!(values[0]["value"], values[0]["last_effect"]);
            assert_eq!(values[1]["error"], "FaultError");
            assert_eq!(values[1]["code"], "invalid_state");
            assert_eq!(values[1]["response"]["requested_at_ns"], 10);
            assert_eq!(values[2]["value"], 30);
            let wire = if target.is_string() {
                json!({"service":"server"})
            } else {
                json!({"machine":"server-vm"})
            };
            assert_eq!(
                requests,
                [
                    json!({"version":1,"id":1,"op":method,"target":wire}),
                    json!({"version":1,"id":2,"op":method,"target":wire}),
                    json!({"version":1,"id":3,"op":"now"})
                ]
            );
        }
    }
}

#[test]
fn handles_connection_evidence_and_trigger_receipts_survive_explicit_reconnects() {
    let connection = json!({"from":"server-vm","to":"client-vm","src_addr":"10.0.0.2","dst_addr":"10.0.0.1","src_port":80,"dst_port":4000,"incarnation":7});
    let calls = json!([
        {"method":"partition","args":[["client"],[{"$machine":"server-vm"}]],"save":"cut"},
        {"method":"heal","args":[{"$ref":"cut"}],"reconnect":true},
        {"method":"throttle","args":["server",2.5],"save":"slow"},
        call("heal",json!([{"$ref":"slow"}])),
        {"method":"clock_skew","args":["server"],"kwargs":{"max_offset":1000},"save":"skew"},
        {"method":"clock_drift","args":[{"$machine":"db-vm"}],"kwargs":{"ppb":5},"save":"drift"},
        call("heal",json!([{"$ref":"skew"}])),call("heal",json!([{"$ref":"drift"}])),
        {"method":"connections","args":["server",{"$machine":"client-vm"}],"kwargs":{"after":3}},
        {"method":"drop_connection","args":[connection],"save":"drop"},call("heal",json!([{"$ref":"drop"}])),
        {"method":"arm_console_stop","args":["server",{"$bytes":[65,0,66]},{"$machine":"server-vm"}],"kwargs":{"match_timeout":1},"save":"stop"},
        call("cancel_trigger",json!([{"$ref":"stop"}])),
        {"method":"arm_console_partition","args":[{"$machine":"server-vm"},{"$bytes":[68,79,78,69]},["server"],["client"]],"kwargs":{"match_timeout":2},"partition":"trigger-cut"},
        call("heal",json!([{"$ref":"trigger-cut"}])),call("sleep",json!([0.000000001])),call("inspect",json!([])),
    ]);
    let results = [
        json!({}),
        json!({"handle":"p3"}),
        json!({}),
        json!({"handle":"skew","offset_ns":-7}),
        json!({"handle":"drift","rate_ppb":5}),
        json!({}),
        json!({}),
        json!({"connections":[connection],"next_after":7}),
        json!({"handle":"drop"}),
        json!({"matched":0,"dropped":0}),
        json!({"trigger_id":1}),
        json!({}),
        json!({"trigger_id":2,"handle":"p1"}),
        json!({}),
        json!({}),
        json!({"services":{"server":"vm-1"},"active_handles":[]}),
    ];
    let (values, requests) = exercise(
        calls,
        vec![
            vec![frame(&reply(1, json!({"handle":"retained-p1"})))],
            results
                .into_iter()
                .enumerate()
                .map(|(i, r)| frame(&reply(i + 1, r)))
                .collect(),
        ],
    );
    assert_eq!(requests[0]["id"], 1);
    assert_eq!(requests[1]["id"], 1);
    assert_eq!(requests[1]["handle"], "retained-p1");
    assert_eq!(requests[2]["factor_permille"], 2500);
    assert_eq!(requests[4]["max_offset_ns"], 1000);
    assert_eq!(values[4]["value"]["offset_ns"], -7);
    assert_eq!(requests[5]["rate_ppb"], 5);
    assert_eq!(values[5]["value"]["rate_ppb"], 5);
    assert_eq!(
        requests[8],
        json!({"version":1,"id":8,"op":"connections","from":{"service":"server"},"to":{"machine":"client-vm"},"after":3})
    );
    assert_eq!(requests[9]["connection"], connection);
    assert_eq!(values[10]["value"], json!({"matched":0,"dropped":0}));
    assert_eq!(requests[11]["marker"], json!([65, 0, 66]));
    assert_eq!(requests[11]["match_timeout_ns"], 1_000_000_000u64);
    assert_eq!(
        requests[11]["fault"],
        json!({"action":"stop","target":{"machine":"server-vm"}})
    );
    assert_eq!(requests[12]["trigger_id"], 1);
    assert_eq!(
        requests[13]["fault"],
        json!({"action":"partition","left":[{"service":"server"}],"right":[{"service":"client"}]})
    );
    assert_eq!(requests[14]["handle"], "p1");
    assert_eq!(requests[15]["duration_ns"], 1);
    assert_eq!(values[16]["value"]["services"], json!({"server":"vm-1"}));
}

#[test]
fn shaping_keeps_absolute_policy_and_all_optional_fault_and_queue_evidence() {
    for (options, extra) in [
        (
            json!({"latency":20_000_000,"jitter":1_000_000,"loss_ppm":500_000}),
            json!({}),
        ),
        (
            json!({"latency":1_000_000,"bandwidth_bps":1_000_000,"queue_bound":12_000_000}),
            json!({"queue_counts":{"matched":2,"queue_dropped":1,"delayed_max_ns":24_000_000}}),
        ),
        (
            json!({"latency":10,"reorder_ppm":500_000,"reorder_delay":100,"duplicate_ppm":1_000_000,"burst_loss_ppm":10_000,"burst_length":4,"latency_spike_ppm":100_000,"latency_spike":200,"tcp_reset_ppm":1_000_000}),
            json!({"network_fault_counts":{"matched":3,"duplicated":3}}),
        ),
    ] {
        let (values, requests) = exercise(
            json!([{"method":"shape","args":[["client"],[{"$machine":"server-vm"}]],"kwargs":options,"save":"shape"},call("heal",json!([{"$ref":"shape"}]))]),
            vec![vec![
                frame(&reply(1, json!({"handle":"p1"}))),
                frame(&reply(2, extra.clone())),
            ]],
        );
        assert_eq!(requests[0]["from"], json!([{"service":"client"}]));
        assert_eq!(requests[0]["to"], json!([{"machine":"server-vm"}]));
        assert_eq!(requests[0]["latency_ns"], options["latency"]);
        if options.get("bandwidth_bps").is_some() {
            assert_eq!(requests[0]["queue_bound_ns"], 12_000_000);
            assert_eq!(requests[0]["bandwidth_bps"], 1_000_000);
        }
        if options.get("duplicate_ppm").is_some() {
            assert_eq!(
                requests[0]["network_faults"],
                json!({"reorder_ppm":500_000,"reorder_delay_ns":100,"duplicate_ppm":1_000_000,"burst_loss_ppm":10_000,"burst_length":4,"latency_spike_ppm":100_000,"latency_spike_ns":200,"tcp_reset_ppm":1_000_000})
            );
        } else {
            assert!(requests[0].get("network_faults").is_none());
        }
        assert_eq!(requests[1]["handle"], "p1");
        if extra != json!({}) {
            assert_eq!(values[1]["value"], extra);
        }
    }
}

#[test]
fn invalid_arguments_never_emit_a_frame() {
    let mut calls = vec![];
    for method in [
        "pause",
        "resume",
        "crash",
        "reboot",
        "stop",
        "restart",
        "pause_process",
        "resume_process",
    ] {
        for target in [
            json!(""),
            json!({"$machine":""}),
            Value::Null,
            json!("x".repeat(16384)),
        ] {
            calls.push(call(method, json!([target])));
        }
    }
    for value in [
        json!(true),
        json!(-1),
        json!({"$float":"nan"}),
        json!({"$float":"inf"}),
        json!(1e100),
        json!(0.0000000001),
    ] {
        calls.push(call("sleep", json!([value])));
    }
    for factor in [
        json!(0.5),
        json!(1000.5),
        json!(1.0001),
        json!(true),
        json!("x"),
        json!({"$float":"inf"}),
    ] {
        calls.push(call("throttle", json!(["server", factor])));
    }
    calls.push(call("partition", json!(["client", ["server"]])));
    for options in [
        json!({}),
        json!({"offset":1,"max_offset":1}),
        json!({"offset":0}),
        json!({"max_offset":0}),
        json!({"max_offset":-1}),
        json!({"offset":1.5}),
        json!({"offset":true}),
        json!({"offset":31_536_000_000_000_001u64}),
    ] {
        calls.push(json!({"method":"clock_skew","args":["server"],"kwargs":options}));
    }
    for options in [
        json!({}),
        json!({"ppb":1,"max_ppb":1}),
        json!({"ppb":0}),
        json!({"max_ppb":0}),
        json!({"max_ppb":-1}),
        json!({"ppb":1.5}),
        json!({"ppb":true}),
        json!({"ppb":-100_000_001}),
        json!({"max_ppb":100_000_001}),
    ] {
        calls.push(json!({"method":"clock_drift","args":["server"],"kwargs":options}));
    }
    for options in [
        json!({"latency":true}),
        json!({"latency":1.5}),
        json!({"latency":-1}),
        json!({"latency":10,"jitter":10}),
        json!({"latency":10,"loss_ppm":1_000_001}),
        json!({"latency":3_600_000_000_000u64,"jitter":1}),
        json!({"latency":10,"bandwidth_bps":0}),
        json!({"latency":10,"bandwidth_bps":true}),
        json!({"latency":10,"bandwidth_bps":1.5}),
        json!({"latency":10,"queue_bound":1}),
        json!({"latency":10,"bandwidth_bps":1,"queue_bound":-1}),
        json!({"latency":10,"duplicate_ppm":true}),
        json!({"latency":10,"duplicate_ppm":0.5}),
        json!({"latency":10,"duplicate_ppm":1_000_001}),
        json!({"latency":10,"reorder_ppm":1}),
        json!({"latency":10,"reorder_delay":-1}),
        json!({"latency":10,"reorder_ppm":1,"reorder_delay":3_600_000_000_000u64}),
        json!({"latency":10,"burst_loss_ppm":1}),
        json!({"latency":10,"burst_length":{"$float":"18446744073709551616"}}),
        json!({"latency":10,"latency_spike_ppm":1}),
        json!({"latency":10,"latency_spike":true}),
        json!({"latency":10,"tcp_reset_ppm":-1}),
    ] {
        calls.push(json!({"method":"shape","args":[["client"],["server"]],"kwargs":options}));
    }
    for (marker, timeout) in [
        (json!({"$bytes":[]}), 1),
        (json!({"$bytes":vec![120;257]}), 1),
        (json!("text"), 1),
        (json!({"$bytes":[120]}), 0),
        (json!({"$bytes":[120]}), -1),
    ] {
        calls.push(json!({"method":"arm_console_stop","args":["server",marker,"server"],"kwargs":{"match_timeout":timeout}}));
    }
    let (values, requests) = exercise(Value::Array(calls), vec![vec![]]);
    assert!(requests.is_empty());
    for value in values.as_array().unwrap() {
        assert_eq!(value["error"], "ValueError", "{value}");
    }
}

#[test]
fn malformed_responses_close_transport_and_never_retry() {
    let mut invalid = vec![
        vec![],
        b"{}".to_vec(),
        b"[]\n".to_vec(),
        b"not json\n".to_vec(),
        vec![b'x'; 16386],
    ];
    invalid.push(
        String::from_utf8(frame(&reply(1, json!({}))))
            .unwrap()
            .encode_utf16()
            .flat_map(u16::to_be_bytes)
            .collect(),
    );
    for (key, value) in [
        ("version", json!(0)),
        ("version", json!(2)),
        ("version", json!(true)),
        ("id", json!(2)),
        ("effect_at_ns", json!(19)),
        ("replied_at_ns", json!(-1)),
        ("result", json!([])),
        ("ok", json!("true")),
        ("effect_at_ns", Value::Null),
    ] {
        let mut response = reply(1, json!({}));
        response[key] = value;
        invalid.push(frame(&response));
    }
    for response in invalid {
        let (values, requests) = exercise(
            json!([
                call("stop", json!(["server"])),
                call("restart", json!(["server"]))
            ]),
            vec![vec![response]],
        );
        assert_eq!(requests.len(), 1);
        assert_eq!(values[0]["error"], "ProtocolError");
        assert_eq!(values[1]["error"], "ProtocolError");
        assert!(values[1]["message"].as_str().unwrap().contains("closed"));
    }
    for (method, args, options, result) in [
        (
            "partition",
            json!([["client"], ["server"]]),
            json!({}),
            json!({}),
        ),
        (
            "throttle",
            json!([{"$machine":"server"},1]),
            json!({}),
            json!({}),
        ),
        (
            "arm_console_stop",
            json!(["server",{"$bytes":[120]},"server"]),
            json!({"match_timeout":1}),
            json!({}),
        ),
        (
            "arm_console_stop",
            json!(["server",{"$bytes":[120]},"server"]),
            json!({"match_timeout":1}),
            json!({"trigger_id":true}),
        ),
        (
            "arm_console_stop",
            json!(["server",{"$bytes":[120]},"server"]),
            json!({"match_timeout":1}),
            json!({"trigger_id":0}),
        ),
        (
            "arm_console_stop",
            json!(["server",{"$bytes":[120]},"server"]),
            json!({"match_timeout":1}),
            json!({"trigger_id":65}),
        ),
    ] {
        let (values, requests) = exercise(
            json!([{"method":method,"args":args,"kwargs":options},call("stop",json!(["server"]))]),
            vec![vec![frame(&reply(1, result))]],
        );
        assert_eq!(requests.len(), 1);
        assert_eq!(values[0]["error"], "ProtocolError");
        assert_eq!(values[1]["error"], "ProtocolError");
    }
}
