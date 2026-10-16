//! Property records carried by the guest supervisor's management transport,
//! and the pure reducer that turns them into an assessment.
//!
//! The supervisor relays each SDK message as `SBX_PROP1 <session> <seq> <hex>`
//! without interpreting it. Session 0 belongs to the supervisor itself. The
//! engine reduces records in commit order; offline reporting runs the same
//! reducer over the same records. Evidence errors (malformed records, broken
//! sequencing, exceeded bounds, conflicting declarations) make an assessment
//! incomplete: a run can never pass because evidence was lost.
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

pub const PREFIX: &[u8] = b"SBX_PROP1 ";
/// Largest SDK message the supervisor relays, before hex encoding.
pub const MAX_RECORD_BYTES: usize = 4096;
pub const MAX_ID_BYTES: usize = 256;
pub const MAX_MESSAGE_BYTES: usize = 1024;
/// Serialized details beyond this size are dropped and marked truncated.
pub const MAX_DETAILS_BYTES: usize = 1024;
pub const MAX_PROPERTIES: usize = 1024;
pub const MAX_EVENT_NAMES: usize = 1024;
pub const MAX_ERRORS: usize = 16;
/// The supervisor's own records use this session.
pub const SUPERVISOR_SESSION: u32 = 0;

/// One relayed SDK message with its supervisor attribution.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Frame {
    pub session: u32,
    pub seq: u64,
    pub payload: Vec<u8>,
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
    let hex = fields.next().ok_or("missing property payload")?;
    let session = u32::try_from(session).map_err(|_| "invalid property session")?;
    if hex.is_empty() || hex.len() > MAX_RECORD_BYTES * 2 || hex.len() % 2 != 0 {
        return Err("invalid property payload length");
    }
    fn digit(byte: u8) -> Result<u8, &'static str> {
        match byte {
            b'0'..=b'9' => Ok(byte - b'0'),
            b'a'..=b'f' => Ok(byte - b'a' + 10),
            _ => Err("invalid property payload encoding"),
        }
    }
    let payload = hex
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| Ok(digit(pair[0])? * 16 + digit(pair[1])?))
        .collect::<Result<Vec<_>, &'static str>>()?;
    Ok(Some(Frame {
        session,
        seq,
        payload,
    }))
}

fn number(field: Option<&[u8]>) -> Result<u64, &'static str> {
    let field = field.ok_or("missing property frame field")?;
    // Canonical decimal only: one encoding per value.
    if field.is_empty() || (field.len() > 1 && field[0] == b'0') {
        return Err("invalid property frame number");
    }
    std::str::from_utf8(field)
        .ok()
        .and_then(|s| s.parse().ok())
        .ok_or("invalid property frame number")
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
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

/// A guest SDK message, after parsing.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum Record {
    /// Make a property known before (or without) evaluating it.
    Declare {
        id: String,
        kind: Kind,
        #[serde(default)]
        message: Option<String>,
    },
    /// Evaluate a property. An undeclared property is declared by its first
    /// evaluation.
    Assert {
        id: String,
        kind: Kind,
        condition: bool,
        #[serde(default)]
        message: Option<String>,
        #[serde(default)]
        details: Option<Value>,
    },
    /// A named workload event.
    Event {
        name: String,
        #[serde(default)]
        details: Option<Value>,
    },
    /// The workload on this machine finished setting up.
    SetupComplete {
        #[serde(default)]
        details: Option<Value>,
    },
    /// Supervisor only: how the main process ended.
    Exit {
        #[serde(default)]
        code: Option<i32>,
        #[serde(default)]
        signal: Option<i32>,
    },
    /// Supervisor only: SDK messages it could not relay.
    Lost { reason: String, count: u64 },
}

impl Record {
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
    fn supervisor_only(&self) -> bool {
        matches!(self, Record::Exit { .. } | Record::Lost { .. })
    }
}

/// Where and when a record was committed. The engine supplies the time at
/// complete-frame acceptance; guest-supplied times are never trusted.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Origin {
    pub machine: String,
    pub incarnation: u32,
    pub session: u32,
    pub seq: u64,
    pub time_ns: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Example {
    pub origin: Origin,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub details_truncated: bool,
}

impl Example {
    fn new(origin: &Origin, details: Option<Value>) -> Self {
        let size = details
            .as_ref()
            .map_or(0, |d| serde_json::to_vec(d).map_or(usize::MAX, |b| b.len()));
        let truncated = size > MAX_DETAILS_BYTES;
        Self {
            origin: origin.clone(),
            details: if truncated { None } else { details },
            details_truncated: truncated,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Property {
    pub kind: Kind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// Declared explicitly, rather than only by evaluation.
    pub declared: bool,
    pub passes: u64,
    pub failures: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_pass: Option<Example>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_failure: Option<Example>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PropertyStatus {
    Passed,
    /// A safety property failed: `always` false, or `unreachable` reached.
    Violated,
    /// Never satisfied: a declared `always` never evaluated, or a `sometimes`
    /// or `reachable` never true. Not a safety violation.
    Unmet,
}

impl Property {
    pub fn status(&self) -> PropertyStatus {
        let evaluated = self.passes + self.failures > 0;
        match self.kind {
            Kind::Always if self.failures > 0 => PropertyStatus::Violated,
            Kind::Always if !evaluated => PropertyStatus::Unmet,
            Kind::AlwaysOrUnreachable if self.failures > 0 => PropertyStatus::Violated,
            Kind::Sometimes if self.passes == 0 => PropertyStatus::Unmet,
            Kind::Reachable if !evaluated => PropertyStatus::Unmet,
            Kind::Unreachable if evaluated => PropertyStatus::Violated,
            _ => PropertyStatus::Passed,
        }
    }
    /// The example that decided the status, if any.
    pub fn example(&self) -> Option<&Example> {
        match (self.kind, self.status()) {
            (Kind::Unreachable, PropertyStatus::Violated) => {
                self.first_pass.as_ref().or(self.first_failure.as_ref())
            }
            (_, PropertyStatus::Violated) => self.first_failure.as_ref(),
            // An unmet goal that was evaluated shows where it was false.
            (_, PropertyStatus::Unmet) => self.first_failure.as_ref(),
            _ => self.first_pass.as_ref(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EvidenceError {
    pub error: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<Origin>,
}

/// How each machine's main process ended, as its supervisor observed it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ExitRecord {
    pub origin: Origin,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signal: Option<i32>,
}

/// Property IDs the SDK may not use; default properties live here.
pub const RESERVED_PREFIX: &str = "seedbox.";
/// Default property: no main process dies from a signal it did not handle.
pub const DEFAULT_FATAL_SIGNAL: &str = "seedbox.main_process_not_killed_by_signal";

/// Reduced property evidence. Checkpoints retain it whole; every field is
/// ordered, so serialization is canonical.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Ledger {
    pub properties: BTreeMap<String, Property>,
    pub events: BTreeMap<String, u64>,
    /// First setup completion per machine.
    pub setup: BTreeMap<String, Origin>,
    pub exits: Vec<ExitRecord>,
    /// Records accepted, including ones that only produced evidence errors.
    pub records: u64,
    /// Count of evidence errors; only the first [`MAX_ERRORS`] are kept.
    pub error_count: u64,
    pub errors: Vec<EvidenceError>,
    /// Next expected sequence per machine, incarnation and session. Stored
    /// as a list: reports flatten this type, and flattened JSON maps cannot
    /// carry integer keys.
    #[serde(with = "sequence_list")]
    sequences: BTreeMap<(String, u32, u32), u64>,
}

mod sequence_list {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    use std::collections::BTreeMap;
    type Map = BTreeMap<(String, u32, u32), u64>;
    pub fn serialize<S: Serializer>(map: &Map, serializer: S) -> Result<S::Ok, S::Error> {
        let list: Vec<_> = map
            .iter()
            .map(|((machine, incarnation, session), next)| (machine, incarnation, session, next))
            .collect();
        list.serialize(serializer)
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Map, D::Error> {
        let list = Vec::<(String, u32, u32, u64)>::deserialize(deserializer)?;
        let count = list.len();
        let map: Map = list
            .into_iter()
            .map(|(machine, incarnation, session, next)| ((machine, incarnation, session), next))
            .collect();
        if map.len() != count {
            return Err(serde::de::Error::custom("duplicate property sequence"));
        }
        Ok(map)
    }
}

impl Ledger {
    fn error(&mut self, error: impl Into<String>, origin: Option<&Origin>) {
        self.error_count += 1;
        if self.errors.len() < MAX_ERRORS {
            self.errors.push(EvidenceError {
                error: error.into(),
                origin: origin.cloned(),
            });
        }
    }

    /// Report a frame that could not be decoded at all. Its machine is known;
    /// nothing else about it is trusted.
    pub fn malformed_frame(&mut self, machine: &str, incarnation: u32, time_ns: u64, error: &str) {
        self.records += 1;
        let origin = Origin {
            machine: machine.into(),
            incarnation,
            session: 0,
            seq: 0,
            time_ns,
        };
        self.error(format!("malformed property frame: {error}"), Some(&origin));
    }

    /// Reduce one committed frame. Never fails: problems become evidence
    /// errors, which make the assessment incomplete.
    pub fn apply(&mut self, frame: &Frame, machine: &str, incarnation: u32, time_ns: u64) {
        self.records += 1;
        let origin = Origin {
            machine: machine.into(),
            incarnation,
            session: frame.session,
            seq: frame.seq,
            time_ns,
        };
        let next = self
            .sequences
            .entry((machine.into(), incarnation, frame.session))
            .or_insert(1);
        if frame.seq != *next {
            let expected = *next;
            *next = frame.seq.saturating_add(1);
            self.error(
                format!("property sequence {} after {}", frame.seq, expected - 1),
                Some(&origin),
            );
        } else {
            *next += 1;
        }
        let record = match Record::parse(&frame.payload) {
            Ok(record) => record,
            Err(error) => return self.error(error, Some(&origin)),
        };
        if record.supervisor_only() != (frame.session == SUPERVISOR_SESSION) {
            return self.error("record not allowed from this session", Some(&origin));
        }
        if let Record::Declare { id, .. } | Record::Assert { id, .. } = &record {
            if id.starts_with(RESERVED_PREFIX) {
                return self.error(format!("property id {id:?} is reserved"), Some(&origin));
            }
        }
        match record {
            Record::Declare { id, kind, message } => {
                self.declare(id, kind, message, true, &origin);
            }
            Record::Assert {
                id,
                kind,
                condition,
                message,
                details,
            } => {
                if self.declare(id.clone(), kind, message, false, &origin) {
                    self.evaluate(&id, condition, details, &origin);
                }
            }
            Record::Event { name, details: _ } => {
                if let Some(count) = self.events.get_mut(&name) {
                    *count += 1;
                } else if self.events.len() < MAX_EVENT_NAMES {
                    self.events.insert(name, 1);
                } else {
                    self.error("too many event names", Some(&origin));
                }
            }
            Record::SetupComplete { details: _ } => {
                self.setup.entry(machine.into()).or_insert(origin);
            }
            Record::Exit { code, signal } => {
                if code.is_some() == signal.is_some() {
                    return self.error("exit needs exactly one of code or signal", Some(&origin));
                }
                let kind = Kind::AlwaysOrUnreachable;
                if self.declare(DEFAULT_FATAL_SIGNAL.into(), kind, None, true, &origin) {
                    let details = signal.map(|s| serde_json::json!({"signal": s}));
                    self.evaluate(DEFAULT_FATAL_SIGNAL, signal.is_none(), details, &origin);
                }
                self.exits.push(ExitRecord {
                    origin,
                    code,
                    signal,
                });
            }
            Record::Lost { reason, count } => {
                let lost = match count {
                    1 => "property record lost".to_string(),
                    count => format!("{count} property records lost"),
                };
                self.error(format!("{lost}: {reason}"), Some(&origin));
            }
        }
    }

    /// Returns whether the property exists afterwards with this kind.
    fn declare(
        &mut self,
        id: String,
        kind: Kind,
        message: Option<String>,
        explicit: bool,
        origin: &Origin,
    ) -> bool {
        if let Some(property) = self.properties.get_mut(&id) {
            if property.kind != kind {
                self.error(
                    format!("property {id:?} redeclared with another kind"),
                    Some(origin),
                );
                return false;
            }
            property.declared |= explicit;
            if property.message.is_none() {
                property.message = message;
            }
            return true;
        }
        if self.properties.len() >= MAX_PROPERTIES {
            self.error("too many properties", Some(origin));
            return false;
        }
        self.properties.insert(
            id,
            Property {
                kind,
                message,
                declared: explicit,
                passes: 0,
                failures: 0,
                first_pass: None,
                first_failure: None,
            },
        );
        true
    }

    fn evaluate(&mut self, id: &str, condition: bool, details: Option<Value>, origin: &Origin) {
        let property = self.properties.get_mut(id).expect("declared");
        // Reachability is about being evaluated; the condition is ignored.
        let holds = condition || matches!(property.kind, Kind::Reachable | Kind::Unreachable);
        let (count, first) = if holds {
            (&mut property.passes, &mut property.first_pass)
        } else {
            (&mut property.failures, &mut property.first_failure)
        };
        *count = count.saturating_add(1);
        if first.is_none() {
            *first = Some(Example::new(origin, details));
        }
    }

    pub fn assessment(&self) -> Assessment {
        let mut properties = Vec::with_capacity(self.properties.len());
        let (mut violated, mut unmet) = (0, 0);
        for (id, property) in &self.properties {
            let status = property.status();
            violated += (status == PropertyStatus::Violated) as u32;
            unmet += (status == PropertyStatus::Unmet) as u32;
            properties.push(PropertyResult {
                id: id.clone(),
                kind: property.kind,
                status,
                message: property.message.clone(),
                passes: property.passes,
                failures: property.failures,
                example: property.example().cloned(),
            });
        }
        // A definite violation stays visible even when evidence is incomplete.
        let status = if violated > 0 {
            AssessmentStatus::Violated
        } else if self.error_count > 0 {
            AssessmentStatus::Incomplete
        } else {
            AssessmentStatus::Passed
        };
        Assessment {
            status,
            violated,
            unmet,
            evidence_complete: self.error_count == 0,
            error_count: self.error_count,
            errors: self.errors.clone(),
            properties,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AssessmentStatus {
    /// No safety property failed and all evidence arrived. Unmet goals do not
    /// fail a single run.
    Passed,
    Violated,
    /// No violation was seen, but some evidence was lost or invalid.
    Incomplete,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PropertyResult {
    pub id: String,
    pub kind: Kind,
    pub status: PropertyStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    pub passes: u64,
    pub failures: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub example: Option<Example>,
}

/// A run's property results, reported beside the oracle verdict.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Assessment {
    pub status: AssessmentStatus,
    pub violated: u32,
    pub unmet: u32,
    pub evidence_complete: bool,
    pub error_count: u64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub errors: Vec<EvidenceError>,
    pub properties: Vec<PropertyResult>,
}

#[cfg(test)]
#[path = "property_tests.rs"]
mod tests;
