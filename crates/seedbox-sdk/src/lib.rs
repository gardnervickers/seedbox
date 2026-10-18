//! Guest-side property SDK. Applications and oracles running under Seedbox
//! declare and evaluate properties; the engine reduces them into a property
//! assessment reported beside the oracle verdict.
//!
//! Each call sends one record to the machine's supervisor over
//! `/run/seedbox/sdk.sock`. Outside Seedbox the socket is absent and every
//! call does nothing, so instrumented code runs unchanged elsewhere.
//!
//! Assertions are nonfatal. A successful send means the supervisor's socket
//! accepted the record. [`flush`] returns once the supervisor has relayed every
//! earlier record from this process's connection to the engine; use it before
//! an oracle exits so its final assertions are part of the result.
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
use std::time::Duration;

/// The supervisor's socket inside every Seedbox machine.
pub const SOCKET: &str = "/run/seedbox/sdk.sock";
/// Largest record the supervisor relays.
pub const MAX_RECORD_BYTES: usize = 4096;
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
    /// More properties exist than the reply lists.
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

/// A connection to one supervisor socket. The free functions use a shared
/// process-wide client for [`SOCKET`].
pub struct Client {
    /// `None` is [`SOCKET`].
    path: Option<PathBuf>,
    socket: Mutex<State>,
}

enum State {
    Unopened,
    Connected(UnixDatagram),
    /// Not running under Seedbox: calls do nothing.
    Absent,
}

impl Client {
    /// The machine's supervisor socket.
    pub const fn machine() -> Self {
        Self {
            path: None,
            socket: Mutex::new(State::Unopened),
        }
    }

    pub fn new(path: impl AsRef<Path>) -> Self {
        Self {
            path: Some(path.as_ref().to_owned()),
            socket: Mutex::new(State::Unopened),
        }
    }

    fn send(&self, record: &Value) -> io::Result<()> {
        let bytes = serde_json::to_vec(record).map_err(io::Error::other)?;
        if bytes.len() > MAX_RECORD_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "property record exceeds 4096 bytes",
            ));
        }
        self.with_socket(|socket| socket.send(&bytes).map(drop))
    }

    /// Wait until the engine has committed and reduced every earlier record
    /// of this client's session: its reply crosses the same commit boundary.
    pub fn flush(&self) -> io::Result<()> {
        self.with_socket(|socket| {
            socket.send(FLUSH)?;
            let mut reply = [0u8; 256];
            let size = socket.recv(&mut reply)?;
            let reply: Value = serde_json::from_slice(&reply[..size]).map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidData, "unexpected flush reply")
            })?;
            match reply["flush"]["seq"].as_u64() {
                Some(_) => Ok(()),
                None => Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "unexpected flush reply",
                )),
            }
        })
    }

    /// As [`Client::flush`], then the run's properties as the engine has
    /// reduced them at that point: each one's passes and failures (at most
    /// 32, by ID; `truncated` when there are more). Empty outside Seedbox.
    pub fn summary(&self) -> io::Result<Summary> {
        let mut summary = Summary::default();
        self.with_socket(|socket| {
            socket.send(SUMMARY)?;
            let mut reply = vec![0u8; 32 * 1024];
            let size = socket.recv(&mut reply)?;
            let invalid = || io::Error::new(io::ErrorKind::InvalidData, "unexpected summary reply");
            let reply: Value = serde_json::from_slice(&reply[..size]).map_err(|_| invalid())?;
            let body = &reply["summary"];
            body["seq"].as_u64().ok_or_else(invalid)?;
            for (id, counts) in body["properties"].as_object().ok_or_else(invalid)? {
                summary.properties.insert(
                    id.clone(),
                    PropertyCounts {
                        passes: counts["passes"].as_u64().ok_or_else(invalid)?,
                        failures: counts["failures"].as_u64().ok_or_else(invalid)?,
                    },
                );
            }
            summary.truncated = body["truncated"].as_bool().unwrap_or(false);
            Ok(())
        })?;
        Ok(summary)
    }

    /// Ask the engine to choose one of `alternatives` at `site`, and wait
    /// for its answer: an index into `alternatives`. The engine draws it from
    /// the run's seed, or a choice input selects it, so a run's choices
    /// replay exactly and a branch can vary them. Outside Seedbox the first
    /// alternative is chosen.
    pub fn choose(&self, site: &str, alternatives: &[&str]) -> io::Result<usize> {
        if alternatives.is_empty() || alternatives.len() > MAX_CHOICE_ALTERNATIVES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "a choice offers 1 to 256 alternatives",
            ));
        }
        let record = json!({"choice": {"site": site, "alternatives": alternatives}});
        let bytes = serde_json::to_vec(&record).map_err(io::Error::other)?;
        if bytes.len() > MAX_RECORD_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "choice record exceeds 4096 bytes",
            ));
        }
        let mut selected = 0;
        self.with_socket(|socket| {
            socket.send(&bytes)?;
            let mut reply = [0u8; 1024];
            let size = socket.recv(&mut reply)?;
            let invalid = || io::Error::new(io::ErrorKind::InvalidData, "unexpected choice reply");
            let reply: Value = serde_json::from_slice(&reply[..size]).map_err(|_| invalid())?;
            let choice = &reply["choice"];
            selected = choice["index"]
                .as_u64()
                .map(|index| index as usize)
                .filter(|index| choice["site"] == site && *index < alternatives.len())
                .ok_or_else(invalid)?;
            Ok(())
        })?;
        Ok(selected)
    }

    fn with_socket(&self, f: impl FnOnce(&UnixDatagram) -> io::Result<()>) -> io::Result<()> {
        let mut state = self.socket.lock().unwrap_or_else(|e| e.into_inner());
        if let State::Unopened = *state {
            let path = self.path.as_deref().unwrap_or(Path::new(SOCKET));
            *state = match connect(path) {
                Ok(socket) => State::Connected(socket),
                Err(e) if matches!(e.raw_os_error(), Some(libc::ENOENT | libc::ENOTDIR)) => {
                    State::Absent
                }
                Err(e) => return Err(e),
            };
        }
        match &*state {
            State::Connected(socket) => f(socket),
            _ => Ok(()),
        }
    }

    /// Declare a property so it is reported even if never evaluated.
    pub fn declare(&self, id: &str, kind: Kind) {
        self.emit(json!({"declare": {"id": id, "kind": kind.name()}}));
    }

    /// Evaluate a property of the given kind.
    pub fn assert(&self, id: &str, kind: Kind, condition: bool, details: Value) {
        let body = json!({"id": id, "kind": kind.name(), "condition": condition});
        self.emit(json!({ "assert": with_details(body, details) }));
    }

    /// Record a named workload event.
    pub fn event(&self, name: &str, details: Value) {
        self.emit(json!({ "event": with_details(json!({"name": name}), details) }));
    }

    /// Report that a named piece of workload state took a value, such as a
    /// role or a recovery phase. Keep values to a few short labels: the
    /// planner treats each new value and each new transition as progress.
    /// A name may not contain `/`, `=`, `:` or `>`, nor a value `>`; such a
    /// report is an evidence error.
    pub fn state(&self, name: &str, value: &str) {
        self.emit(json!({"state": {"name": name, "value": value}}));
    }

    /// Mark the workload on this machine as set up.
    pub fn setup_complete(&self, details: Value) {
        self.emit(json!({ "setup_complete": with_details(json!({}), details) }));
    }

    /// Assertions never fail the caller. A record that cannot be sent is
    /// reported once on stderr; the engine's missing evidence shows up as an
    /// unmet or absent property rather than a crash in the workload.
    fn emit(&self, record: Value) {
        if let Err(error) = self.send(&record) {
            static REPORTED: std::sync::Once = std::sync::Once::new();
            REPORTED.call_once(|| eprintln!("seedbox-sdk: property record not sent: {error}"));
        }
    }
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
/// Report a named piece of workload state's new value; see [`Client::state`].
pub fn state(name: &str, value: &str) {
    CLIENT.state(name, value);
}
/// Mark the workload on this machine as set up.
pub fn setup_complete(details: Value) {
    CLIENT.setup_complete(details);
}
/// Let the engine choose one of `alternatives` at `site`; see
/// [`Client::choose`]. Outside Seedbox, or if the reply path fails, the first
/// alternative is chosen and the failure is reported once on stderr.
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
/// [`control::quiet`]; a host timeout ending the run first leaves the
/// property unevaluated, never passed.
pub fn recovers_within(id: &str, deadline: Duration, probe: impl FnMut() -> bool) -> bool {
    let (recovered, details) = poll_recovery(deadline, probe, std::time::Instant::now);
    always(id, recovered, details);
    recovered
}

/// Poll until `probe` succeeds within `deadline` or the deadline passes. A
/// success counts only when the probe returned before the deadline, and no
/// sleep runs past it.
fn poll_recovery(
    deadline: Duration,
    mut probe: impl FnMut() -> bool,
    now: impl Fn() -> std::time::Instant,
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
        std::thread::sleep(Duration::from_millis(20).min(deadline - waited));
    }
}

pub mod control;
pub mod workload;

#[cfg(test)]
mod tests;
