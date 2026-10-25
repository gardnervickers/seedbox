//! Guest-side property SDK. Applications and oracles running under Seedbox
//! declare and evaluate properties; the engine reduces them into a property
//! assessment reported beside the oracle verdict, and a violated property
//! fails the run even when the oracle passes.
//!
//! Each call sends one record to the machine's supervisor over [`SOCKET`].
//! Outside Seedbox the socket is absent and every call does nothing, so
//! instrumented code runs unchanged elsewhere. The engine does not serve the
//! socket yet, so for now every call does nothing under Seedbox too.
//!
//! Assertions are nonfatal and always arrive whole: details that would not
//! fit a record are dropped and marked truncated. A successful send means the
//! supervisor's socket accepted the record; a record that could not be sent
//! is reported to the engine as lost, which makes the run's evidence
//! incomplete rather than letting a failure go unseen. [`flush`] returns once
//! the engine has committed every earlier record from this process's
//! connection; use it before an oracle exits so its final assertions are part
//! of the result.
//!
//! ```no_run
//! use serde_json::json;
//! seedbox_sdk::declare("acknowledged writes survive", seedbox_sdk::Kind::Always);
//! seedbox_sdk::always("acknowledged writes survive", true, json!({"key": 7}));
//! seedbox_sdk::sometimes("leader changed", false, json!(null));
//! seedbox_sdk::flush().unwrap();
//! ```
use serde_json::{json, Value};
use std::io;
use std::os::unix::net::UnixDatagram;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// The supervisor's socket inside every Seedbox machine.
pub const SOCKET: &str = "/run/seedbox/sdk.sock";
/// Largest record the supervisor relays.
pub const MAX_RECORD_BYTES: usize = 4096;
/// Longest property ID or event name.
const MAX_ID_BYTES: usize = 256;
/// Largest flush or summary reply.
const MAX_REPLY_BYTES: usize = 16 * 1024;
const FLUSH: &[u8] = br#"{"flush":{}}"#;
const SUMMARY: &[u8] = br#"{"summary":{}}"#;

/// One property's evaluations so far.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PropertyCounts {
    pub passes: u64,
    pub failures: u64,
}

/// The run's properties as the engine has reduced them, from [`summary`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Summary {
    pub properties: std::collections::BTreeMap<String, PropertyCounts>,
    /// More properties exist than the reply lists (at most 32, by ID).
    pub truncated: bool,
}

/// How a property is judged.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// Every evaluation holds, and at least one happens.
    Always,
    /// Every evaluation holds; never evaluating it is fine.
    AlwaysOrUnreachable,
    /// At least one evaluation holds.
    Sometimes,
    /// Reached at least once.
    Reachable,
    /// Never reached.
    Unreachable,
}

impl Kind {
    fn name(self) -> &'static str {
        match self {
            Kind::Always => "always",
            Kind::AlwaysOrUnreachable => "always_or_unreachable",
            Kind::Sometimes => "sometimes",
            Kind::Reachable => "reachable",
            Kind::Unreachable => "unreachable",
        }
    }
}

/// A connection to one supervisor socket: one session, whose records keep
/// their order. The free functions share a process-wide client for
/// [`SOCKET`]; a thread that must not wait behind another's flush can use a
/// client of its own.
pub struct Client {
    /// `None` is [`SOCKET`].
    path: Option<PathBuf>,
    inner: Mutex<Inner>,
}

struct Inner {
    state: State,
    /// Records this client failed to send, reported as one `lost` record
    /// before anything else once a send succeeds again.
    lost: u64,
    lost_reason: String,
    reply_timeout: Duration,
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

/// How long [`Client::flush`] and [`Client::summary`] wait for the engine's
/// reply, in guest time.
pub const REPLY_TIMEOUT: Duration = Duration::from_secs(60);

impl Client {
    /// The machine's supervisor socket.
    pub const fn machine() -> Self {
        Self {
            path: None,
            inner: Mutex::new(Inner::new()),
        }
    }

    pub fn new(path: impl AsRef<Path>) -> Self {
        Self {
            path: Some(path.as_ref().to_owned()),
            inner: Mutex::new(Inner::new()),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn path(&self) -> &Path {
        self.path.as_deref().unwrap_or(Path::new(SOCKET))
    }

    /// Send one record, which [`fit`] always brings within the limit.
    fn send(&self, record: Value) -> io::Result<()> {
        let bytes = fit(record);
        let path = self.path().to_owned();
        let mut inner = self.lock();
        inner.send(&path, &bytes).map(drop)
    }

    /// Send `request`, then wait for the reply that confirms it: `{key:
    /// {"seq": N, ...}}`, where N is the request's own sequence number. A
    /// missing, late or mismatched reply is an error and closes the
    /// session, so a late reply is never mistaken for a later one's.
    fn request(&self, request: &[u8], key: &str) -> io::Result<Option<Value>> {
        let path = self.path().to_owned();
        let mut inner = self.lock();
        let Some(seq) = inner.send(&path, request)? else {
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

    /// As [`Client::flush`], then the run's properties as the engine has
    /// reduced them at that point. Empty outside Seedbox.
    pub fn summary(&self) -> io::Result<Summary> {
        match self.request(SUMMARY, "summary")? {
            Some(body) => parse_summary(&body).ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "unexpected summary reply")
            }),
            None => Ok(Summary::default()),
        }
    }

    /// Declare a property so it is reported even if never evaluated.
    pub fn declare(&self, id: &str, kind: Kind) {
        self.emit(declare_record(id, kind));
    }

    /// Evaluate a property of the given kind. Details too large for one
    /// record are dropped and marked truncated; the evaluation still goes.
    pub fn assert(&self, id: &str, kind: Kind, condition: bool, details: Value) {
        self.emit(assert_record(id, kind, condition, details));
    }

    /// Record a named workload event.
    pub fn event(&self, name: &str, details: Value) {
        self.emit(event_record(name, details));
    }

    /// Mark the workload on this machine as set up.
    pub fn setup_complete(&self, details: Value) {
        self.emit(setup_record(details));
    }

    /// Assertions never fail the caller. A record that cannot be sent is
    /// counted and reported to the engine as lost once the socket accepts
    /// again (the next call reconnects as a new session), which makes the
    /// run's evidence incomplete; the first failure is also printed on
    /// stderr.
    fn emit(&self, record: Value) {
        if let Err(error) = self.send(record) {
            static REPORTED: std::sync::Once = std::sync::Once::new();
            REPORTED.call_once(|| eprintln!("seedbox-sdk: property record not sent: {error}"));
        }
    }
}

impl Inner {
    const fn new() -> Self {
        Self {
            state: State::Unopened,
            lost: 0,
            lost_reason: String::new(),
            reply_timeout: REPLY_TIMEOUT,
        }
    }

    /// Send one message on the session, opening it first and reporting
    /// earlier losses before it. Returns its sequence number, or `None`
    /// outside Seedbox. A failure counts the message as lost and closes the
    /// session.
    fn send(&mut self, path: &Path, bytes: &[u8]) -> io::Result<Option<u64>> {
        let result = self.try_send(path, bytes);
        if let Err(error) = &result {
            self.lost += 1;
            self.lost_reason = error.to_string();
            self.state = State::Unopened;
        }
        result
    }

    fn try_send(&mut self, path: &Path, bytes: &[u8]) -> io::Result<Option<u64>> {
        if let State::Unopened = self.state {
            self.state = match connect(path) {
                Ok(socket) => {
                    socket.set_read_timeout(Some(self.reply_timeout))?;
                    State::Connected { socket, sent: 0 }
                }
                Err(e) if matches!(e.raw_os_error(), Some(libc::ENOENT | libc::ENOTDIR)) => {
                    State::Absent
                }
                Err(e) => return Err(e),
            };
        }
        let State::Connected { socket, sent } = &mut self.state else {
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

fn declare_record(id: &str, kind: Kind) -> Value {
    json!({"declare": {"id": id, "kind": kind.name()}})
}

fn assert_record(id: &str, kind: Kind, condition: bool, details: Value) -> Value {
    let body = json!({"id": id, "kind": kind.name(), "condition": condition});
    json!({ "assert": with_details(body, details) })
}

fn event_record(name: &str, details: Value) -> Value {
    json!({ "event": with_details(json!({"name": name}), details) })
}

fn setup_record(details: Value) -> Value {
    json!({ "setup_complete": with_details(json!({}), details) })
}

fn lost_record(count: u64, reason: &str) -> Vec<u8> {
    let reason: String = reason.chars().take(MAX_ID_BYTES).collect();
    serde_json::to_vec(&json!({"lost": {"reason": reason, "count": count}})).expect("JSON")
}

/// The record's bytes, within [`MAX_RECORD_BYTES`] whatever its contents, so
/// that every evaluation reaches the engine. Oversized details go first (an
/// assertion says it lost them); an ID or name still too long is cut to one
/// byte past the limit, which the engine reports as an evidence error.
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
            for key in ["id", "name"] {
                if let Some(Value::String(text)) = body.get_mut(key) {
                    let mut end = text.len().min(MAX_ID_BYTES + 1);
                    while !text.is_char_boundary(end) {
                        end -= 1;
                    }
                    text.truncate(end);
                }
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

fn parse_summary(body: &Value) -> Option<Summary> {
    let mut summary = Summary::default();
    for (id, counts) in body["properties"].as_object()? {
        summary.properties.insert(
            id.clone(),
            PropertyCounts {
                passes: counts["passes"].as_u64()?,
                failures: counts["failures"].as_u64()?,
            },
        );
    }
    summary.truncated = body["truncated"].as_bool()?;
    Some(summary)
}

/// Null details are omitted from the record.
fn with_details(mut body: Value, details: Value) -> Value {
    if !details.is_null() {
        body["details"] = details;
    }
    body
}

/// The supervisor socket exists only in Linux guests.
#[cfg(not(target_os = "linux"))]
fn connect(_: &Path) -> io::Result<UnixDatagram> {
    Err(io::Error::from_raw_os_error(libc::ENOENT))
}

#[cfg(target_os = "linux")]
fn connect(path: &Path) -> io::Result<UnixDatagram> {
    use std::os::fd::{FromRawFd, OwnedFd};
    use std::os::unix::ffi::OsStrExt;
    let bytes = path.as_os_str().as_bytes();
    // SAFETY: an all-zero sockaddr_un is a valid initial value.
    let mut address: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    if bytes.len() >= address.sun_path.len() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "SDK socket path too long",
        ));
    }
    address.sun_family = libc::AF_UNIX as libc::sa_family_t;
    for (target, byte) in address.sun_path.iter_mut().zip(bytes) {
        *target = *byte as libc::c_char;
    }
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
            std::mem::size_of::<libc::sockaddr_un>() as libc::socklen_t,
        )
    };
    if result != 0 {
        return Err(io::Error::last_os_error());
    }
    // send/recv on a connected SOCK_SEQPACKET socket keep message boundaries.
    Ok(UnixDatagram::from(owned))
}

static CLIENT: Client = Client::machine();

/// Declare a property so it is reported even if never evaluated.
pub fn declare(id: &str, kind: Kind) {
    CLIENT.declare(id, kind);
}
/// Every evaluation must hold, and at least one must happen.
pub fn always(id: &str, condition: bool, details: Value) {
    CLIENT.assert(id, Kind::Always, condition, details);
}
/// Every evaluation must hold; never reaching it is fine.
pub fn always_or_unreachable(id: &str, condition: bool, details: Value) {
    CLIENT.assert(id, Kind::AlwaysOrUnreachable, condition, details);
}
/// At least one evaluation should hold.
pub fn sometimes(id: &str, condition: bool, details: Value) {
    CLIENT.assert(id, Kind::Sometimes, condition, details);
}
/// This point should be reached at least once.
pub fn reachable(id: &str, details: Value) {
    CLIENT.assert(id, Kind::Reachable, true, details);
}
/// This point must never be reached.
pub fn unreachable(id: &str, details: Value) {
    CLIENT.assert(id, Kind::Unreachable, true, details);
}
/// Record a named workload event.
pub fn event(name: &str, details: Value) {
    CLIENT.event(name, details);
}
/// Mark the workload on this machine as set up.
pub fn setup_complete(details: Value) {
    CLIENT.setup_complete(details);
}
/// Wait until the engine has committed and reduced every earlier record
/// from this process. Succeeds immediately outside Seedbox.
pub fn flush() -> io::Result<()> {
    CLIENT.flush()
}

/// Flush, then the run's properties as the engine has reduced them so far:
/// a workload can finish once, for example, a property has been violated.
/// Empty outside Seedbox.
pub fn summary() -> io::Result<Summary> {
    CLIENT.summary()
}

/// A recovery obligation, judged as an `always` property: `probe` must
/// succeed within `deadline` of guest time from now, polled every 20ms.
/// Start it once the system should be able to recover, for example after
/// healing every fault; a host timeout ending the run first leaves the
/// property unevaluated, never passed.
pub fn recovers_within(id: &str, deadline: Duration, probe: impl FnMut() -> bool) -> bool {
    let (recovered, details) = poll_recovery(deadline, probe, Instant::now, std::thread::sleep);
    always(id, recovered, details);
    recovered
}

/// Poll until `probe` succeeds within `deadline` or the deadline passes. A
/// success counts only when the probe returned before the deadline, and no
/// sleep runs past it.
fn poll_recovery(
    deadline: Duration,
    mut probe: impl FnMut() -> bool,
    now: impl Fn() -> Instant,
    mut sleep: impl FnMut(Duration),
) -> (bool, Value) {
    let start = now();
    loop {
        let succeeded = probe();
        let waited = now().duration_since(start);
        if succeeded && waited <= deadline {
            return (true, json!({"waited_ms": waited.as_millis() as u64}));
        }
        if waited >= deadline {
            let deadline_ms = deadline.as_millis() as u64;
            return (
                false,
                json!({"deadline_ms": deadline_ms, "waited_ms": waited.as_millis() as u64}),
            );
        }
        sleep(Duration::from_millis(20).min(deadline - waited));
    }
}

#[cfg(test)]
mod tests;
