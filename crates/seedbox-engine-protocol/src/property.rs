//! Guest property records and the property findings a run reports.
//!
//! The guest library (`seedbox-guest`) connects a `SOCK_SEQPACKET` socket to
//! [`SOCKET`] on any machine; each connection is one session and each message
//! one JSON [`Record`] of at most [`MAX_RECORD_BYTES`]. The supervisor numbers
//! a boot's sessions from 1 and each session's messages from 1, and relays
//! every message uninterpreted as one management line,
//! `SBX_PROP1 <session> <seq> <lowercase hex>` ([`Frame`]). Session
//! [`SUPERVISOR_SESSION`] carries the supervisor's own records. The engine
//! commits frames in occurrence order and answers [`Record::Flush`] with a
//! [`Reply`] on `SBX_GUEST1 <session> <json>`, which the supervisor writes
//! back to that session's socket.
//!
//! Properties are `always` or `never` conditions. Only failures are kept: a
//! run's [`Assessment`] lists each violated property with its first example,
//! each [`Record::Require`]d property that was never evaluated, and any lost
//! evidence (records the guest library or supervisor lost, or that were
//! malformed, out of sequence or beyond a bound), so a run can never pass
//! because evidence was lost. There is no per-property tally.
//!
//! Nothing reads these frames yet: the supervisor relay and the reduction
//! into an [`Assessment`] come with the engine change that follows this one.
//! The guest library never drops an evaluation for its size: it drops the
//! details instead and says so ([`Record::Assert`]'s `details_truncated`).
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The guest library's socket inside every machine. Outside Seedbox it is
/// absent.
pub const SOCKET: &str = "/run/seedbox/guest.sock";
/// The supervisor sets this to [`SOCKET`] in every application it starts,
/// so the guest library can tell an unreachable socket (lost evidence) from
/// running outside Seedbox.
pub const SOCKET_ENV: &str = "SEEDBOX_GUEST_SOCKET";
/// The supervisor's fallback listener in the Linux abstract socket namespace,
/// for programs that cannot reach [`SOCKET`]'s path (a mount over `/run`).
pub const ABSTRACT_SOCKET: &str = "seedbox-guest";
/// A program that reaches neither socket writes one kernel log line,
/// `seedbox-guest: lost COUNT property records: REASON`, which the supervisor
/// relays as a [`Record::Lost`] on [`SUPERVISOR_SESSION`].
pub const KMSG_LOST: &str = "seedbox-guest: lost ";
/// Concurrent guest sessions a supervisor serves; it refuses more as lost.
pub const MAX_SESSIONS: usize = 64;
pub const PREFIX: &[u8] = b"SBX_PROP1 ";
pub const REPLY_PREFIX: &[u8] = b"SBX_GUEST1 ";
/// Largest guest message the supervisor relays, before hex encoding.
pub const MAX_RECORD_BYTES: usize = 4096;
/// Largest reply line, which stays inside the supervisor's 16 KiB frame;
/// the supervisor writes its JSON to the session's socket as one message.
pub const MAX_REPLY_BYTES: usize = 16 * 1024;
pub const MAX_ID_BYTES: usize = 256;
/// Serialized details beyond this size are dropped and marked truncated.
pub const MAX_DETAILS_BYTES: usize = 1024;
/// Distinct property IDs a run tracks, required or evaluated; more are lost
/// evidence.
pub const MAX_PROPERTIES: usize = 1024;
/// Lost evidence kept with its reason and origin; more is only counted.
pub const MAX_LOST_EVIDENCE: usize = 16;
/// The supervisor's own records use this session.
pub const SUPERVISOR_SESSION: u32 = 0;

/// Property IDs guest programs may not use; default properties live here.
pub const RESERVED_PREFIX: &str = "seedbox.";
/// Default `never` property: the kernel kills a process for running out of
/// memory. Each [`crate::out_of_memory::Kill`] report on a machine without
/// `allowCrashes: true`, whether or not the victim is the application, is a
/// violation whose details are the report.
pub const DEFAULT_NO_OOM: &str = "seedbox.no_out_of_memory_kills";
/// Default `never` property: a process dies of a core-dumping signal it did
/// not handle. Each [`crate::crash::Report`], one per crash, on a machine
/// without `allowCrashes: true` is a violation. An OOM kill is never a crash.
pub const DEFAULT_NO_CRASH: &str = "seedbox.no_crashing_processes";

/// One relayed guest message with its supervisor attribution.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Frame {
    pub session: u32,
    pub seq: u64,
    pub payload: Vec<u8>,
}

impl Frame {
    /// The management line, without its newline.
    pub fn encode(&self) -> Vec<u8> {
        line(
            PREFIX,
            &format!("{} {} ", self.session, self.seq),
            &self.payload,
        )
    }
}

/// Decode one complete management line, excluding its newline. Other frame
/// kinds are ignored; malformed property frames are reported explicitly.
pub fn decode(line: &[u8]) -> Result<Option<Frame>, &'static str> {
    let Some(rest) = line.strip_prefix(PREFIX) else {
        return Ok(None);
    };
    let mut fields = rest.splitn(3, |b| *b == b' ');
    let session = number(fields.next())?;
    let seq = number(fields.next())?;
    let session = u32::try_from(session).map_err(|_| "invalid property session")?;
    let payload = hex(fields.next(), MAX_RECORD_BYTES)?;
    Ok(Some(Frame {
        session,
        seq,
        payload,
    }))
}

/// The reply line for one session, without its newline. Compact JSON has no
/// newline, so the reply travels unencoded.
pub fn encode_reply(session: u32, reply: &Reply) -> Vec<u8> {
    let mut line = REPLY_PREFIX.to_vec();
    line.extend_from_slice(format!("{session} ").as_bytes());
    serde_json::to_writer(&mut line, reply).expect("replies serialize");
    line
}

/// Decode one reply line, excluding its newline: its session and reply.
pub fn decode_reply(line: &[u8]) -> Result<Option<(u32, Reply)>, &'static str> {
    let Some(rest) = line.strip_prefix(REPLY_PREFIX) else {
        return Ok(None);
    };
    if line.len() > MAX_REPLY_BYTES {
        return Err("reply too long");
    }
    let mut fields = rest.splitn(2, |b| *b == b' ');
    let session = u32::try_from(number(fields.next())?).map_err(|_| "invalid reply session")?;
    let payload = fields.next().ok_or("missing reply payload")?;
    let reply = serde_json::from_slice(payload).map_err(|_| "invalid reply payload")?;
    Ok(Some((session, reply)))
}

fn line(prefix: &[u8], fields: &str, payload: &[u8]) -> Vec<u8> {
    let mut line = prefix.to_vec();
    line.extend_from_slice(fields.as_bytes());
    for byte in payload {
        line.extend_from_slice(format!("{byte:02x}").as_bytes());
    }
    line
}

fn number(field: Option<&[u8]>) -> Result<u64, &'static str> {
    let field = field.ok_or("missing property frame field")?;
    // Canonical decimal only: one encoding per value.
    if field.is_empty() || (field.len() > 1 && field[0] == b'0') {
        return Err("invalid property frame number");
    }
    std::str::from_utf8(field)
        .ok()
        .filter(|s| s.bytes().all(|b| b.is_ascii_digit()))
        .and_then(|s| s.parse().ok())
        .ok_or("invalid property frame number")
}

fn hex(field: Option<&[u8]>, max: usize) -> Result<Vec<u8>, &'static str> {
    let hex = field.ok_or("missing property payload")?;
    if hex.is_empty() || hex.len() > max * 2 || hex.len() % 2 != 0 {
        return Err("invalid property payload length");
    }
    fn digit(byte: u8) -> Result<u8, &'static str> {
        match byte {
            b'0'..=b'9' => Ok(byte - b'0'),
            b'a'..=b'f' => Ok(byte - b'a' + 10),
            _ => Err("invalid property payload encoding"),
        }
    }
    hex.as_chunks::<2>()
        .0
        .iter()
        .map(|pair| Ok(digit(pair[0])? * 16 + digit(pair[1])?))
        .collect()
}

/// How an evaluation is judged.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    /// Violated when the condition is false.
    Always,
    /// Violated when the condition is true.
    Never,
}

impl Kind {
    /// Whether an evaluation with this condition violates the property.
    pub fn violated_by(self, condition: bool) -> bool {
        match self {
            Self::Always => !condition,
            Self::Never => condition,
        }
    }
}

/// A guest message, after parsing. Each serializes as a one-key object, such
/// as `{"assert":{"id":"p","kind":"always","condition":true}}`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum Record {
    /// The property must be evaluated at least once during the run; one
    /// never evaluated by the end is an unmet requirement, which fails the
    /// run. Properties not required may go unevaluated.
    Require { id: String },
    /// Evaluate a property. Only a violating evaluation is kept, and only
    /// the first per property as its example.
    Assert {
        id: String,
        kind: Kind,
        condition: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        details: Option<Value>,
        /// The guest library dropped details that would not fit
        /// [`MAX_RECORD_BYTES`], so that the evaluation itself still arrives.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        details_truncated: bool,
    },
    /// Ask for a [`Reply::Flush`] once the engine has committed and reduced
    /// every earlier record of this session.
    Flush {},
    /// Records that never arrived: from a guest session, the ones that
    /// process failed to send (sent once it can again); from
    /// [`SUPERVISOR_SESSION`], the ones the supervisor failed to relay.
    /// Either is lost evidence.
    Lost { reason: String, count: u64 },
    /// From [`SUPERVISOR_SESSION`] only: guest `session` ended, so its
    /// sequence will not continue.
    Closed { session: u32 },
}

impl Record {
    /// Parse and bound one payload. Failures are lost evidence.
    pub fn parse(payload: &[u8]) -> Result<Self, String> {
        let record: Self =
            serde_json::from_slice(payload).map_err(|e| format!("malformed record: {e}"))?;
        if let Record::Require { id } | Record::Assert { id, .. } = &record {
            if id.is_empty() || id.len() > MAX_ID_BYTES || id.chars().any(char::is_control) {
                return Err("invalid property id".into());
            }
        }
        Ok(record)
    }

    /// [`SUPERVISOR_SESSION`] sends only [`Record::Lost`] and
    /// [`Record::Closed`]; guest sessions send anything else.
    pub fn allowed_from(&self, session: u32) -> bool {
        match self {
            Record::Lost { .. } => true,
            Record::Closed { .. } => session == SUPERVISOR_SESSION,
            _ => session != SUPERVISOR_SESSION,
        }
    }
}

/// The engine's answer to a [`Record::Flush`]; `seq` is that record's
/// sequence number in its session.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum Reply {
    Flush { seq: u64 },
}

/// Where and when a record was committed. The engine supplies the time at
/// frame acceptance; guest-supplied times are never trusted.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub struct Origin {
    pub machine: String,
    pub incarnation: u32,
    pub session: u32,
    pub seq: u64,
    pub time_ns: u64,
}

/// One evaluation kept as an example. Details larger than
/// [`MAX_DETAILS_BYTES`] serialized are dropped and marked truncated.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub struct Example {
    pub origin: Origin,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub details_truncated: bool,
}

/// A violated property and its first violating evaluation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub struct Violation {
    pub id: String,
    pub kind: Kind,
    pub example: Example,
}

/// Why some evidence did not count, and where, when a record carried it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub struct LostEvidence {
    pub reason: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<Origin>,
}

/// What a run's property findings make of it; see [`Assessment::status`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AssessmentStatus {
    Passed,
    /// A property was violated or a requirement was never evaluated.
    Failed,
    /// Nothing failed, but some evidence was lost.
    Incomplete,
}

/// A run's property findings: violations, unmet requirements and lost
/// evidence. Empty when nothing went wrong.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub struct Assessment {
    /// Sorted by ID.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub violations: Vec<Violation>,
    /// Required properties never evaluated, sorted.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unmet_requirements: Vec<String>,
    /// Evidence that did not count: the records reported lost, plus one for
    /// each record refused as malformed, out of sequence or beyond a bound.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub lost_count: u64,
    /// The first [`MAX_LOST_EVIDENCE`] reasons.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub lost_evidence: Vec<LostEvidence>,
}

fn is_zero(count: &u64) -> bool {
    *count == 0
}

impl Assessment {
    /// A violation or unmet requirement fails; lost evidence alone leaves
    /// the result incomplete. A definite violation stays a failure even
    /// when evidence was also lost.
    pub fn status(&self) -> AssessmentStatus {
        if !self.violations.is_empty() || !self.unmet_requirements.is_empty() {
            AssessmentStatus::Failed
        } else if self.lost_count > 0 {
            AssessmentStatus::Incomplete
        } else {
            AssessmentStatus::Passed
        }
    }

    pub fn is_empty(&self) -> bool {
        self.status() == AssessmentStatus::Passed
    }
}

#[cfg(test)]
mod tests;
