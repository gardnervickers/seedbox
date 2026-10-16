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
