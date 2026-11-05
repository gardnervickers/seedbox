use super::*;
use std::{io::BufRead, os::unix::net::UnixListener, thread};

/// A stand-in controller: answers each request with `reply(request)`, and
/// returns every request it read once the client disconnects.
fn serve(
    reply: impl Fn(&Value) -> Value + Send + Sync + 'static,
) -> (tempfile::TempDir, thread::JoinHandle<Vec<Value>>) {
    let (directory, server) = serve_connections(1, reply);
    let server = thread::spawn(move || server.join().unwrap().pop().unwrap());
    (directory, server)
}

/// [`serve`] for `connections` connections, each answered on its own
/// thread; returns their requests in accept order.
fn serve_connections(
    connections: usize,
    reply: impl Fn(&Value) -> Value + Send + Sync + 'static,
) -> (tempfile::TempDir, thread::JoinHandle<Vec<Vec<Value>>>) {
    // macOS bounds Unix socket paths, so keep them short.
    let directory = tempfile::Builder::new()
        .prefix("sbx-ctl-")
        .tempdir_in("/tmp")
        .unwrap();
    let listener = UnixListener::bind(directory.path().join("control.sock")).unwrap();
    let reply = std::sync::Arc::new(reply);
    let server = thread::spawn(move || {
        let servers: Vec<_> = (0..connections)
            .map(|_| {
                let (stream, _) = listener.accept().unwrap();
                let reply = reply.clone();
                thread::spawn(move || {
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
                })
            })
            .collect();
        servers.into_iter().map(|s| s.join().unwrap()).collect()
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
fn disk_and_scheduler_faults_use_the_oracle_operations() {
    use crate::control::{DiskEffect, DiskOp, DiskScope};
    let (directory, server) = serve(|request| {
        let handle = format!("p{}", request["id"]);
        ok(json!({ "handle": handle }))
    });
    let mut world = GuestWorld::connect_to(directory.path().join("control.sock")).unwrap();
    for action in [
        Action::DiskError {
            handle: "eio".into(),
            machine: "db".into(),
            op: DiskOp::Write,
            ppm: 10,
            effect: DiskEffect::Eio,
            scope: DiskScope::All,
        },
        Action::DiskError {
            handle: "rot".into(),
            machine: "db".into(),
            op: DiskOp::Read,
            ppm: 20,
            effect: DiskEffect::Corrupt,
            scope: DiskScope::Data,
        },
        Action::DiskLatency {
            handle: "slow".into(),
            machine: "db".into(),
            op: DiskOp::Flush,
            latency_ns: 3_000_000,
            jitter_ns: 0,
        },
        Action::DiskLatency {
            handle: "jittery".into(),
            machine: "db".into(),
            op: DiskOp::Any,
            latency_ns: 3_000_000,
            jitter_ns: 500,
        },
        Action::Preempt {
            handle: "race".into(),
            machine: "db".into(),
            ppm: 1_000,
        },
        Action::Heal {
            handle: "slow".into(),
        },
    ] {
        world.apply(&action).unwrap();
    }
    drop(world);
    let mut requests = server.join().unwrap();
    for request in &mut requests {
        let object = request.as_object_mut().unwrap();
        object.remove("version");
        object.remove("id");
    }
    let db = json!({"machine": "db"});
    assert_eq!(
        requests,
        [
            json!({"op": "disk_error", "target": db, "disk_op": "write", "ppm": 10}),
            json!({"op": "disk_error", "target": db, "disk_op": "read", "ppm": 20,
                   "effect": "corrupt", "scope": "data"}),
            json!({"op": "disk_latency", "target": db, "disk_op": "flush",
                   "latency_ns": 3_000_000}),
            json!({"op": "disk_latency", "target": db, "disk_op": "any",
                   "latency_ns": 3_000_000, "jitter_ns": 500}),
            json!({"op": "preempt", "target": db, "ppm": 1_000}),
            json!({"op": "heal", "handle": "p3"}),
        ]
    );
}

#[test]
fn spawn_sends_the_machine_name_and_its_seedbox_yaml_spec() {
    let (directory, server) = serve(|_| ok(json!({})));
    let mut world = GuestWorld::connect_to(directory.path().join("control.sock")).unwrap();
    let spec = json!({"build": {"context": "./workload"}, "environment": {"ROLE": "replica"}});
    world.spawn("replica", spec.clone()).unwrap();
    // A spec that is not an object is refused before anything is sent.
    assert!(world.spawn("other", json!("image")).is_err());
    drop(world);
    let requests = server.join().unwrap();
    assert_eq!(
        requests,
        [json!({"version": 1, "id": 1, "op": "spawn", "machine": "replica", "spec": spec})]
    );
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

#[test]
fn on_break_serves_hits_on_its_own_connection_and_resumes_them() {
    let hit = json!({"break_id": 1, "source": "program", "machine": "n1", "boot": 0,
                     "name": "leader", "at_ns": 25});
    let delivered = std::sync::atomic::AtomicBool::new(false);
    let (directory, server) = serve_connections(2, move |request| {
        match request["op"].as_str().unwrap() {
            // The first wait returns the hit, later ones time out.
            "wait_break" if !delivered.swap(true, Ordering::SeqCst) => ok(hit.clone()),
            _ => ok(json!({})),
        }
    });
    let mut world = GuestWorld::connect_to(directory.path().join("control.sock")).unwrap();
    let (handled, seen) = std::sync::mpsc::channel();
    world
        .on_break("leader", move |world, hit| {
            world.apply(&Action::Stop {
                machine: hit.machine.clone(),
            })?;
            handled.send(hit.clone()).unwrap();
            Ok(())
        })
        .unwrap();
    // Registering a second name declares both, ascending.
    world.on_break("commit", |_, _| Ok(())).unwrap();
    let hit = seen.recv().unwrap();
    assert_eq!(
        hit,
        Hit {
            id: 1,
            machine: "n1".into(),
            boot: 0,
            name: "leader".into(),
            at_ns: 25,
            block_request: None
        }
    );
    drop(world);
    let mut connections = server.join().unwrap().into_iter();
    let main = connections.next().unwrap();
    assert_eq!(main[0]["names"], json!(["leader"]));
    assert_eq!(main[1]["names"], json!(["commit", "leader"]));
    let served: Vec<_> = connections.next().unwrap();
    let ops: Vec<_> = served.iter().map(|r| r["op"].as_str().unwrap()).collect();
    assert_eq!(ops[..3], ["wait_break", "stop", "resume_break"]);
    assert_eq!(served[2]["break_id"], 1);
    assert!(ops[3..].iter().all(|op| *op == "wait_break"));
}

#[test]
fn break_on_block_request_names_its_machine_and_request() {
    let (directory, server) = serve(|_| ok(json!({})));
    let mut world = GuestWorld::connect_to(directory.path().join("control.sock")).unwrap();
    world
        .control()
        .break_on_block_request("n1", crate::control::DiskOp::Flush, 3)
        .unwrap();
    drop(world);
    assert_eq!(
        server.join().unwrap()[0],
        json!({"version": 1, "id": 1, "op": "break_on_block_request",
               "target": {"machine": "n1"}, "request": "flush", "nth": 3})
    );
}

#[test]
fn block_request_hits_carry_their_request_and_need_no_declaration() {
    let hit = json!({"break_id": 2, "source": "block_request", "machine": "n1", "boot": 1,
                     "name": "block_request", "request": "flush", "nth": 3, "at_ns": 40});
    let delivered = std::sync::atomic::AtomicBool::new(false);
    let (directory, server) =
        serve_connections(2, move |request| match request["op"].as_str().unwrap() {
            "wait_break" if !delivered.swap(true, Ordering::SeqCst) => ok(hit.clone()),
            _ => ok(json!({})),
        });
    let mut world = GuestWorld::connect_to(directory.path().join("control.sock")).unwrap();
    // The reserved name is refused before anything is sent.
    assert!(world
        .control()
        .declare_breaks(["leader", "block_request"])
        .is_err());
    let (handled, seen) = std::sync::mpsc::channel();
    world
        .on_break("block_request", move |_, hit| {
            handled.send(hit.clone()).unwrap();
            Ok(())
        })
        .unwrap();
    world.on_break("leader", |_, _| Ok(())).unwrap();
    let hit = seen.recv().unwrap();
    assert_eq!(
        hit,
        Hit {
            id: 2,
            machine: "n1".into(),
            boot: 1,
            name: "block_request".into(),
            at_ns: 40,
            block_request: Some((crate::control::DiskOp::Flush, 3)),
        }
    );
    drop(world);
    let mut connections = server.join().unwrap().into_iter();
    let main = connections.next().unwrap();
    // Registering the block request handler declares nothing for it.
    assert_eq!(main[0]["names"], json!([]));
    assert_eq!(main[1]["names"], json!(["leader"]));
    assert_eq!(main.len(), 2);
}

#[test]
fn hits_whose_source_and_fields_disagree_are_refused() {
    for invalid in [
        json!({"break_id":1,"machine":"m","boot":0,"name":"leader","at_ns":5}),
        json!({"break_id":1,"source":"block_request","machine":"m","boot":0,"name":"block_request","request":"flush","at_ns":5}),
        json!({"break_id":1,"source":"block_request","machine":"m","boot":0,"name":"leader","request":"flush","nth":1,"at_ns":5}),
        json!({"break_id":1,"source":"program","machine":"m","boot":0,"name":"leader","request":"flush","nth":1,"at_ns":5}),
    ] {
        let (directory, server) = serve(move |_| ok(invalid.clone()));
        let mut world = GuestWorld::connect_to(directory.path().join("control.sock")).unwrap();
        assert!(world
            .control()
            .wait_break(std::time::Duration::from_secs(1))
            .is_err());
        drop(world);
        server.join().unwrap();
    }
}
