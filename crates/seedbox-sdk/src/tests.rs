use super::*;
use seedbox_engine_protocol::property::{
    Counts, Kind as ProtocolKind, Record, Reply, SOCKET as PROTOCOL_SOCKET,
};
use std::cell::Cell;

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
        // SAFETY: plain socket creation; the descriptor is owned immediately.
        let fd = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_SEQPACKET, 0) };
        assert!(fd >= 0);
        // SAFETY: `fd` is a fresh descriptor owned here.
        let owned = unsafe { std::os::fd::OwnedFd::from_raw_fd(fd) };
        // SAFETY: an all-zero sockaddr_un is a valid initial value.
        let mut address: libc::sockaddr_un = unsafe { std::mem::zeroed() };
        address.sun_family = libc::AF_UNIX as libc::sa_family_t;
        for (target, byte) in address.sun_path.iter_mut().zip(path.as_os_str().as_bytes()) {
            *target = *byte as libc::c_char;
        }
        let size = std::mem::size_of::<libc::sockaddr_un>() as libc::socklen_t;
        // SAFETY: the address is initialized and its size is passed exactly.
        let bound = unsafe { libc::bind(fd, (&address as *const libc::sockaddr_un).cast(), size) };
        assert_eq!(bound, 0);
        // SAFETY: `fd` is a bound socket.
        assert_eq!(unsafe { libc::listen(fd, 4) }, 0);
        Self(owned)
    }
    /// Listen in the abstract namespace, as the supervisor's fallback does.
    fn bind_abstract(name: &str) -> Self {
        use std::os::fd::FromRawFd;
        // SAFETY: plain socket creation; the descriptor is owned immediately.
        let fd = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_SEQPACKET, 0) };
        assert!(fd >= 0);
        // SAFETY: `fd` is a fresh descriptor owned here.
        let owned = unsafe { std::os::fd::OwnedFd::from_raw_fd(fd) };
        // SAFETY: an all-zero sockaddr_un is a valid initial value.
        let mut address: libc::sockaddr_un = unsafe { std::mem::zeroed() };
        address.sun_family = libc::AF_UNIX as libc::sa_family_t;
        for (target, byte) in address.sun_path[1..].iter_mut().zip(name.bytes()) {
            *target = byte as libc::c_char;
        }
        let size = std::mem::offset_of!(libc::sockaddr_un, sun_path) + 1 + name.len();
        // SAFETY: the address is initialized and its size is passed exactly.
        let bound = unsafe {
            libc::bind(
                fd,
                (&address as *const libc::sockaddr_un).cast(),
                size as libc::socklen_t,
            )
        };
        assert_eq!(bound, 0);
        // SAFETY: `fd` is a bound socket.
        assert_eq!(unsafe { libc::listen(fd, 4) }, 0);
        Self(owned)
    }
    fn accept(&self) -> UnixDatagram {
        use std::os::fd::{AsRawFd, FromRawFd};
        // SAFETY: the listener is live; no peer address is requested.
        let fd = unsafe {
            libc::accept(
                self.0.as_raw_fd(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        assert!(fd >= 0);
        // SAFETY: `fd` is the accepted connection, owned here.
        UnixDatagram::from(unsafe { std::os::fd::OwnedFd::from_raw_fd(fd) })
    }
}

fn parse(record: Value) -> Record {
    Record::parse(&serde_json::to_vec(&record).unwrap()).unwrap()
}

/// The SDK writes its own JSON; the protocol crate's parser must accept every
/// record exactly as the SDK builds it, and the SDK must read the replies the
/// protocol crate encodes.
#[test]
fn sdk_encoding_decodes_with_the_protocol_crate() {
    use seedbox_engine_protocol::property as protocol;
    assert_eq!(SOCKET, PROTOCOL_SOCKET);
    assert_eq!(SOCKET_ENV, protocol::SOCKET_ENV);
    assert_eq!(ABSTRACT_SOCKET, protocol::ABSTRACT_SOCKET);
    assert_eq!(KMSG_LOST, protocol::KMSG_LOST);
    assert_eq!(
        MAX_RECORD_BYTES,
        seedbox_engine_protocol::property::MAX_RECORD_BYTES
    );
    assert_eq!(
        MAX_REPLY_BYTES,
        seedbox_engine_protocol::property::MAX_REPLY_BYTES
    );
    assert_eq!(
        MAX_ID_BYTES,
        seedbox_engine_protocol::property::MAX_ID_BYTES
    );
    assert_eq!(
        parse(declare_record("p", Kind::AlwaysOrUnreachable)),
        Record::Declare {
            id: "p".into(),
            kind: ProtocolKind::AlwaysOrUnreachable,
            message: None
        }
    );
    for (kind, expected) in [
        (Kind::Always, ProtocolKind::Always),
        (Kind::AlwaysOrUnreachable, ProtocolKind::AlwaysOrUnreachable),
        (Kind::Sometimes, ProtocolKind::Sometimes),
        (Kind::Reachable, ProtocolKind::Reachable),
        (Kind::Unreachable, ProtocolKind::Unreachable),
    ] {
        assert_eq!(
            parse(assert_record("p", kind, false, json!({"k": 1}))),
            Record::Assert {
                id: "p".into(),
                kind: expected,
                condition: false,
                message: None,
                details: Some(json!({"k": 1})),
                details_truncated: false,
            }
        );
    }
    assert_eq!(
        Record::parse(&lost_record(2, "Broken pipe")),
        Ok(Record::Lost {
            reason: "Broken pipe".into(),
            count: 2
        })
    );
    assert_eq!(
        parse(event_record("e", Value::Null)),
        Record::Event {
            name: "e".into(),
            details: None
        }
    );
    assert_eq!(
        parse(setup_record(json!({"nodes": 3}))),
        Record::SetupComplete {
            details: Some(json!({"nodes": 3}))
        }
    );
    assert_eq!(
        parse(state_record("role", "leader")),
        Record::State {
            name: "role".into(),
            value: "leader".into()
        }
    );
    assert_eq!(
        parse(choice_record("retry", &["no", "yes"])),
        Record::Choice {
            site: "retry".into(),
            alternatives: vec!["no".into(), "yes".into()]
        }
    );
    assert_eq!(
        MAX_CHOICE_ALTERNATIVES,
        seedbox_engine_protocol::property::MAX_CHOICE_ALTERNATIVES
    );
    assert_eq!(
        CHOICE_UNANSWERED_EVENT,
        seedbox_engine_protocol::property::CHOICE_UNANSWERED_EVENT
    );
    assert_eq!(
        CHOICE_TIMEOUT.as_nanos(),
        u128::from(seedbox_engine_protocol::choice::CHOICE_TIMEOUT_NS)
    );
    let choice = Reply::Choice {
        seq: 2,
        site: "retry".into(),
        occurrence: 1,
        index: 1,
    };
    let body = reply_body(&serde_json::to_vec(&choice).unwrap(), "choice", 2).unwrap();
    assert_eq!(body["index"], 1);
    assert_eq!(Record::parse(FLUSH), Ok(Record::Flush {}));
    assert_eq!(Record::parse(SUMMARY), Ok(Record::Summary {}));

    let flush = serde_json::to_vec(&Reply::Flush { seq: 3 }).unwrap();
    assert_eq!(reply_body(&flush, "flush", 3), Some(json!({"seq": 3})));
    // A reply to another request, or for another sequence number, is refused.
    assert_eq!(reply_body(&flush, "summary", 3), None);
    assert_eq!(reply_body(&flush, "flush", 4), None);
    let summary = Reply::Summary {
        seq: 4,
        properties: [(
            "p".to_string(),
            Counts {
                passes: 2,
                failures: 1,
            },
        )]
        .into(),
        truncated: true,
    };
    let body = reply_body(&serde_json::to_vec(&summary).unwrap(), "summary", 4).unwrap();
    let parsed = parse_summary(&body).unwrap();
    assert_eq!(
        parsed.properties["p"],
        PropertyCounts {
            passes: 2,
            failures: 1
        }
    );
    assert!(parsed.truncated);
}

#[test]
fn absent_socket_makes_every_call_a_no_op() {
    let directory = tempfile::tempdir().unwrap();
    let client = Client::new(directory.path().join("missing.sock"));
    client.assert("p", Kind::Always, false, Value::Null);
    client.flush().unwrap();
    assert_eq!(client.summary().unwrap(), Summary::default());
}

/// Under Seedbox an unreachable socket is lost evidence, reported once on
/// the kernel log in the form the supervisor relays, never absence.
#[test]
fn an_unreachable_socket_under_seedbox_is_reported_as_lost() {
    let directory = tempfile::tempdir().unwrap();
    let kmsg = directory.path().join("kmsg");
    std::fs::write(&kmsg, "").unwrap();
    let client = Client::with_fallback(
        &directory.path().join("missing.sock"),
        "seedbox-sdk-test-nobody-listens",
        &kmsg,
    );
    client.assert("p", Kind::Always, false, Value::Null);
    client.event("e", Value::Null);
    assert!(client.flush().is_err());
    let logged = std::fs::read_to_string(&kmsg).unwrap();
    assert_eq!(logged.lines().count(), 1, "{logged}");
    let count = logged
        .strip_prefix(KMSG_LOST)
        .and_then(|rest| rest.split_once(" property records: "))
        .map(|(count, _)| count);
    assert_eq!(count, Some("1"), "{logged}");
}

#[cfg(target_os = "linux")]
#[test]
fn an_unreachable_path_falls_back_to_the_abstract_listener() {
    let directory = tempfile::tempdir().unwrap();
    let name = format!("seedbox-sdk-test-{}", std::process::id());
    let listener = OwnedListener::bind_abstract(&name);
    let kmsg = directory.path().join("kmsg");
    let client = Client::with_fallback(&directory.path().join("missing.sock"), &name, &kmsg);
    client.declare("p", Kind::Always);
    let server = listener.accept();
    let mut buffer = [0u8; MAX_RECORD_BYTES];
    let n = server.recv(&mut buffer).unwrap();
    assert!(matches!(
        Record::parse(&buffer[..n]).unwrap(),
        Record::Declare { .. }
    ));
    assert!(!kmsg.exists());
}

/// A failing evaluation must arrive however large its details: the
/// engine's verdict depends on it.
#[test]
fn choices_take_the_first_alternative_outside_seedbox_and_check_their_bounds() {
    let directory = tempfile::tempdir().unwrap();
    let client = Client::new(directory.path().join("missing.sock"));
    client.state("role", "leader");
    assert_eq!(client.choose("retry", &["no", "yes"]).unwrap(), 0);
    for bad in [vec![], vec!["a"; MAX_CHOICE_ALTERNATIVES + 1]] {
        assert_eq!(
            client.choose("retry", &bad).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
    }
    let long = "x".repeat(MAX_RECORD_BYTES);
    assert_eq!(
        client.choose("retry", &[&long]).unwrap_err().kind(),
        io::ErrorKind::InvalidInput
    );
}

#[cfg(target_os = "linux")]
#[test]
fn choices_wait_for_the_engine_answer_for_their_site() {
    let (_directory, path, listener) = server();
    let client = Client::new(&path);
    client.state("phase", "recovering");
    let server = listener.accept();
    let replier = std::thread::spawn(move || {
        let mut buffer = [0u8; MAX_RECORD_BYTES];
        let n = server.recv(&mut buffer).unwrap();
        assert!(matches!(
            Record::parse(&buffer[..n]),
            Ok(Record::State { .. })
        ));
        for (site, index) in [("retry", 1), ("other", 0)] {
            let n = server.recv(&mut buffer).unwrap();
            assert!(matches!(
                Record::parse(&buffer[..n]),
                Ok(Record::Choice { .. })
            ));
            let reply = Reply::Choice {
                seq: 2,
                site: site.into(),
                occurrence: 1,
                index,
            };
            server.send(&serde_json::to_vec(&reply).unwrap()).unwrap();
        }
    });
    assert_eq!(client.choose("retry", &["no", "yes"]).unwrap(), 1);
    // An answer for another site is an error, not a silent choice.
    assert!(client.choose("retry", &["no", "yes"]).is_err());
    replier.join().unwrap();
}

#[cfg(target_os = "linux")]
#[test]
fn unanswered_choices_time_out_reopen_the_session_and_are_recorded() {
    let (_directory, path, listener) = server();
    let client = Client::new(&path).with_choice_timeout(Duration::from_millis(1));
    client.state("phase", "recovering");
    let first = listener.accept();
    // Nobody answers the choice on the first session.
    let error = client.choose("retry", &["no", "yes"]).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::TimedOut);
    let mut buffer = [0u8; MAX_RECORD_BYTES];
    for _ in 0..2 {
        first.recv(&mut buffer).unwrap();
    }
    // The event goes out on a new session.
    let second = listener.accept();
    let n = second.recv(&mut buffer).unwrap();
    match Record::parse(&buffer[..n]).unwrap() {
        Record::Event { name, details } => {
            assert_eq!(name, CHOICE_UNANSWERED_EVENT);
            assert_eq!(details.unwrap()["site"], "retry");
        }
        other => panic!("unexpected record {other:?}"),
    }
}

#[test]
fn oversized_records_keep_their_evaluation_and_mark_lost_details() {
    let big = json!({"log": "x".repeat(2 * MAX_RECORD_BYTES)});
    let bytes = fit(assert_record("p", Kind::Always, false, big.clone()));
    assert!(bytes.len() <= MAX_RECORD_BYTES);
    assert_eq!(
        Record::parse(&bytes),
        Ok(Record::Assert {
            id: "p".into(),
            kind: ProtocolKind::Always,
            condition: false,
            message: None,
            details: None,
            details_truncated: true,
        })
    );
    // Details that fit are kept as they are.
    let small = assert_record("p", Kind::Always, false, json!({"k": 1}));
    assert_eq!(fit(small.clone()), serde_json::to_vec(&small).unwrap());
    // Other records just lose their details.
    let event = fit(event_record("e", big.clone()));
    assert_eq!(
        Record::parse(&event),
        Ok(Record::Event {
            name: "e".into(),
            details: None
        })
    );
    // An impossible ID still arrives, as a record the engine refuses: an
    // evidence error rather than nothing.
    let huge = "i".repeat(2 * MAX_RECORD_BYTES);
    let bytes = fit(assert_record(&huge, Kind::Always, false, big));
    assert!(bytes.len() <= MAX_RECORD_BYTES);
    assert_eq!(
        Record::parse(&bytes),
        Err("invalid property id".to_string())
    );
    // A multibyte ID is cut on a character boundary.
    let wide = "é".repeat(MAX_RECORD_BYTES);
    assert!(fit(declare_record(&wide, Kind::Always)).len() <= MAX_RECORD_BYTES);
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
    // The engine, through the supervisor, confirms the session's records.
    let replier = std::thread::spawn(move || {
        let n = server.recv(&mut buffer).unwrap();
        assert_eq!(&buffer[..n], FLUSH);
        let flush = Reply::Flush { seq: 3 };
        server.send(&serde_json::to_vec(&flush).unwrap()).unwrap();
        let n = server.recv(&mut buffer).unwrap();
        assert_eq!(&buffer[..n], SUMMARY);
        let summary = Reply::Summary {
            seq: 4,
            properties: [(
                "p".to_string(),
                Counts {
                    passes: 1,
                    failures: 0,
                },
            )]
            .into(),
            truncated: false,
        };
        server.send(&serde_json::to_vec(&summary).unwrap()).unwrap();
        server
    });
    client.flush().unwrap();
    let summary = client.summary().unwrap();
    assert_eq!(
        summary.properties["p"],
        PropertyCounts {
            passes: 1,
            failures: 0
        }
    );
    let server = replier.join().unwrap();
    // A reply that is not the engine's, or confirms another record, is an
    // error, not a silent success; it also ends the session.
    let replier = std::thread::spawn(move || {
        let mut buffer = [0u8; 64];
        server.recv(&mut buffer).unwrap();
        server.send(b"ok").unwrap();
    });
    assert!(client.flush().is_err());
    replier.join().unwrap();
    // The next flush opens a new session, accepted by the replier.
    let replier = std::thread::spawn(move || {
        let server = listener.accept();
        let mut buffer = [0u8; 64];
        server.recv(&mut buffer).unwrap();
        // The new session's first message is seq 1, not 7.
        server
            .send(&serde_json::to_vec(&Reply::Flush { seq: 7 }).unwrap())
            .unwrap();
    });
    assert!(client.flush().is_err());
    replier.join().unwrap();
}

#[cfg(target_os = "linux")]
#[test]
fn records_that_cannot_be_sent_are_reported_as_lost() {
    let (_directory, path, listener) = server();
    let client = Client::new(&path);
    client.declare("p", Kind::Always);
    // The supervisor side goes away; the next record cannot be sent.
    drop(listener.accept());
    client.assert("p", Kind::Always, false, Value::Null);
    // The next call opens a new session and reports the loss first.
    client.event("after", Value::Null);
    let server = listener.accept();
    let mut buffer = [0u8; MAX_RECORD_BYTES];
    let n = server.recv(&mut buffer).unwrap();
    let Record::Lost { count, .. } = Record::parse(&buffer[..n]).unwrap() else {
        panic!("expected a lost record");
    };
    assert_eq!(count, 1);
    let n = server.recv(&mut buffer).unwrap();
    assert!(matches!(
        Record::parse(&buffer[..n]).unwrap(),
        Record::Event { .. }
    ));
    // The flush that follows confirms the session's third message.
    let replier = std::thread::spawn(move || {
        let n = server.recv(&mut buffer).unwrap();
        assert_eq!(&buffer[..n], FLUSH);
        server
            .send(&serde_json::to_vec(&Reply::Flush { seq: 3 }).unwrap())
            .unwrap();
    });
    client.flush().unwrap();
    replier.join().unwrap();
}

/// Poll against a fake clock that each probe advances by `step`.
fn poll(deadline_ms: u64, step_ms: u64, succeeds: impl Fn(u32) -> bool) -> (bool, Value, u32) {
    let base = Instant::now();
    let clock = Cell::new(base);
    let calls = Cell::new(0);
    let probe = || {
        clock.set(clock.get() + Duration::from_millis(step_ms));
        calls.set(calls.get() + 1);
        succeeds(calls.get())
    };
    let sleep = |pause: Duration| clock.set(clock.get() + pause);
    let (recovered, details) = poll_recovery(
        Duration::from_millis(deadline_ms),
        probe,
        || clock.get(),
        sleep,
    );
    (recovered, details, calls.get())
}

#[test]
fn recovery_passes_within_the_deadline_and_fails_after_it() {
    // The third probe succeeds after two 20ms pauses.
    let (recovered, details, calls) = poll(100, 1, |call| call == 3);
    assert!(recovered);
    assert_eq!(calls, 3);
    assert_eq!(details["waited_ms"], 43);
    // A probe that never succeeds fails at the deadline, without oversleeping.
    let (recovered, details, _) = poll(30, 1, |_| false);
    assert!(!recovered);
    assert_eq!(details["deadline_ms"], 30);
    assert_eq!(details["waited_ms"], 31);
    // A probe that succeeds, but only after the deadline, fails.
    let (recovered, _, calls) = poll(10, 30, |_| true);
    assert!(!recovered);
    assert_eq!(calls, 1);
    // A probe that fails past the deadline is not retried.
    let (recovered, details, calls) = poll(10, 30, |call| call > 1);
    assert!(!recovered);
    assert_eq!(calls, 1);
    assert_eq!(details["deadline_ms"], 10);
}
