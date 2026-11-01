//! Guest-side properties. Applications and oracles running under Seedbox
//! evaluate `always` and `never` properties; a violation fails the run even
//! when the oracle passes. Properties are optional to hit unless
//! [`require`]d: a required property that is never evaluated fails the run
//! once it completes. The engine keeps only failures: each violated
//! property's first example, unmet requirements and lost evidence.
//!
//! Each call sends one record to the machine's supervisor over [`SOCKET`],
//! whose path the supervisor also exports as [`SOCKET_ENV`]. Outside Seedbox
//! the variable is unset, the socket absent and every call does nothing, so
//! instrumented code runs unchanged elsewhere. Under Seedbox a socket the
//! program cannot reach (a directory mounted over `/run`, say) is not taken for
//! absence: the library connects through the supervisor's abstract socket
//! instead, and if that fails too it reports its records as lost through
//! `/dev/kmsg`, which the supervisor relays, so the run's evidence is
//! incomplete rather than silently empty.
//!
//! Evaluations are nonfatal and always arrive whole: details that would not
//! fit a record are dropped and marked truncated. A successful send means the
//! supervisor's socket accepted the record; a record that could not be sent
//! is reported to the engine as lost, which makes the run's evidence
//! incomplete rather than letting a failure go unseen. [`flush`] returns once
//! the engine has committed every earlier record from this process's
//! connection; use it before an oracle exits so its final evaluations are part
//! of the result.
//!
//! ```no_run
//! use serde_json::json;
//! seedbox_guest::require("failover exercised");
//! seedbox_guest::always("acked writes survive", true, json!({"key": 7}));
//! seedbox_guest::never("two leaders", false, json!({"leaders": 1}));
//! seedbox_guest::flush().unwrap();
//! ```
use serde_json::{json, Value};
use std::io;
use std::os::unix::net::UnixDatagram;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

/// The supervisor's socket inside every Seedbox machine.
pub const SOCKET: &str = "/run/seedbox/guest.sock";
/// Set by the supervisor to the socket's path in every application it starts.
pub const SOCKET_ENV: &str = "SEEDBOX_GUEST_SOCKET";
/// The supervisor's fallback listener in the abstract socket namespace.
const ABSTRACT_SOCKET: &str = "seedbox-guest";
/// Where the library reports records it could not deliver at all.
const KMSG: &str = "/dev/kmsg";
/// The kernel log line the supervisor relays as lost records.
const KMSG_LOST: &str = "seedbox-guest: lost ";
/// Largest record the supervisor relays.
pub const MAX_RECORD_BYTES: usize = 4096;
/// Longest property ID.
const MAX_ID_BYTES: usize = 256;
/// Largest flush reply.
const MAX_REPLY_BYTES: usize = 16 * 1024;
const FLUSH: &[u8] = br#"{"flush":{}}"#;

/// How an evaluation is judged.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    /// Violated when the condition is false.
    Always,
    /// Violated when the condition is true.
    Never,
}

impl Kind {
    fn name(self) -> &'static str {
        match self {
            Kind::Always => "always",
            Kind::Never => "never",
        }
    }
}

/// A connection to one supervisor socket: one session, whose records keep
/// their order. The free functions share a process-wide client for
/// [`SOCKET`]; a thread that must not wait behind another's flush can use a
/// client of its own. A process forked after a call opens a session of its
/// own on its next call, leaving its parent's session and unreported losses
/// to the parent.
pub struct Client {
    /// `None` is the machine's socket: [`SOCKET_ENV`]'s path, or [`SOCKET`].
    path: Option<PathBuf>,
    /// For a client of its own path: where it falls back, as under Seedbox.
    fallback: Option<Fallback>,
    inner: Mutex<Inner>,
}

/// Where a session connects. With a fallback (under Seedbox), an unreachable
/// path is a failure, never absence.
struct Target {
    path: PathBuf,
    fallback: Option<Fallback>,
}

struct Fallback {
    abstract_name: String,
    kmsg: PathBuf,
}

struct Inner {
    /// The process this state belongs to, from its first call: a child
    /// forked after that inherits the state but must not use it.
    owner: Option<u32>,
    state: State,
    /// Records this client failed to send, reported as one `lost` record
    /// before anything else once a send succeeds again.
    lost: u64,
    lost_reason: String,
    reply_timeout: Duration,
    /// The loss was already reported through the kernel log.
    logged: bool,
}

enum State {
    Unopened,
    /// `sent` counts this session's messages, the supervisor's `seq`.
    Connected {
        socket: UnixDatagram,
        sent: u64,
    },
    /// Not running under Seedbox: calls do nothing.
    Absent,
}

/// How long [`Client::flush`] waits for the engine's reply, in guest time.
pub const REPLY_TIMEOUT: Duration = Duration::from_secs(60);

impl Client {
    /// The machine's supervisor socket.
    pub const fn machine() -> Self {
        Self {
            path: None,
            fallback: None,
            inner: Mutex::new(Inner::new()),
        }
    }

    pub fn new(path: impl AsRef<Path>) -> Self {
        Self {
            path: Some(path.as_ref().to_owned()),
            fallback: None,
            inner: Mutex::new(Inner::new()),
        }
    }

    /// A client of `path` that behaves as under Seedbox: an unreachable path
    /// falls back to the abstract `name`, then reports to `kmsg`.
    #[cfg(test)]
    fn with_fallback(path: &Path, name: &str, kmsg: &Path) -> Self {
        Self {
            path: Some(path.to_owned()),
            fallback: Some(Fallback {
                abstract_name: name.into(),
                kmsg: kmsg.to_owned(),
            }),
            inner: Mutex::new(Inner::new()),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn target(&self) -> Target {
        match &self.path {
            Some(path) => Target {
                path: path.clone(),
                fallback: self.fallback.as_ref().map(|f| Fallback {
                    abstract_name: f.abstract_name.clone(),
                    kmsg: f.kmsg.clone(),
                }),
            },
            None => match std::env::var_os(SOCKET_ENV) {
                Some(path) => Target {
                    path: PathBuf::from(path),
                    fallback: Some(Fallback {
                        abstract_name: ABSTRACT_SOCKET.into(),
                        kmsg: KMSG.into(),
                    }),
                },
                None => Target {
                    path: SOCKET.into(),
                    fallback: None,
                },
            },
        }
    }

    /// Send one record, which [`fit`] always brings within the limit.
    fn send(&self, record: Value) -> io::Result<()> {
        let bytes = fit(record);
        let target = self.target();
        let mut inner = self.lock();
        inner.send(&target, &bytes).map(drop)
    }

    /// Send `request`, then wait for the reply that confirms it: `{key:
    /// {"seq": N, ...}}`, where N is the request's own sequence number. A
    /// missing, late or mismatched reply is an error and closes the
    /// session, so a late reply is never mistaken for a later one's.
    fn request(&self, request: &[u8], key: &str) -> io::Result<Option<Value>> {
        let target = self.target();
        let mut inner = self.lock();
        let Some(seq) = inner.send(&target, request)? else {
            return Ok(None);
        };
        let State::Connected { socket, .. } = &inner.state else {
            unreachable!("a sent request has a session");
        };
        let mut reply = vec![0u8; MAX_REPLY_BYTES];
        let received = socket.recv(&mut reply);
        let invalid = || {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unexpected {key} reply"),
            )
        };
        let body = match received {
            Ok(size) => reply_body(&reply[..size], key, seq).ok_or_else(invalid),
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) =>
            {
                Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!("no {key} reply"),
                ))
            }
            Err(error) => Err(error),
        };
        if body.is_err() {
            inner.state = State::Unopened;
        }
        body.map(Some)
    }

    /// Wait until the engine has committed and reduced every earlier record
    /// of this client's session: its reply crosses the same commit boundary.
    /// Records this client failed to send are reported first, so they count
    /// as lost evidence. Fails if no reply confirms this flush within
    /// [`REPLY_TIMEOUT`].
    pub fn flush(&self) -> io::Result<()> {
        self.request(FLUSH, "flush").map(drop)
    }

    /// Require `id` to be evaluated at least once during the run; see
    /// [`require`].
    pub fn require(&self, id: &str) {
        self.emit(require_record(id));
    }

    /// Evaluate a property that must always hold; see [`always`].
    pub fn always(&self, id: &str, ok: bool, details: Value) {
        self.emit(assert_record(id, Kind::Always, ok, details));
    }

    /// Evaluate a property that must never happen; see [`never`].
    pub fn never(&self, id: &str, condition: bool, details: Value) {
        self.emit(assert_record(id, Kind::Never, condition, details));
    }

    /// Evaluations never fail the caller. A record that cannot be sent is
    /// counted and reported to the engine as lost once the socket accepts
    /// again (the next call reconnects as a new session), which makes the
    /// run's evidence incomplete; the first failure is also printed on
    /// stderr.
    fn emit(&self, record: Value) {
        if let Err(error) = self.send(record) {
            static REPORTED: std::sync::Once = std::sync::Once::new();
            REPORTED.call_once(|| eprintln!("seedbox-guest: property record not sent: {error}"));
        }
    }
}

impl Inner {
    const fn new() -> Self {
        Self {
            owner: None,
            state: State::Unopened,
            lost: 0,
            lost_reason: String::new(),
            reply_timeout: REPLY_TIMEOUT,
            logged: false,
        }
    }

    /// Send one message on the session, opening it first and reporting
    /// earlier losses before it. Returns its sequence number, or `None`
    /// outside Seedbox. A failure counts the message as lost and closes the
    /// session.
    fn send(&mut self, target: &Target, bytes: &[u8]) -> io::Result<Option<u64>> {
        self.leave_inherited_session();
        let result = self.try_send(target, bytes);
        if let Err(error) = &result {
            self.lost += 1;
            self.lost_reason = error.to_string();
            self.state = State::Unopened;
            if let (Some(fallback), false) = (&target.fallback, self.logged) {
                // Nothing reaches the supervisor's sockets: say so once on
                // the kernel log, which the supervisor reads regardless.
                self.logged = log_lost(&fallback.kmsg, self.lost, &self.lost_reason);
            }
        }
        result
    }

    /// A forked child inherits its parent's state: a connected socket and
    /// sequence count, or losses not yet reported. Sending on the socket
    /// would interleave two processes' records under one sequence, and
    /// either could read the other's flush reply; reporting the losses would
    /// count them twice, since the parent still reports them. So the child
    /// closes its copy of any socket unused, which leaves the parent's
    /// session open, and starts afresh: its next record opens a session of
    /// its own, with no lost count and nothing yet on the kernel log.
    fn leave_inherited_session(&mut self) {
        let pid = std::process::id();
        if self.owner.is_some_and(|owner| owner != pid) {
            *self = Self {
                reply_timeout: self.reply_timeout,
                ..Self::new()
            };
        }
        self.owner = Some(pid);
    }

    fn try_send(&mut self, target: &Target, bytes: &[u8]) -> io::Result<Option<u64>> {
        if let State::Unopened = self.state {
            let socket = match (connect(&target.path), &target.fallback) {
                (Ok(socket), _) => Some(socket),
                (Err(e), None)
                    if matches!(e.raw_os_error(), Some(libc::ENOENT | libc::ENOTDIR)) =>
                {
                    None
                }
                (Err(e), None) => return Err(e),
                // Under Seedbox the socket exists; this program cannot reach
                // its path, so try the supervisor's abstract listener.
                (Err(e), Some(fallback)) => match connect_abstract(&fallback.abstract_name) {
                    Ok(socket) => Some(socket),
                    Err(_) => return Err(e),
                },
            };
            self.state = match socket {
                Some(socket) => {
                    socket.set_read_timeout(Some(self.reply_timeout))?;
                    State::Connected { socket, sent: 0 }
                }
                None => State::Absent,
            };
        }
        let State::Connected { socket, sent, .. } = &mut self.state else {
            return Ok(None);
        };
        if self.lost > 0 {
            send_message(socket, &lost_record(self.lost, &self.lost_reason))?;
            *sent += 1;
            self.lost = 0;
        }
        send_message(socket, bytes)?;
        *sent += 1;
        Ok(Some(*sent))
    }
}

/// Send without raising SIGPIPE when the supervisor has closed the session.
#[cfg(target_os = "linux")]
fn send_message(socket: &UnixDatagram, bytes: &[u8]) -> io::Result<()> {
    use std::os::fd::AsRawFd;
    // SAFETY: the descriptor is live and the buffer is valid for its length.
    let sent = unsafe {
        libc::send(
            socket.as_raw_fd(),
            bytes.as_ptr().cast(),
            bytes.len(),
            libc::MSG_NOSIGNAL,
        )
    };
    if sent < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn send_message(socket: &UnixDatagram, bytes: &[u8]) -> io::Result<()> {
    socket.send(bytes).map(drop)
}

// The wire format, written here rather than shared with
// seedbox-engine-protocol so that guest programs link only libc and
// serde_json. Tests decode every record with the protocol's own parser.

fn require_record(id: &str) -> Value {
    json!({"require": {"id": id}})
}

fn assert_record(id: &str, kind: Kind, condition: bool, details: Value) -> Value {
    let body = json!({"id": id, "kind": kind.name(), "condition": condition});
    json!({ "assert": with_details(body, details) })
}

fn lost_record(count: u64, reason: &str) -> Vec<u8> {
    let reason: String = reason.chars().take(MAX_ID_BYTES).collect();
    serde_json::to_vec(&json!({"lost": {"reason": reason, "count": count}})).expect("JSON")
}

/// The record's bytes, within [`MAX_RECORD_BYTES`] whatever its contents, so
/// that every evaluation reaches the engine. Oversized details go first (an
/// assertion says it lost them); an ID or name still too long is cut to one
/// byte past the limit, which the engine reports as lost evidence.
fn fit(mut record: Value) -> Vec<u8> {
    let encode = |record: &Value| serde_json::to_vec(record).expect("JSON values serialize");
    let mut bytes = encode(&record);
    if bytes.len() <= MAX_RECORD_BYTES {
        return bytes;
    }
    if let Some(body) = record.as_object_mut().and_then(|r| r.values_mut().next()) {
        if let Some(body) = body.as_object_mut() {
            if body.remove("details").is_some() && body.contains_key("condition") {
                body.insert("details_truncated".into(), true.into());
            }
            if let Some(Value::String(text)) = body.get_mut("id") {
                let mut end = text.len().min(MAX_ID_BYTES + 1);
                while !text.is_char_boundary(end) {
                    end -= 1;
                }
                text.truncate(end);
            }
        }
    }
    bytes = encode(&record);
    debug_assert!(bytes.len() <= MAX_RECORD_BYTES);
    bytes
}

/// The body of a `{key: {"seq": seq, ...}}` reply.
fn reply_body(reply: &[u8], key: &str, seq: u64) -> Option<Value> {
    let mut reply: Value = serde_json::from_slice(reply).ok()?;
    let body = reply.get_mut(key)?.take();
    (body["seq"].as_u64()? == seq).then_some(body)
}

/// Null details are omitted from the record.
fn with_details(mut body: Value, details: Value) -> Value {
    if !details.is_null() {
        body["details"] = details;
    }
    body
}

/// Report `count` undeliverable records on the kernel log, as
/// `seedbox-guest: lost COUNT property records: REASON`. Returns whether the
/// line was written.
fn log_lost(kmsg: &Path, count: u64, reason: &str) -> bool {
    use std::io::Write;
    let reason: String = reason
        .chars()
        .filter(|c| !c.is_control())
        .take(MAX_ID_BYTES)
        .collect();
    let line = format!("{KMSG_LOST}{count} property records: {reason}\n");
    std::fs::OpenOptions::new()
        .write(true)
        .open(kmsg)
        .and_then(|mut file| file.write_all(line.as_bytes()))
        .is_ok()
}

/// The supervisor socket exists only in Linux guests.
#[cfg(not(target_os = "linux"))]
fn connect(_: &Path) -> io::Result<UnixDatagram> {
    Err(io::Error::from_raw_os_error(libc::ENOENT))
}

#[cfg(not(target_os = "linux"))]
fn connect_abstract(_: &str) -> io::Result<UnixDatagram> {
    Err(io::Error::from_raw_os_error(libc::ENOENT))
}

#[cfg(target_os = "linux")]
fn connect(path: &Path) -> io::Result<UnixDatagram> {
    use std::os::unix::ffi::OsStrExt;
    connect_address(path.as_os_str().as_bytes(), false)
}

/// The abstract namespace has no files or permissions to get in the way.
#[cfg(target_os = "linux")]
fn connect_abstract(name: &str) -> io::Result<UnixDatagram> {
    connect_address(name.as_bytes(), true)
}

#[cfg(target_os = "linux")]
fn connect_address(bytes: &[u8], abstract_name: bool) -> io::Result<UnixDatagram> {
    use std::os::fd::{FromRawFd, OwnedFd};
    // SAFETY: an all-zero sockaddr_un is a valid initial value.
    let mut address: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    let offset = usize::from(abstract_name);
    if bytes.len() + offset >= address.sun_path.len() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "guest socket path too long",
        ));
    }
    address.sun_family = libc::AF_UNIX as libc::sa_family_t;
    for (target, byte) in address.sun_path[offset..].iter_mut().zip(bytes) {
        *target = *byte as libc::c_char;
    }
    // An abstract name is exactly its bytes after the leading NUL.
    let size = if abstract_name {
        std::mem::offset_of!(libc::sockaddr_un, sun_path) + 1 + bytes.len()
    } else {
        std::mem::size_of::<libc::sockaddr_un>()
    };
    // SAFETY: plain socket creation; the descriptor is owned immediately.
    let fd = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC, 0) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `fd` is a fresh descriptor this function owns.
    let owned = unsafe { OwnedFd::from_raw_fd(fd) };
    // SAFETY: the address is initialized and its size is passed exactly.
    let result = unsafe {
        libc::connect(
            fd,
            (&address as *const libc::sockaddr_un).cast(),
            size as libc::socklen_t,
        )
    };
    if result != 0 {
        return Err(io::Error::last_os_error());
    }
    // send/recv on a connected SOCK_SEQPACKET socket keep message boundaries.
    Ok(UnixDatagram::from(owned))
}

static CLIENT: Client = Client::machine();

/// Require `id` to be evaluated, by [`always`] or [`never`], at least once
/// during the run, from any machine. Call it at startup for each property
/// the test must exercise, such as a failover path: one never evaluated by
/// the end fails the run. Properties not required may go unevaluated.
pub fn require(id: &str) {
    CLIENT.require(id);
}

/// `ok` must hold every time this is evaluated: one false evaluation
/// violates the property and fails the run. `details` describe the
/// evaluation; the first violation's are kept as its example.
pub fn always(id: &str, ok: bool, details: Value) {
    CLIENT.always(id, ok, details);
}

/// `condition` must never hold: one true evaluation violates the property
/// and fails the run. `never(id, true, ..)` marks a point that must not be
/// reached.
pub fn never(id: &str, condition: bool, details: Value) {
    CLIENT.never(id, condition, details);
}

/// Wait until the engine has committed and reduced every earlier record
/// from this process. Succeeds immediately outside Seedbox. Call it before
/// an oracle exits so its final evaluations count.
pub fn flush() -> io::Result<()> {
    CLIENT.flush()
}

#[cfg(test)]
mod tests;
