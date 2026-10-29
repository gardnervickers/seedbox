use super::*;
use seedbox_engine_protocol::property::{
    Kind as ProtocolKind, Record, Reply, SOCKET as PROTOCOL_SOCKET,
};

/// Capture what a client would send by pointing it at a fake socket server.
#[cfg(target_os = "linux")]
fn server() -> (tempfile::TempDir, PathBuf, OwnedListener) {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("guest.sock");
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

/// The guest library writes its own JSON; the protocol crate's parser must
/// accept every record exactly as the library builds it, and the library must
/// read the replies the protocol crate encodes.
#[test]
fn guest_encoding_decodes_with_the_protocol_crate() {
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
        parse(require_record("p")),
        Record::Require { id: "p".into() }
    );
    for (kind, expected) in [
        (Kind::Always, ProtocolKind::Always),
        (Kind::Never, ProtocolKind::Never),
    ] {
        assert_eq!(
            parse(assert_record("p", kind, false, json!({"k": 1}))),
            Record::Assert {
                id: "p".into(),
                kind: expected,
                condition: false,
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
    assert_eq!(Record::parse(FLUSH), Ok(Record::Flush {}));

    let flush = serde_json::to_vec(&Reply::Flush { seq: 3 }).unwrap();
    assert_eq!(reply_body(&flush, "flush", 3), Some(json!({"seq": 3})));
    // A reply to another request, or for another sequence number, is refused.
    assert_eq!(reply_body(&flush, "choice", 3), None);
    assert_eq!(reply_body(&flush, "flush", 4), None);
}

#[test]
fn absent_socket_makes_every_call_a_no_op() {
    let directory = tempfile::tempdir().unwrap();
    let client = Client::new(directory.path().join("missing.sock"));
    client.require("p");
    client.always("p", false, Value::Null);
    client.never("q", true, Value::Null);
    client.flush().unwrap();
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
        "seedbox-guest-test-nobody-listens",
        &kmsg,
    );
    client.always("p", false, Value::Null);
    client.never("q", true, Value::Null);
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
    let name = format!("seedbox-guest-test-{}", std::process::id());
    let listener = OwnedListener::bind_abstract(&name);
    let kmsg = directory.path().join("kmsg");
    let client = Client::with_fallback(&directory.path().join("missing.sock"), &name, &kmsg);
    client.require("p");
    let server = listener.accept();
    let mut buffer = [0u8; MAX_RECORD_BYTES];
    let n = server.recv(&mut buffer).unwrap();
    assert!(matches!(
        Record::parse(&buffer[..n]).unwrap(),
        Record::Require { .. }
    ));
    assert!(!kmsg.exists());
}

/// A failing evaluation must arrive however large its details: the
/// engine's verdict depends on it.
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
            details: None,
            details_truncated: true,
        })
    );
    // Details that fit are kept as they are.
    let small = assert_record("p", Kind::Always, false, json!({"k": 1}));
    assert_eq!(fit(small.clone()), serde_json::to_vec(&small).unwrap());
    // An impossible ID still arrives, as a record the engine refuses: an
    // lost evidence rather than nothing.
    let huge = "i".repeat(2 * MAX_RECORD_BYTES);
    let bytes = fit(assert_record(&huge, Kind::Always, false, big));
    assert!(bytes.len() <= MAX_RECORD_BYTES);
    assert_eq!(
        Record::parse(&bytes),
        Err("invalid property id".to_string())
    );
    // A multibyte ID is cut on a character boundary.
    let wide = "é".repeat(MAX_RECORD_BYTES);
    assert!(fit(require_record(&wide)).len() <= MAX_RECORD_BYTES);
}

#[cfg(target_os = "linux")]
#[test]
fn records_and_flush_round_trip_over_seqpacket() {
    let (_directory, path, listener) = server();
    let client = Client::new(&path);
    client.require("p");
    let server = listener.accept();
    client.always("p", true, json!({"n": 1}));
    let mut buffer = [0u8; MAX_RECORD_BYTES];
    let mut records = Vec::new();
    for _ in 0..2 {
        let n = server.recv(&mut buffer).unwrap();
        records.push(Record::parse(&buffer[..n]).unwrap());
    }
    assert!(matches!(records[0], Record::Require { .. }));
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
        server
    });
    client.flush().unwrap();
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
    client.require("p");
    // The supervisor side goes away; the next record cannot be sent.
    drop(listener.accept());
    client.always("p", false, Value::Null);
    // The next call opens a new session and reports the loss first.
    client.never("q", false, Value::Null);
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
        Record::Assert {
            kind: ProtocolKind::Never,
            ..
        }
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

/// A child forked after the parent's session opened must not write into it
/// or read its replies: each process's records and flush replies travel in
/// a session of its own, each with its own sequence.
#[cfg(target_os = "linux")]
#[test]
fn a_forked_child_opens_its_own_session() {
    use std::os::fd::AsRawFd;
    let (_directory, path, listener) = server();
    // Fail rather than hang if the child never connects.
    let timeout = libc::timeval {
        tv_sec: 10,
        tv_usec: 0,
    };
    // SAFETY: the listener is live and `timeout` is a valid timeval.
    let set = unsafe {
        libc::setsockopt(
            listener.0.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_RCVTIMEO,
            (&timeout as *const libc::timeval).cast(),
            std::mem::size_of::<libc::timeval>() as libc::socklen_t,
        )
    };
    assert_eq!(set, 0);
    let client = Client::new(&path);
    client.lock().reply_timeout = Duration::from_secs(10);
    client.always("parent before fork", true, Value::Null);
    let parent = listener.accept();
    parent
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let mut buffer = [0u8; MAX_RECORD_BYTES];
    let receive = |session: &UnixDatagram, buffer: &mut [u8]| {
        let n = session.recv(buffer).unwrap();
        Record::parse(&buffer[..n]).unwrap()
    };
    let id = |record: Record| match record {
        Record::Assert { id, .. } => id,
        other => panic!("expected an assertion, got {other:?}"),
    };
    assert_eq!(id(receive(&parent, &mut buffer)), "parent before fork");

    // SAFETY: the child only uses the client and then leaves with _exit,
    // never returning into the test harness.
    #[allow(
        clippy::disallowed_methods,
        reason = "the regression subject is a forked child's session; it execs nothing"
    )]
    let child = unsafe { libc::fork() };
    assert!(child >= 0, "{}", io::Error::last_os_error());
    if child == 0 {
        let flushed = std::panic::catch_unwind(|| {
            client.always("child", true, Value::Null);
            client.flush().is_ok()
        });
        // SAFETY: ends the forked child without running the harness.
        unsafe { libc::_exit(if matches!(flushed, Ok(true)) { 0 } else { 1 }) };
    }

    // The child's records arrive in a new session, numbered from 1, and
    // its flush is answered there.
    let session = listener.accept();
    session
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    assert_eq!(id(receive(&session, &mut buffer)), "child");
    assert_eq!(receive(&session, &mut buffer), Record::Flush {});
    session
        .send(&serde_json::to_vec(&Reply::Flush { seq: 2 }).unwrap())
        .unwrap();
    let mut status = 0;
    // SAFETY: waits for the child forked above.
    assert_eq!(unsafe { libc::waitpid(child, &mut status, 0) }, child);
    assert!(
        libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0,
        "child flush failed: status {status}"
    );

    // Nothing from the child reached the parent's session, which is still
    // open, and the parent's flush is its own second message.
    parent.set_nonblocking(true).unwrap();
    assert_eq!(
        parent.recv(&mut buffer).unwrap_err().kind(),
        io::ErrorKind::WouldBlock
    );
    parent.set_nonblocking(false).unwrap();
    let replier = std::thread::spawn(move || {
        let mut buffer = [0u8; 64];
        let n = parent.recv(&mut buffer).unwrap();
        assert_eq!(&buffer[..n], FLUSH);
        parent
            .send(&serde_json::to_vec(&Reply::Flush { seq: 2 }).unwrap())
            .unwrap();
    });
    client.flush().unwrap();
    replier.join().unwrap();
}
