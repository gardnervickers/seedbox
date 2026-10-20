//! Private, bounded process/pipe boundary for the Python SDK contract target.
//! The checker command runner cannot be reused here: it owns process-global
//! signals and whole-command capture, not an interactive anonymous-pipe peer.
use seedbox_test_time::HangGuard;
use serde_json::{json, Value};
use std::{
    io::{Read, Write},
    os::{fd::AsRawFd, unix::process::CommandExt},
    process::{Child, ChildStdin, ChildStdout, Command, Stdio},
    time::Duration,
};

const LIMIT: usize = 256 * 1024;
// Host time is an I/O hang guard, never a post-completion speed oracle.
pub const DEADLINE: Duration = Duration::from_secs(30);

pub struct Sdk {
    child: Child,
    input: ChildStdin,
    output: ChildStdout,
    pending: Vec<u8>,
    pub guard: HangGuard,
    injected_remaining: Option<Duration>,
}
impl Sdk {
    pub fn new() -> Self {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let mut child = seedbox_test::process::spawn(
            Command::new("python3")
                .args(["-B", "-u"])
                .arg(root.join("crates/seedbox-engine-protocol/tests/support/python_sdk.py"))
                .env_clear()
                .env("PATH", std::env::var_os("PATH").unwrap_or_default())
                .env("PYTHONPATH", root.join("python"))
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::inherit())
                .process_group(0),
        )
        .expect("start production Python SDK adapter (nix develop supplies python3)");
        let input = child.stdin.take().expect("adapter stdin");
        let output = child.stdout.take().expect("adapter stdout");
        // Install RAII ownership before any fallible fcntl or pipe exchange.
        let sdk = Self {
            child,
            input,
            output,
            pending: Vec::new(),
            guard: HangGuard::new("Python SDK adapter case I/O", DEADLINE),
            injected_remaining: None,
        };
        for fd in [sdk.input.as_raw_fd(), sdk.output.as_raw_fd()] {
            let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
            assert!(
                flags >= 0,
                "read pipe flags: {}",
                std::io::Error::last_os_error()
            );
            assert_eq!(
                unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) },
                0
            );
        }
        sdk
    }
    fn ready(&self, fd: i32, events: i16) {
        loop {
            let remaining = self
                .injected_remaining
                .or_else(|| self.guard.remaining().ok())
                .filter(|remaining| !remaining.is_zero())
                .expect("SDK child exceeded 30-second case deadline");
            let mut poll = libc::pollfd {
                fd,
                events,
                revents: 0,
            };
            let rc = unsafe {
                // Bound actual pipe readiness with the shared case guard; silence never satisfies a call.
                libc::poll(
                    &mut poll,
                    1,
                    remaining.as_millis().clamp(1, i32::MAX as u128) as i32,
                )
            };
            if rc < 0 && std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            assert!(
                rc > 0,
                "SDK pipe deadline/poll failure: {}",
                std::io::Error::last_os_error()
            );
            return;
        }
    }
    pub fn exchange(&mut self, command: Value) -> Value {
        self.send_command(command);
        self.observation()
    }
    fn send_command(&mut self, command: Value) {
        let mut bytes = serde_json::to_vec(&command).unwrap();
        bytes.push(b'\n');
        assert!(bytes.len() <= LIMIT, "bounded adapter command");
        let mut remaining = bytes.as_slice();
        while !remaining.is_empty() {
            self.ready(self.input.as_raw_fd(), libc::POLLOUT);
            match self.input.write(remaining) {
                Ok(0) => panic!("adapter input closed"),
                Ok(n) => remaining = &remaining[n..],
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                    ) => {}
                Err(e) => panic!("adapter input: {e}"),
            }
        }
    }
    fn observation(&mut self) -> Value {
        loop {
            if let Some(end) = self.pending.iter().position(|byte| *byte == b'\n') {
                let result =
                    serde_json::from_slice(&self.pending[..end]).expect("adapter JSON observation");
                self.pending.drain(..=end);
                return result;
            }
            assert!(self.pending.len() < LIMIT, "bounded adapter observation");
            self.ready(self.output.as_raw_fd(), libc::POLLIN);
            let mut chunk = [0; 4096];
            match self.output.read(&mut chunk) {
                Ok(0) => panic!("adapter exited before observation"),
                Ok(n) => self.pending.extend_from_slice(&chunk[..n]),
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                    ) => {}
                Err(e) => panic!("adapter output: {e}"),
            }
        }
    }
    pub fn open(response: Vec<u8>) -> Self {
        let mut sdk = Self::new();
        let opened = sdk.exchange(json!({"action":"open", "response":response}));
        assert!(opened.get("error").is_none(), "{opened}");
        assert_eq!(opened["mode"], "rb"); // Old Peer.makefile assertion.
        sdk
    }
    pub fn call(&mut self, op: &str, args: Value, kwargs: Value) -> Value {
        self.exchange(json!({"action":"call", "op":op, "args":args, "kwargs":kwargs, "save":op}))
    }
    pub fn close(&mut self) -> Value {
        let out = self.exchange(json!({"action":"close"}));
        // A failed __exit__ must fail Rust just as leaving Python's `with`
        // failed the original suite, even if the caller only needs requests.
        assert!(out.get("error").is_none(), "SDK context exit: {out}");
        out
    }
}
impl Drop for Sdk {
    fn drop(&mut self) {
        // Never try_wait/reap before signalling: reserve the leader PID to
        // prevent killing a reused process group. No child-created descendants.
        unsafe {
            libc::kill(-(self.child.id() as i32), libc::SIGKILL);
        }
        let status = self.child.wait();
        if !std::thread::panicking() {
            assert!(status.is_ok(), "reap SDK child: {status:?}");
        }
    }
}

pub fn reply(id: u64, result: Value, changes: Value) -> Vec<u8> {
    let mut value = json!({"version":1, "id":id, "ok":true, "result":result,
        "requested_at_ns":10, "admitted_at_ns":20, "effect_at_ns":30, "replied_at_ns":40});
    for (key, item) in changes.as_object().unwrap() {
        value[key] = item.clone();
    }
    let mut bytes = serde_json::to_vec(&value).unwrap();
    bytes.push(b'\n');
    bytes
}
pub fn replies(values: &[(u64, Value, Value)]) -> Vec<u8> {
    values
        .iter()
        .flat_map(|(id, result, changes)| reply(*id, result.clone(), changes.clone()))
        .collect()
}
pub fn ok(value: &Value) -> &Value {
    assert!(value.get("error").is_none(), "{value}");
    &value["value"]
}
pub fn error(value: &Value, expected: &str) {
    assert_eq!(value["error"]["type"], expected, "{value}");
    assert_eq!(value["error"]["public_type_identity"], true);
}
pub fn typed(value: &Value, expected: &str) {
    assert_eq!(value["type"], expected);
    assert_eq!(value["public_type_identity"], true);
    assert_eq!(value["reconstructed_equal"], true);
}
pub fn effect(value: &Value, requested: u64, admitted: u64, at: u64, replied: u64) {
    typed(value, "Effect");
    assert_eq!(
        value["fields"],
        json!({"requested_at_ns":requested,"admitted_at_ns":admitted,"effect_at_ns":at,"replied_at_ns":replied})
    );
}
pub fn reference(op: &str) -> Value {
    json!({"$ref":op})
}
pub fn machine(name: &str) -> Value {
    json!({"$machine":name})
}

#[test]
fn blocked_real_sdk_call_has_deadline_and_child_is_reaped_on_unwind() {
    use std::{io::BufRead, os::unix::net::UnixListener};
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("blocked.sock");
    let listener = UnixListener::bind(&path).unwrap();
    listener.set_nonblocking(true).unwrap();
    let mut sdk = Sdk::new();
    let connected = sdk.exchange(json!({"action":"connect","path":path}));
    assert!(connected.get("error").is_none(), "{connected}");
    let (peer, _) = listener.accept().expect("real SDK connected");
    // BSD-derived hosts let an accepted socket inherit non-blocking mode.
    peer.set_nonblocking(false).unwrap();
    // A real request receipt, not silence or a sleep, establishes SDK progress.
    // The socket timeout is only a hang guard if that progress never happens.
    // Bound receipt of the actual SDK request before deliberately exhausting its I/O budget.
    peer.set_read_timeout(Some(sdk.guard.remaining().unwrap()))
        .unwrap();
    sdk.send_command(json!({"action":"call","op":"stop","args":["server"],"kwargs":{}}));
    let mut reader = std::io::BufReader::new(peer.take(16_385));
    let mut frame = Vec::new();
    reader.read_until(b'\n', &mut frame).unwrap();
    assert_eq!(frame.last(), Some(&b'\n'));
    assert_eq!(
        serde_json::from_slice::<Value>(&frame).unwrap(),
        json!({"version":1,"id":1,"op":"stop","target":{"service":"server"}})
    );
    // Keep the peer open without replying. Inject an exhausted remaining budget
    // only AFTER its request arrived: the same observation deadline branch must
    // reject this pending call regardless of host scheduling speed.
    sdk.injected_remaining = Some(Duration::ZERO);
    let pid = sdk.child.id() as i32;
    let failure = std::panic::catch_unwind(move || {
        sdk.observation();
    });
    let panic = failure.expect_err("pending SDK read must reject the injected expired deadline");
    let message = panic
        .downcast_ref::<String>()
        .map(String::as_str)
        .or_else(|| panic.downcast_ref::<&str>().copied());
    assert_eq!(message, Some("SDK child exceeded 30-second case deadline"));
    let mut status = 0;
    assert_eq!(
        unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) },
        -1
    );
    assert_eq!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(libc::ECHILD)
    );
    assert_eq!(unsafe { libc::kill(-pid, 0) }, -1);
    assert_eq!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(libc::ESRCH)
    );
}

#[test]
fn translation_value_error_cannot_satisfy_sdk_value_error_expectation() {
    let mut sdk = Sdk::open(vec![]);
    let pid = sdk.child.id() as i32;
    // exchange() supplies the I/O hang guard; success is exact EOF plus the
    // reaping predicates below, never an elapsed host-time comparison.
    let failure = std::panic::catch_unwind(move || {
        let observation = sdk.call("sleep", json!([{"$integer":"not-an-integer"}]), json!({}));
        // The old boundary incorrectly reported the adapter's int() failure
        // as an SDK ValueError, allowing this exact assertion to pass.
        error(&observation, "ValueError");
    });
    let panic = failure.expect_err("translation failure must not become an SDK error observation");
    let message = panic
        .downcast_ref::<String>()
        .map(String::as_str)
        .or_else(|| panic.downcast_ref::<&str>().copied());
    assert_eq!(message, Some("adapter exited before observation"));
    let mut status = 0;
    assert_eq!(
        unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) },
        -1
    );
    assert_eq!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(libc::ECHILD)
    );
    assert_eq!(unsafe { libc::kill(-pid, 0) }, -1);
    assert_eq!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(libc::ESRCH)
    );
}
