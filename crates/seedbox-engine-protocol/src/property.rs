//! Guest property records and the property results a run reports.
//!
//! The guest SDK (`seedbox-sdk`) connects a `SOCK_SEQPACKET` socket to
//! [`SOCKET`] on any machine; each connection is one session and each message
//! one JSON [`Record`] of at most [`MAX_RECORD_BYTES`]. The supervisor numbers
//! a boot's sessions from 1 and each session's messages from 1, and relays
//! every message uninterpreted as one management line,
//! `SBX_PROP1 <session> <seq> <lowercase hex>` ([`Frame`]). Session
//! [`SUPERVISOR_SESSION`] carries the supervisor's own records. The engine
//! commits frames in occurrence order and answers [`Record::Flush`] and
//! [`Record::Summary`] with a [`Reply`] on `SBX_SDK1 <session> <json>`,
//! which the supervisor writes back to that session's socket.
//!
//! Nothing reads these frames yet: the supervisor relay and the reduction
//! into an [`Assessment`] come with the engine change that follows this one.
//! Evidence errors (malformed records, broken sequencing, exceeded bounds,
//! conflicting declarations, records the SDK or supervisor lost) make an
//! assessment incomplete, so a run can never pass because evidence was lost.
//! The SDK never drops an evaluation for its size: it drops the details
//! instead and says so ([`Record::Assert`]'s `details_truncated`).
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

/// The SDK socket inside every machine. Outside Seedbox it is absent.
pub const SOCKET: &str = "/run/seedbox/sdk.sock";
pub const PREFIX: &[u8] = b"SBX_PROP1 ";
pub const REPLY_PREFIX: &[u8] = b"SBX_SDK1 ";
/// Largest SDK message the supervisor relays, before hex encoding.
pub const MAX_RECORD_BYTES: usize = 4096;
/// Largest reply line, which stays inside the supervisor's 16 KiB frame;
/// the supervisor writes its JSON to the session's socket as one message.
pub const MAX_REPLY_BYTES: usize = 16 * 1024;
pub const MAX_ID_BYTES: usize = 256;
pub const MAX_MESSAGE_BYTES: usize = 1024;
/// Serialized details beyond this size are dropped and marked truncated.
pub const MAX_DETAILS_BYTES: usize = 1024;
pub const MAX_PROPERTIES: usize = 1024;
pub const MAX_EVENT_NAMES: usize = 1024;
/// Evidence errors kept with their origin; later ones are only counted.
pub const MAX_ERRORS: usize = 16;
/// Properties a [`Reply::Summary`] lists, by ID; more set `truncated`.
pub const MAX_SUMMARY_PROPERTIES: usize = 32;
/// The supervisor's own records use this session.
pub const SUPERVISOR_SESSION: u32 = 0;

/// Property IDs the SDK may not use; default properties live here.
pub const RESERVED_PREFIX: &str = "seedbox.";
/// Default `always_or_unreachable` property: the kernel kills no process for
/// running out of memory. Each [`crate::out_of_memory::Kill`] report, whether
/// or not the victim is the application, is one evaluation whose details are
/// the report: passing on a machine that declares
/// `expectedOutOfMemoryKills: true`, failing on any other.
pub const DEFAULT_NO_OOM: &str = "seedbox.no_out_of_memory_kills";
/// Default `always_or_unreachable` property: no process dies of a
/// core-dumping signal it did not handle. Each [`crate::crash::Report`], one
/// per crash, is one evaluation: passing when it matches one of its machine's
/// declared `expectedCrashes`, failing otherwise. An OOM kill is never a crash.
pub const DEFAULT_NO_CRASH: &str = "seedbox.no_crashing_processes";

/// One relayed SDK message with its supervisor attribution.
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

/// How a property is judged; see [`PropertyStatus::of`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    /// Every evaluation holds, and at least one happens.
    Always,
    /// Every evaluation holds; never evaluating it is fine.
    AlwaysOrUnreachable,
    /// At least one evaluation holds.
    Sometimes,
    /// Reached at least once; the condition is ignored.
    Reachable,
    /// Never reached; the condition is ignored.
    Unreachable,
}

/// A guest message, after parsing. Each serializes as a one-key object, such
/// as `{"assert":{"id":"p","kind":"always","condition":true}}`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum Record {
    /// Make a property known before (or without) evaluating it, so a property
    /// that is never evaluated is still reported.
    Declare {
        id: String,
        kind: Kind,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        message: Option<String>,
    },
    /// Evaluate a property. An undeclared property is declared by its first
    /// evaluation; a later declaration or evaluation with another kind is an
    /// evidence error.
    Assert {
        id: String,
        kind: Kind,
        condition: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        message: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        details: Option<Value>,
        /// The SDK dropped details that would not fit [`MAX_RECORD_BYTES`],
        /// so that the evaluation itself still arrives.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        details_truncated: bool,
    },
    /// A named workload event; reports count each name.
    Event {
        name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        details: Option<Value>,
    },
    /// The workload on this machine finished setting up.
    SetupComplete {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        details: Option<Value>,
    },
    /// Ask for a [`Reply::Flush`] once the engine has committed and reduced
    /// every earlier record of this session.
    Flush {},
    /// As `Flush`, answered with a [`Reply::Summary`].
    Summary {},
    /// Records that never arrived: from an SDK session, the ones that
    /// process failed to send (sent once it can again); from
    /// [`SUPERVISOR_SESSION`], the ones the supervisor failed to relay.
    /// Either is an evidence error.
    Lost { reason: String, count: u64 },
}

impl Record {
    /// Parse and bound one payload. Failures are evidence errors.
    pub fn parse(payload: &[u8]) -> Result<Self, String> {
        let record: Self =
            serde_json::from_slice(payload).map_err(|e| format!("malformed record: {e}"))?;
        let (id, message) = match &record {
            Record::Declare { id, message, .. } | Record::Assert { id, message, .. } => {
                (Some(id), message.as_ref())
            }
            Record::Event { name, .. } => (Some(name), None),
            _ => (None, None),
        };
        if let Some(id) = id {
            if id.is_empty() || id.len() > MAX_ID_BYTES || id.chars().any(char::is_control) {
                return Err("invalid property id".into());
            }
        }
        if message.is_some_and(|m| m.len() > MAX_MESSAGE_BYTES) {
            return Err("property message too long".into());
        }
        Ok(record)
    }

    /// [`SUPERVISOR_SESSION`] sends only [`Record::Lost`]; SDK sessions
    /// send anything.
    pub fn allowed_from(&self, session: u32) -> bool {
        session != SUPERVISOR_SESSION || matches!(self, Record::Lost { .. })
    }
}

/// The engine's answer to a [`Record::Flush`] or [`Record::Summary`]; `seq` is
/// that record's sequence number in its session.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum Reply {
    Flush {
        seq: u64,
    },
    /// The run's properties as reduced so far, built by [`Reply::summary`]:
    /// the first by ID, at most [`MAX_SUMMARY_PROPERTIES`] and only as many
    /// as fit a [`MAX_REPLY_BYTES`] line, with `truncated` set when any are
    /// left out.
    Summary {
        seq: u64,
        properties: BTreeMap<String, Counts>,
        truncated: bool,
    },
}

impl Reply {
    /// A summary of `properties` that fits one reply line for any session.
    /// IDs escape in JSON (`"` and `\` double), so the count that fits
    /// depends on them; the first IDs in order are kept.
    pub fn summary(seq: u64, properties: impl IntoIterator<Item = (String, Counts)>) -> Self {
        let sorted: BTreeMap<String, Counts> = properties.into_iter().collect();
        let mut kept = BTreeMap::new();
        let mut truncated = false;
        // The longest session prefix: u32::MAX and its separator.
        let fixed = REPLY_PREFIX.len() + u32::MAX.to_string().len() + 1;
        for (id, counts) in sorted {
            if kept.len() == MAX_SUMMARY_PROPERTIES {
                truncated = true;
                break;
            }
            kept.insert(id.clone(), counts);
            let candidate = Self::Summary {
                seq,
                properties: kept,
                // The longer spelling, so the final flag cannot overflow.
                truncated: false,
            };
            let size = fixed
                + serde_json::to_vec(&candidate)
                    .expect("replies serialize")
                    .len();
            let Self::Summary { properties, .. } = candidate else {
                unreachable!()
            };
            kept = properties;
            if size > MAX_REPLY_BYTES {
                kept.remove(&id);
                truncated = true;
                break;
            }
        }
        Self::Summary {
            seq,
            properties: kept,
            truncated,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Counts {
    pub passes: u64,
    pub failures: u64,
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

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PropertyStatus {
    Passed,
    /// A safety property failed: `always` or `always_or_unreachable` false,
    /// or `unreachable` reached.
    Violated,
    /// Never satisfied: an `always` never evaluated, or a `sometimes` or
    /// `reachable` never true. Not a safety violation.
    Unmet,
}

impl PropertyStatus {
    /// What a kind's passing and failing evaluation counts mean. Reachability
    /// counts every evaluation as a pass.
    pub fn of(kind: Kind, passes: u64, failures: u64) -> Self {
        let evaluated = passes + failures > 0;
        match kind {
            Kind::Always | Kind::AlwaysOrUnreachable if failures > 0 => Self::Violated,
            Kind::Always if !evaluated => Self::Unmet,
            Kind::Sometimes if passes == 0 => Self::Unmet,
            Kind::Reachable if !evaluated => Self::Unmet,
            Kind::Unreachable if evaluated => Self::Violated,
            _ => Self::Passed,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub struct EvidenceError {
    pub error: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<Origin>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub struct PropertyResult {
    pub id: String,
    pub kind: Kind,
    pub status: PropertyStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    pub passes: u64,
    pub failures: u64,
    /// The evaluation that decided the status: the first failure of a
    /// violated or unmet property, the first hit of a reached `unreachable`,
    /// otherwise the first pass.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub example: Option<Example>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AssessmentStatus {
    /// No property was violated and all evidence arrived. Unmet goals do not
    /// fail a single run.
    Passed,
    /// Some property was violated, even if evidence is also incomplete.
    Violated,
    /// No violation was seen, but some evidence was lost or invalid.
    Incomplete,
}

/// A run's property results, reported beside the oracle verdict.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub struct Assessment {
    pub status: AssessmentStatus,
    pub violated: u32,
    pub unmet: u32,
    pub evidence_complete: bool,
    pub error_count: u64,
    /// The first [`MAX_ERRORS`] evidence errors.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub errors: Vec<EvidenceError>,
    /// Sorted by ID.
    pub properties: Vec<PropertyResult>,
}

#[cfg(test)]
mod tests;
