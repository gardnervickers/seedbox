//! CLI-only HTTP fixture: no engine or guest is launched.
use super::*;
use std::{
    io::{Read, Write},
    os::{
        fd::AsRawFd,
        unix::{fs::PermissionsExt, net::UnixListener},
    },
    time::Duration,
};

fn resource(imported: bool) -> Vec<u8> {
    let request = seedbox_server::test_support::resolved_request("history-fixture");
    let id = "k7m2p9qx".parse().unwrap();
    let metadata = seedbox_api::Metadata {
        name: "history-fixture".into(),
        labels: Default::default(),
        annotations: Default::default(),
        id,
        revision: 1,
        created_at_unix_ms: 0,
        deletion_requested_at_unix_ms: None,
        parent: None,
        checkpoint: None,
    };
    let document = seedbox_api::Simulation::queued(metadata, request.spec);
    let mut value = serde_json::to_value(document).unwrap();
    if imported {
        value["spec"]["start"] =
            serde_json::json!({"kind":"checkpoint", "manifest":{}, "assets":{}});
    }
    serde_json::to_vec(&value).unwrap()
}

fn observe(args: &[&str], imported: bool) -> Output {
    let root = tempfile::tempdir().unwrap();
    let state = root.path().join(".local/state/seedbox");
    fs::create_dir_all(&state).unwrap();
    fs::set_permissions(&state, fs::Permissions::from_mode(0o700)).unwrap();
    let listener = UnixListener::bind(state.join("server.sock")).unwrap();
    let document = resource(imported);
    let server = std::thread::spawn(move || {
        let guard = seedbox_test_time::HangGuard::new(
            "CLI fixture must receive stream, resource and cleanup requests",
            Duration::from_secs(30),
        );
        let mut requests = 0;
        while requests < 3 {
            let mut fd = libc::pollfd {
                fd: listener.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            let remaining = i32::try_from(guard.remaining().unwrap().as_millis()).unwrap();
            let ready = unsafe { libc::poll(&mut fd, 1, remaining) };
            assert_eq!(ready, 1);
            let (mut socket, _) = listener.accept().unwrap();
            socket
                .set_read_timeout(Some(guard.remaining().unwrap()))
                .unwrap();
            socket
                .set_write_timeout(Some(guard.remaining().unwrap()))
                .unwrap();
            let mut header = Vec::new();
            loop {
                let mut byte = [0];
                if socket.read(&mut byte).unwrap() == 0 {
                    break;
                }
                header.push(byte[0]);
                assert!(header.len() < 32 * 1024);
                if header.ends_with(b"\r\n\r\n") {
                    break;
                }
            }
            if header.is_empty() {
                continue;
            } // daemon's connect-only liveness probe
            requests += 1;
            let header = String::from_utf8(header).unwrap();
            let path = header.split_whitespace().nth(1).unwrap();
            let (status, content_type, body): (&str, &str, &[u8]) =
                if path == "/v1/simulations/k7m2p9qx" {
                    ("200 OK", "application/json", &document)
                } else if path.contains("/logs") || path.contains("/events") {
                    (
                        "200 OK",
                        "text/event-stream",
                        b"data: {\"type\":\"closed\"}\n\n",
                    )
                } else {
                    assert_eq!(path, "/v1", "only ServerCleanup may make this request");
                    ("404 Not Found", "application/json", b"{}")
                };
            write!(socket, "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).unwrap();
            socket.write_all(body).unwrap();
        }
    });
    let output = seedbox(args, root.path());
    server.join().unwrap();
    output
}

#[test]
fn imported_checkpoint_logs_and_events_diagnose_absent_prefix_without_fabricated_bytes() {
    for args in [
        vec!["logs", "k7m2p9qx", "oracle"],
        vec!["logs", "k7m2p9qx", "oracle", "--json"],
        vec!["events", "k7m2p9qx"],
    ] {
        let output = observe(&args, true);
        assert!(output.status.success(), "{}", stderr(&output));
        assert!(
            output.stdout.is_empty(),
            "an absent prefix is not application bytes or an event"
        );
        assert_eq!(
            stderr(&output),
            "omission: history before checkpoint not available\n"
        );
    }
}

#[test]
fn fresh_logs_and_events_do_not_claim_missing_checkpoint_history() {
    for args in [
        vec!["logs", "k7m2p9qx", "oracle"],
        vec!["events", "k7m2p9qx"],
    ] {
        let output = observe(&args, false);
        assert!(output.status.success(), "{}", stderr(&output));
        assert!(output.stdout.is_empty());
        assert_eq!(stderr(&output), "");
    }
}
