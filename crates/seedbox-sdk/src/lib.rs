//! Guest-side property SDK. Applications and oracles running under Seedbox
//! declare and evaluate properties; the engine reduces them into a property
//! assessment reported beside the oracle verdict, and a violated property
//! fails the run even when the oracle passes.
//!
//! Each call sends one record to the machine's supervisor over [`SOCKET`],
//! whose path the supervisor also exports as [`SOCKET_ENV`]. Outside Seedbox
//! the variable is unset, the socket absent and every call does nothing, so
//! instrumented code runs unchanged elsewhere. Under Seedbox a socket the
//! program cannot reach (a directory mounted over `/run`, say) is not taken for
//! absence: the SDK connects through the supervisor's abstract socket
//! instead, and if that fails too it reports its records as lost through
//! `/dev/kmsg`, which the supervisor relays, so the run's evidence is
//! incomplete rather than silently empty.
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
/// Set by the supervisor to the socket's path in every application it starts.
pub const SOCKET_ENV: &str = "SEEDBOX_SDK_SOCKET";
/// The supervisor's fallback listener in the abstract socket namespace.
const ABSTRACT_SOCKET: &str = "seedbox-sdk";
/// Where the SDK reports records it could not deliver at all.
const KMSG: &str = "/dev/kmsg";
/// The kernel log line the supervisor relays as lost records.
const KMSG_LOST: &str = "seedbox-sdk: lost ";
/// Largest record the supervisor relays.
pub const MAX_RECORD_BYTES: usize = 4096;
/// Longest property ID or event name.
const MAX_ID_BYTES: usize = 256;
/// Largest flush or summary reply.
const MAX_REPLY_BYTES: usize = 16 * 1024;
/// A choice offers at most this many alternatives.
pub const MAX_CHOICE_ALTERNATIVES: usize = 256;
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
    /// `None` is the machine's socket: [`SOCKET_ENV`]'s path, or [`SOCKET`].
    path: Option<PathBuf>,
    /// For a client of its own path: where it falls back, as under Seedbox.
    fallback: Option<Fallback>,
    inner: Mutex<Inner>,
    choice_timeout: Duration,
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
    state: State,
    /// Records this client failed to send, reported as one `lost` record
    /// before anything else once a send succeeds again.
    lost: u64,
    lost_reason: String,
    reply_timeout: Duration,
    /// The loss was already reported through the kernel log.
    logged: bool,
}

/// How long [`Client::choose`] waits for the engine's answer, in the guest's
/// own (simulated) time.
pub const CHOICE_TIMEOUT: Duration = Duration::from_secs(10);
/// The event recorded when a choice went unanswered.
pub const CHOICE_UNANSWERED_EVENT: &str = "seedbox.choice_unanswered";

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
            fallback: None,
            inner: Mutex::new(Inner::new()),
            choice_timeout: CHOICE_TIMEOUT,
        }
    }

    pub fn new(path: impl AsRef<Path>) -> Self {
        Self {
            path: Some(path.as_ref().to_owned()),
            fallback: None,
            inner: Mutex::new(Inner::new()),
            choice_timeout: CHOICE_TIMEOUT,
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
            choice_timeout: CHOICE_TIMEOUT,
        }
    }

    /// Wait at most `timeout` for each choice's answer.
    pub fn with_choice_timeout(mut self, timeout: Duration) -> Self {
        self.choice_timeout = timeout;
        self
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
        self.request_within(request, key, None)
    }

    /// As [`Client::request`], waiting `timeout` rather than the session's
    /// [`REPLY_TIMEOUT`]. A missing reply closes the session either way.
    fn request_within(
        &self,
        request: &[u8],
        key: &str,
        timeout: Option<Duration>,
    ) -> io::Result<Option<Value>> {
        let target = self.target();
        let mut inner = self.lock();
        let Some(seq) = inner.send(&target, request)? else {
            return Ok(None);
        };
        let reply_timeout = inner.reply_timeout;
        let State::Connected { socket, .. } = &inner.state else {
            unreachable!("a sent request has a session");
        };
        let mut reply = vec![0u8; MAX_REPLY_BYTES];
        let received = match timeout {
            Some(timeout) => socket.set_read_timeout(Some(timeout)).and_then(|()| {
                let received = socket.recv(&mut reply);
                socket.set_read_timeout(Some(reply_timeout))?;
                received
            }),
            None => socket.recv(&mut reply),
        };
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

    /// Ask the engine to choose one of `alternatives` at `site`, and wait
    /// for its answer: an index into `alternatives`. The engine draws it from
    /// the run's seed, or a choice input selects it, so a run's choices
    /// replay exactly and a branch can vary them. Outside Seedbox the first
    /// alternative is chosen; so it is under Seedbox until the engine
    /// answers choices.
    ///
    /// With no answer within the choice timeout ([`CHOICE_TIMEOUT`] by
    /// default) this returns [`io::ErrorKind::TimedOut`]. The client then
    /// closes its session, as for any missing reply, so a late answer can
    /// never be read as another request's, and records a
    /// [`CHOICE_UNANSWERED_EVENT`] event naming the site on a new one.
    /// [`choose`] takes the first alternative then.
    pub fn choose(&self, site: &str, alternatives: &[&str]) -> io::Result<usize> {
        if alternatives.is_empty() || alternatives.len() > MAX_CHOICE_ALTERNATIVES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "a choice offers 1 to 256 alternatives",
            ));
        }
        let record =
            serde_json::to_vec(&choice_record(site, alternatives)).map_err(io::Error::other)?;
        if record.len() > MAX_RECORD_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "choice record exceeds 4096 bytes",
            ));
        }
        let body = match self.request_within(&record, "choice", Some(self.choice_timeout)) {
            Ok(Some(body)) => body,
            Ok(None) => return Ok(0),
            Err(error) => {
                if error.kind() == io::ErrorKind::TimedOut {
                    self.emit(event_record(
                        CHOICE_UNANSWERED_EVENT,
                        json!({"site": site, "timeout_ms": self.choice_timeout.as_millis() as u64}),
                    ));
                }
                return Err(error);
            }
        };
        body["index"]
            .as_u64()
            .and_then(|index| usize::try_from(index).ok())
            .filter(|index| body["site"] == site && *index < alternatives.len())
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "unexpected choice reply"))
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

    /// Report that a named piece of workload state took a value, such as a
    /// role or a recovery phase. Keep values to a few short labels (at most
    /// 64 bytes): a campaign treats each new value and each new transition
    /// as progress. A name may not contain `/`, `=`, `:` or `>`, nor a value
    /// `>`; such a report is an evidence error.
    pub fn state(&self, name: &str, value: &str) {
        self.emit(state_record(name, value));
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
            logged: false,
        }
    }

    /// Send one message on the session, opening it first and reporting
    /// earlier losses before it. Returns its sequence number, or `None`
    /// outside Seedbox. A failure counts the message as lost and closes the
    /// session.
    fn send(&mut self, target: &Target, bytes: &[u8]) -> io::Result<Option<u64>> {
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

fn state_record(name: &str, value: &str) -> Value {
    json!({"state": {"name": name, "value": value}})
}

fn choice_record(site: &str, alternatives: &[&str]) -> Value {
    json!({"choice": {"site": site, "alternatives": alternatives}})
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

/// Report `count` undeliverable records on the kernel log, as
/// `seedbox-sdk: lost COUNT property records: REASON`. Returns whether the
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
            "SDK socket path too long",
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
/// Report a named piece of workload state's new value; see [`Client::state`].
pub fn state(name: &str, value: &str) {
    CLIENT.state(name, value);
}
/// Mark the workload on this machine as set up.
pub fn setup_complete(details: Value) {
    CLIENT.setup_complete(details);
}
/// Let the engine choose one of `alternatives` at `site`; see
/// [`Client::choose`]. If the request cannot be sent or is not answered in
/// time, the first alternative is chosen and the failure is reported once on
/// stderr; an unanswered choice is also recorded as an event.
pub fn choose(site: &str, alternatives: &[&str]) -> usize {
    CLIENT.choose(site, alternatives).unwrap_or_else(|error| {
        static REPORTED: std::sync::Once = std::sync::Once::new();
        REPORTED.call_once(|| eprintln!("seedbox-sdk: choice not answered: {error}"));
        0
    })
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
