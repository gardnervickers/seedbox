//! Property acceptance probe. Every mode exits 0, so only the properties
//! decide the run's verdict:
//!
//! - `pass` (the oracle) requires and evaluates passing properties, crashes
//!   a child on its own machine, which allows crashes, then crashes and
//!   reboots machine `node` and waits until `node`'s second boot has
//!   flushed its records;
//! - `node` evaluates a property on each boot, flushes, then serves its
//!   port so the oracle knows the records counted;
//! - `violate` evaluates a false `always`;
//! - `unmet` requires a property it never evaluates;
//! - `unexpected-crash` crashes a child on a machine that does not allow
//!   crashes;
//! - `lose` sends a record too large to relay, which the supervisor reports
//!   lost: the run's evidence is incomplete;
//! - `hidden-socket` mounts a tmpfs over `/run`, as some entrypoints do, then
//!   evaluates a true `never`, which must still reach the engine.
use serde_json::{json, Value};
use std::{
    io::{BufRead, BufReader, Write},
    net::{TcpListener, TcpStream, ToSocketAddrs},
    os::unix::net::UnixStream,
    process::{Command, ExitCode},
    time::Duration,
};

/// Where `node` serves once its records are flushed.
const NODE_PORT: u16 = 7000;

/// Crash a child with SIGABRT, a core-dumping signal the kernel reports.
fn crash() {
    let status = Command::new("/property-probe")
        .arg("abort")
        .status()
        .unwrap();
    // The child died of SIGABRT (6).
    assert_eq!(
        std::os::unix::process::ExitStatusExt::signal(&status),
        Some(6)
    );
}

/// One connection to the guest control RPC (`python/seedbox/control.py`).
struct Control {
    stream: BufReader<UnixStream>,
    id: u64,
}

impl Control {
    fn connect() -> Self {
        let stream = UnixStream::connect("/run/seedbox/control.sock").expect("control socket");
        Self {
            stream: BufReader::new(stream),
            id: 0,
        }
    }

    fn call(&mut self, op: &str, mut arguments: Value) {
        self.id += 1;
        arguments["version"] = json!(1);
        arguments["id"] = json!(self.id);
        arguments["op"] = json!(op);
        let mut line = serde_json::to_vec(&arguments).unwrap();
        line.push(b'\n');
        self.stream.get_mut().write_all(&line).unwrap();
        let mut reply = String::new();
        self.stream.read_line(&mut reply).unwrap();
        let reply: Value = serde_json::from_str(&reply).unwrap();
        assert_eq!(reply["ok"], true, "{op}: {reply}");
    }
}

/// Wait, in simulated time, until `node` serves: it does so only once its
/// boot's records were flushed, so the engine has reduced them.
fn until_node_serves(control: &mut Control) {
    for _ in 0..120 {
        let address = ("node", NODE_PORT).to_socket_addrs().ok().and_then(|mut a| a.next());
        let served = address
            .and_then(|address| TcpStream::connect_timeout(&address, Duration::from_secs(1)).ok())
            .and_then(|stream| {
                let mut line = String::new();
                BufReader::new(stream).read_line(&mut line).ok()?;
                Some(line)
            });
        if served.as_deref() == Some("flushed\n") {
            return;
        }
        control.call("sleep", json!({"duration_ns": 500_000_000u64}));
    }
    panic!("node never served");
}

fn main() -> ExitCode {
    let mode = std::env::args().nth(1).unwrap_or_default();
    match mode.as_str() {
        "abort" => std::process::abort(),
        "pass" => {
            seedbox_guest::require("probe: arithmetic holds");
            seedbox_guest::require("node: booted");
            seedbox_guest::always("probe: arithmetic holds", 2 + 2 == 4, json!({"sum": 4}));
            seedbox_guest::never("probe: two plus two is five", 2 + 2 == 5, json!(null));
            assert!(std::env::var_os("SEEDBOX_GUEST_SOCKET").is_some());
            // This machine allows crashes, so the child's is no violation.
            crash();
            // A real reboot: node's second boot numbers its sessions from 1
            // again, which must not read as broken sequencing.
            let mut control = Control::connect();
            until_node_serves(&mut control);
            control.call("crash", json!({"target": {"machine": "node"}}));
            control.call("reboot", json!({"target": {"machine": "node"}}));
            until_node_serves(&mut control);
        }
        "node" => {
            seedbox_guest::always("node: booted", true, json!(null));
            seedbox_guest::flush().expect("flush");
            let listener = TcpListener::bind(("0.0.0.0", NODE_PORT)).expect("listen");
            for mut stream in listener.incoming().flatten() {
                let _ = stream.write_all(b"flushed\n");
            }
        }
        "violate" => seedbox_guest::always("probe: arithmetic holds", 2 + 2 == 5, json!({"sum": 4})),
        "unmet" => seedbox_guest::require("probe: never evaluated"),
        "unexpected-crash" => crash(),
        "lose" => {
            seedbox_guest::always("probe: arithmetic holds", true, json!(null));
            // A record the supervisor cannot relay: over 4096 bytes, sent on
            // a session of its own. The supervisor reports it lost.
            let path = std::env::var("SEEDBOX_GUEST_SOCKET").expect("under Seedbox");
            // SAFETY: plain socket calls on a descriptor owned here, with an
            // initialized address and buffer.
            unsafe {
                let fd = libc::socket(libc::AF_UNIX, libc::SOCK_SEQPACKET, 0);
                assert!(fd >= 0);
                let mut address: libc::sockaddr_un = std::mem::zeroed();
                address.sun_family = libc::AF_UNIX as libc::sa_family_t;
                for (target, byte) in address.sun_path.iter_mut().zip(path.bytes()) {
                    *target = byte as libc::c_char;
                }
                let size = std::mem::size_of::<libc::sockaddr_un>() as libc::socklen_t;
                assert_eq!(
                    libc::connect(fd, (&address as *const libc::sockaddr_un).cast(), size),
                    0
                );
                let oversized = vec![b'x'; 5000];
                assert_eq!(
                    libc::send(fd, oversized.as_ptr().cast(), oversized.len(), 0),
                    5000
                );
                libc::close(fd);
            }
        }
        "hidden-socket" => {
            let (source, target) = (c"tmpfs", c"/run");
            // SAFETY: valid NUL-terminated strings; no mount data.
            let mounted = unsafe {
                libc::mount(
                    source.as_ptr(),
                    target.as_ptr(),
                    source.as_ptr(),
                    0,
                    std::ptr::null(),
                )
            };
            assert_eq!(mounted, 0, "{}", std::io::Error::last_os_error());
            assert!(!std::path::Path::new("/run/seedbox/guest.sock").exists());
            seedbox_guest::never("probe: two plus two is four", 2 + 2 == 4, json!({"sum": 4}));
        }
        _ => {
            eprintln!(
                "usage: property-probe pass|node|violate|unmet|unexpected-crash|lose|hidden-socket"
            );
            return ExitCode::from(2);
        }
    }
    seedbox_guest::flush().expect("flush");
    println!("PROBE {mode} done");
    ExitCode::SUCCESS
}
