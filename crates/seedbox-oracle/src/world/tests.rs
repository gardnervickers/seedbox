use super::*;
use std::{io::BufRead, os::unix::net::UnixListener, thread};

/// A stand-in controller: answers each request with `reply(request)`, and
/// returns every request it read once the client disconnects.
fn serve(
    reply: impl Fn(&Value) -> Value + Send + 'static,
) -> (tempfile::TempDir, thread::JoinHandle<Vec<Value>>) {
    // macOS bounds Unix socket paths, so keep them short.
    let directory = tempfile::Builder::new()
        .prefix("sbx-ctl-")
        .tempdir_in("/tmp")
        .unwrap();
    let listener = UnixListener::bind(directory.path().join("control.sock")).unwrap();
    let server = thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        let mut writer = stream.try_clone().unwrap();
        let mut requests = Vec::new();
        for line in BufReader::new(stream).lines() {
            let Ok(line) = line else { break };
            let request: Value = serde_json::from_str(&line).unwrap();
            let mut response = reply(&request);
            if response.get("id").is_none() {
                response["id"] = request["id"].clone();
            }
            requests.push(request);
            writeln!(writer, "{response}").unwrap();
        }
        requests
    });
    (directory, server)
}

fn ok(result: Value) -> Value {
    json!({"version": 1, "ok": true, "requested_at_ns": 1, "admitted_at_ns": 1,
           "effect_at_ns": 2, "replied_at_ns": 3, "result": result})
}

#[test]
fn actions_become_rpc_calls_and_handles_map_to_the_worlds() {
    let (directory, server) = serve(|request| match request["op"].as_str().unwrap() {
        "now" => ok(json!({"now_ns": 2})),
        "partition" => ok(json!({"handle": "p1"})),
        "clock_skew" => ok(json!({"handle": "p2", "offset_ns": 5})),
        "heal" if request["handle"] == "p9" => {
            json!({"version": 1, "ok": false, "error": {"code": "invalid_target", "message": "no such handle"}})
        }
        _ => ok(json!({})),
    });
    let mut world = GuestWorld::connect_to(directory.path().join("control.sock")).unwrap();
    assert_eq!(world.now_ns(), Ok(2));
    world.sleep(Duration::from_millis(250)).unwrap();
    let cut = Action::Partition {
        handle: "cut".into(),
        left: vec!["n1".into()],
        right: vec!["n2".into(), "n3".into()],
    };
    world.apply(&cut).unwrap();
    // A name in force is refused before anything is sent.
    assert!(world.apply(&cut).is_err());
    world
        .apply(&Action::ClockSkew {
            handle: "skew".into(),
            machine: "n2".into(),
            offset_ns: Some(-5),
            max_offset_ns: None,
        })
        .unwrap();
    world
        .apply(&Action::Stop {
            machine: "n3".into(),
        })
        .unwrap();
    assert_eq!(world.handles().len(), 2);
    world
        .apply(&Action::Heal {
            handle: "cut".into(),
        })
        .unwrap();
    assert!(world
        .apply(&Action::Heal {
            handle: "cut".into()
        })
        .unwrap_err()
        .0
        .contains("no fault with handle"));
    // A refusal names the world's error and keeps the connection usable.
    world.handles.insert("stale".into(), "p9".into());
    let refused = world
        .apply(&Action::Heal {
            handle: "stale".into(),
        })
        .unwrap_err();
    assert_eq!(refused.0, "heal: invalid_target: no such handle");
    assert!(!world.handles().contains_key("stale"));
    assert_eq!(world.now_ns(), Ok(2));
    // Callers of the raw RPC cannot override its framing.
    let forged = world.control().call("now", json!({"id": 99})).unwrap_err();
    assert_eq!(forged.0, "now arguments may not set \"id\"");
    drop(world);
    let requests = server.join().unwrap();
    let ops: Vec<_> = requests.iter().map(|r| r["op"].as_str().unwrap()).collect();
    assert_eq!(
        ops,
        [
            "now",
            "sleep",
            "partition",
            "clock_skew",
            "stop",
            "heal",
            "heal",
            "now"
        ]
    );
    let ids: Vec<_> = requests.iter().map(|r| r["id"].as_u64().unwrap()).collect();
    assert_eq!(ids, (1..=8).collect::<Vec<_>>());
    assert_eq!(requests[1]["duration_ns"], 250_000_000);
    assert_eq!(
        requests[2],
        json!({"version": 1, "id": 3, "op": "partition", "left": [{"machine": "n1"}],
               "right": [{"machine": "n2"}, {"machine": "n3"}]})
    );
    assert_eq!(requests[3]["offset_ns"], -5);
    assert_eq!(requests[4]["target"], json!({"machine": "n3"}));
    assert_eq!(requests[5]["handle"], "p1");
}

#[test]
fn an_uncorrelated_response_closes_the_connection() {
    let (directory, server) = serve(|_| {
        let mut response = ok(json!({"now_ns": 1}));
        response["id"] = json!(99);
        response
    });
    let mut world = GuestWorld::connect_to(directory.path().join("control.sock")).unwrap();
    assert!(world.now_ns().unwrap_err().0.contains("correlation"));
    assert_eq!(
        world.now_ns().unwrap_err().0,
        "control connection is closed"
    );
    drop(world);
    assert_eq!(server.join().unwrap().len(), 1);
}
