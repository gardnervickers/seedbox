use super::*;
use seedbox_engine_protocol::property::{Kind as ProtocolKind, Record};

/// Capture what a client would send by pointing it at a fake socket server.
#[cfg(target_os = "linux")]
fn server() -> (tempfile::TempDir, PathBuf, OwnedListener) {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("sdk.sock");
    let listener = OwnedListener::bind(&path);
    (directory, path, listener)
}

#[cfg(target_os = "linux")]
struct OwnedListener(std::os::fd::OwnedFd);

#[cfg(target_os = "linux")]
impl OwnedListener {
    fn bind(path: &Path) -> Self {
        use std::os::fd::FromRawFd;
        use std::os::unix::ffi::OsStrExt;
        let fd = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_SEQPACKET, 0) };
        assert!(fd >= 0);
        let owned = unsafe { std::os::fd::OwnedFd::from_raw_fd(fd) };
        let mut address: libc::sockaddr_un = unsafe { std::mem::zeroed() };
        address.sun_family = libc::AF_UNIX as libc::sa_family_t;
        for (target, byte) in address.sun_path.iter_mut().zip(path.as_os_str().as_bytes()) {
            *target = *byte as libc::c_char;
        }
        let size = std::mem::size_of::<libc::sockaddr_un>() as libc::socklen_t;
        assert_eq!(
            unsafe { libc::bind(fd, (&address as *const libc::sockaddr_un).cast(), size) },
            0
        );
        assert_eq!(unsafe { libc::listen(fd, 4) }, 0);
        Self(owned)
    }
    fn accept(&self) -> UnixDatagram {
        use std::os::fd::{AsRawFd, FromRawFd};
        let fd = unsafe {
            libc::accept(
                self.0.as_raw_fd(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        assert!(fd >= 0);
        UnixDatagram::from(unsafe { std::os::fd::OwnedFd::from_raw_fd(fd) })
    }
}

fn parse(record: Value) -> Record {
    Record::parse(&serde_json::to_vec(&record).unwrap()).unwrap()
}

#[test]
fn records_match_the_engine_schema() {
    assert_eq!(
        parse(json!({"declare": {"id": "p", "kind": Kind::AlwaysOrUnreachable.name()}})),
        Record::Declare {
            id: "p".into(),
            kind: ProtocolKind::AlwaysOrUnreachable,
            message: None
        }
    );
    for (kind, expected) in [
        (Kind::Always, ProtocolKind::Always),
        (Kind::Sometimes, ProtocolKind::Sometimes),
        (Kind::Reachable, ProtocolKind::Reachable),
        (Kind::Unreachable, ProtocolKind::Unreachable),
    ] {
        let body = json!({"id": "p", "kind": kind.name(), "condition": false});
        assert_eq!(
            parse(json!({"assert": with_details(body, json!({"k": 1}))})),
            Record::Assert {
                id: "p".into(),
                kind: expected,
                condition: false,
                message: None,
                details: Some(json!({"k": 1}))
            }
        );
    }
    assert_eq!(
        parse(json!({"event": with_details(json!({"name": "e"}), Value::Null)})),
        Record::Event {
            name: "e".into(),
            details: None
        }
    );
    assert_eq!(
        parse(json!({"setup_complete": with_details(json!({}), Value::Null)})),
        Record::SetupComplete { details: None }
    );
}

#[test]
fn absent_socket_makes_every_call_a_no_op() {
    let directory = tempfile::tempdir().unwrap();
    let client = Client::new(directory.path().join("missing.sock"));
    client.assert("p", Kind::Always, false, Value::Null);
    client.flush().unwrap();
}

#[test]
fn oversized_records_are_refused_before_sending() {
    let directory = tempfile::tempdir().unwrap();
    let client = Client::new(directory.path().join("missing.sock"));
    let big = json!({"assert": {"id": "p", "details": "x".repeat(MAX_RECORD_BYTES)}});
    assert_eq!(
        client.send(&big).unwrap_err().kind(),
        io::ErrorKind::InvalidInput
    );
}

#[cfg(target_os = "linux")]
#[test]
fn records_and_flush_round_trip_over_seqpacket() {
    let (_directory, path, listener) = server();
    let client = Client::new(&path);
    client.declare("p", Kind::Always);
    let server = listener.accept();
    client.assert("p", Kind::Always, true, json!({"n": 1}));
    let mut buffer = [0u8; MAX_RECORD_BYTES];
    let mut records = Vec::new();
    for _ in 0..2 {
        let n = server.recv(&mut buffer).unwrap();
        records.push(Record::parse(&buffer[..n]).unwrap());
    }
    assert!(matches!(records[0], Record::Declare { .. }));
    assert!(matches!(
        records[1],
        Record::Assert {
            condition: true,
            ..
        }
    ));
    let replier = std::thread::spawn(move || {
        let n = server.recv(&mut buffer).unwrap();
        assert_eq!(&buffer[..n], FLUSH);
        server.send(b"ok").unwrap();
    });
    client.flush().unwrap();
    replier.join().unwrap();
}

#[test]
fn choices_match_the_engine_schema_and_choose_the_first_outside_seedbox() {
    assert_eq!(
        parse(json!({"choice": {"site": "pick", "alternatives": ["a", "b"]}})),
        Record::Choice {
            site: "pick".into(),
            alternatives: vec!["a".into(), "b".into()],
        }
    );
    let client = Client::new("/nonexistent/seedbox/sdk.sock");
    assert_eq!(client.choose("pick", &["a", "b"]).unwrap(), 0);
    assert!(client.choose("pick", &[]).is_err());
}

#[cfg(target_os = "linux")]
#[test]
fn a_choice_waits_for_its_reply() {
    let (_directory, path, listener) = server();
    let client = Client::new(&path);
    client.event("connect", json!(null));
    let server = listener.accept();
    let mut buffer = [0u8; MAX_RECORD_BYTES];
    server.recv(&mut buffer).unwrap();
    let replier = std::thread::spawn(move || {
        let n = server.recv(&mut buffer).unwrap();
        assert!(matches!(
            Record::parse(&buffer[..n]).unwrap(),
            Record::Choice { .. }
        ));
        server
            .send(br#"{"choice":{"index":2,"occurrence":1,"site":"pick"}}"#)
            .unwrap();
    });
    assert_eq!(client.choose("pick", &["a", "b", "c"]).unwrap(), 2);
    replier.join().unwrap();
}

#[test]
fn recovery_obligations_pass_once_the_probe_succeeds_and_fail_at_the_deadline() {
    let mut calls = 0;
    assert!(recovers_within("healthy", Duration::from_secs(5), || {
        calls += 1;
        calls == 3
    }));
    assert_eq!(calls, 3);
    assert!(!recovers_within("never", Duration::from_millis(30), || {
        false
    }));
}

#[test]
fn workload_decisions_take_the_first_alternative_outside_seedbox() {
    let decisions = workload::Decisions::with_socket("client-1", "/nonexistent/sdk.sock");
    assert_eq!(*decisions.pick("op", &[("read", 'r'), ("write", 'w')]), 'r');
    assert_eq!(decisions.index("key", 1000), 0);
    assert!(!decisions.flag("retry"));
}

#[cfg(target_os = "linux")]
#[test]
fn workload_sites_name_their_client() {
    let (_directory, path, listener) = server();
    let decisions = workload::Decisions::with_socket("client-2", &path);
    let replier = std::thread::spawn(move || {
        let server = listener.accept();
        let mut buffer = [0u8; MAX_RECORD_BYTES];
        let n = server.recv(&mut buffer).unwrap();
        let Record::Choice { site, alternatives } = Record::parse(&buffer[..n]).unwrap() else {
            panic!("not a choice");
        };
        assert_eq!((site.as_str(), alternatives.len()), ("client-2/key", 3));
        server
            .send(br#"{"choice":{"index":2,"occurrence":1,"site":"client-2/key"}}"#)
            .unwrap();
    });
    assert_eq!(decisions.index("key", 3), 2);
    replier.join().unwrap();
}
