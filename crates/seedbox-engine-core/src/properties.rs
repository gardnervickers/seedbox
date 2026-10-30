//! Reduction of committed guest property evidence into a run's
//! [`Assessment`], and the verdict policy that applies it.
//!
//! The Driver feeds a [`Ledger`] every committed `SBX_PROP1` frame, `SBX_OOM1`
//! victim report and `SBX_CRASH1` fatal-signal report in commit order, with
//! the machine, boot and commit time; nothing here reads guest clocks. Only
//! failures are kept: each property's first violation, which properties were
//! required and whether each was evaluated, and lost evidence. The reduction
//! never fails: malformed records, broken sequencing, exceeded bounds and
//! lost records become lost evidence, which makes the run incomplete rather
//! than letting it pass.
//!
//! Records still in a guest when its machine crashes, or when the run ends,
//! are not evidence at all: a guest program that must know its records
//! counted calls `flush`, whose reply the engine sends only after reducing
//! them.
use seedbox_engine_protocol::{
    crash,
    out_of_memory::Kill,
    property::{
        Assessment, Example, Frame, Kind, LostEvidence, Origin, Record, Reply, Violation,
        DEFAULT_NO_CRASH, DEFAULT_NO_OOM, MAX_DETAILS_BYTES, MAX_LOST_EVIDENCE, MAX_PROPERTIES,
        MAX_SESSIONS, RESERVED_PREFIX, SUPERVISOR_SESSION,
    },
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};

/// What the run knows of one property ID: whether some guest required it
/// and whether any evaluated it. No tally is kept.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Seen {
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    required: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    evaluated: bool,
}

/// A property's first violation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct First {
    kind: Kind,
    example: Example,
}

/// Reduced property evidence for one run. Every field is ordered, so its
/// serialization (which Driver checkpoints retain) is canonical.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Ledger {
    /// Machines whose crashes and out-of-memory kills are allowed.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    allow_crashes: BTreeSet<String>,
    /// Guest property IDs required or evaluated so far.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    properties: BTreeMap<String, Seen>,
    /// Each violated property's first violation, default properties too.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    violations: BTreeMap<String, First>,
    #[serde(default, skip_serializing_if = "is_zero")]
    lost_count: u64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    lost_evidence: Vec<LostEvidence>,
    /// Each machine's live sessions, by machine name. Machine names are
    /// unique: projects name machines with DNS labels, and the Driver keys
    /// unnamed ones `machine:N`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    sequences: BTreeMap<String, Sessions>,
}

fn is_zero(count: &u64) -> bool {
    *count == 0
}

/// One machine's live guest sessions: only its latest incarnation, since an
/// earlier boot's sessions cannot send again, and only sessions the
/// supervisor has not reported closed.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Sessions {
    incarnation: u32,
    /// Session to the next expected sequence number.
    next: BTreeMap<u32, u64>,
}

/// Sessions tracked per machine: the supervisor's own and as many guest
/// sessions as it serves at once.
const MAX_TRACKED_SESSIONS: usize = MAX_SESSIONS + 1;

impl Ledger {
    /// A ledger in which crashes and out-of-memory kills on the named
    /// machines (those with `allowCrashes: true`) are not violations.
    pub fn new(allow_crashes: BTreeSet<String>) -> Self {
        Self {
            allow_crashes,
            ..Self::default()
        }
    }

    /// Check a ledger decoded from a checkpoint against the bounds reduction
    /// keeps, so a restored run reduces exactly as the original would.
    pub fn validate(&self) -> Result<(), &'static str> {
        let defaults = [DEFAULT_NO_CRASH, DEFAULT_NO_OOM];
        if self.properties.len() > MAX_PROPERTIES
            || self.lost_evidence.len() > MAX_LOST_EVIDENCE
            || (self.lost_evidence.len() as u64) > self.lost_count
            || self
                .sequences
                .values()
                .any(|s| s.next.len() > MAX_TRACKED_SESSIONS)
            || self.violations.keys().any(|id| {
                !defaults.contains(&id.as_str())
                    && !self.properties.get(id).is_some_and(|seen| seen.evaluated)
            })
        {
            return Err("property ledger bounds");
        }
        Ok(())
    }

    /// Count `count` pieces of lost evidence under one reason.
    fn lose(&mut self, count: u64, reason: impl Into<String>, origin: Option<&Origin>) {
        self.lost_count = self.lost_count.saturating_add(count.max(1));
        if self.lost_evidence.len() < MAX_LOST_EVIDENCE {
            self.lost_evidence.push(LostEvidence {
                reason: reason.into(),
                origin: origin.cloned(),
            });
        }
    }

    /// Check and advance a session's sequence. Records of a machine's new
    /// incarnation retire its earlier ones.
    fn sequence(&mut self, origin: &Origin) {
        let sessions = match self.sequences.get_mut(&origin.machine) {
            Some(sessions) if sessions.incarnation == origin.incarnation => sessions,
            _ => {
                let fresh = Sessions {
                    incarnation: origin.incarnation,
                    next: BTreeMap::new(),
                };
                let slot = self.sequences.entry(origin.machine.clone()).or_default();
                *slot = fresh;
                slot
            }
        };
        let full = sessions.next.len() >= MAX_TRACKED_SESSIONS
            && !sessions.next.contains_key(&origin.session);
        if full {
            // A supervisor serves at most MAX_SESSIONS at once and reports
            // each one that closes; forget the oldest rather than grow.
            let oldest = sessions
                .next
                .keys()
                .copied()
                .find(|s| *s != SUPERVISOR_SESSION);
            if let Some(oldest) = oldest {
                sessions.next.remove(&oldest);
            }
        }
        let next = sessions.next.entry(origin.session).or_insert(1);
        let expected = *next;
        *next = origin.seq.saturating_add(1);
        if full {
            self.lose(1, "too many open guest sessions", Some(origin));
        }
        if origin.seq != expected {
            self.lose(
                1,
                format!("property sequence {} where {expected} was next", origin.seq),
                Some(origin),
            );
        }
    }

    /// Reduce one committed guest frame. Returns the reply its flush record
    /// asks for, which follows every record reduced so far; the caller
    /// delivers it to the same session of the same boot.
    pub fn record(
        &mut self,
        machine: &str,
        incarnation: u32,
        time_ns: u64,
        frame: &Frame,
    ) -> Option<Reply> {
        let origin = Origin {
            machine: machine.into(),
            incarnation,
            session: frame.session,
            seq: frame.seq,
            time_ns,
        };
        self.sequence(&origin);
        let record = match Record::parse(&frame.payload) {
            Ok(record) => record,
            Err(error) => {
                self.lose(1, error, Some(&origin));
                return None;
            }
        };
        if !record.allowed_from(frame.session) {
            self.lose(
                1,
                "record not allowed from the supervisor session",
                Some(&origin),
            );
            return None;
        }
        if let Record::Require { id } | Record::Assert { id, .. } = &record {
            if id.starts_with(RESERVED_PREFIX) {
                self.lose(1, format!("property id {id:?} is reserved"), Some(&origin));
                return None;
            }
        }
        match record {
            Record::Require { id } => {
                if let Some(seen) = self.seen(id, &origin) {
                    seen.required = true;
                }
            }
            Record::Assert {
                id,
                kind,
                condition,
                details,
                details_truncated,
            } => {
                self.seen(id.clone(), &origin)?.evaluated = true;
                if kind.violated_by(condition) {
                    self.violate(id, kind, details, details_truncated, &origin);
                }
            }
            Record::Flush {} => return Some(Reply::Flush { seq: frame.seq }),
            Record::Closed { session } => {
                if let Some(sessions) = self.sequences.get_mut(machine) {
                    sessions.next.remove(&session);
                }
            }
            Record::Lost { reason, count } => {
                let lost = match count {
                    1 => "property record lost".to_string(),
                    count => format!("{count} property records lost"),
                };
                self.lose(count, format!("{lost}: {reason}"), Some(&origin));
            }
        }
        None
    }

    /// The property's entry, made on first sight within [`MAX_PROPERTIES`];
    /// beyond it the record is lost evidence.
    fn seen(&mut self, id: String, origin: &Origin) -> Option<&mut Seen> {
        if !self.properties.contains_key(&id) && self.properties.len() >= MAX_PROPERTIES {
            self.lose(1, "too many properties", Some(origin));
            return None;
        }
        Some(self.properties.entry(id).or_default())
    }

    /// Keep a property's first violation.
    fn violate(
        &mut self,
        id: String,
        kind: Kind,
        details: Option<Value>,
        truncated: bool,
        origin: &Origin,
    ) {
        self.violations.entry(id).or_insert_with(|| {
            let size = details
                .as_ref()
                .map_or(0, |d| serde_json::to_vec(d).map_or(usize::MAX, |b| b.len()));
            let dropped = size > MAX_DETAILS_BYTES;
            First {
                kind,
                example: Example {
                    origin: origin.clone(),
                    details: if dropped { None } else { details },
                    details_truncated: truncated || dropped,
                },
            }
        });
    }

    /// Evidence that is not a guest record: the supervisor's own session,
    /// without a sequence.
    fn kernel_origin(machine: &str, incarnation: u32, time_ns: u64) -> Origin {
        Origin {
            machine: machine.into(),
            incarnation,
            session: SUPERVISOR_SESSION,
            seq: 0,
            time_ns,
        }
    }

    /// A kernel OOM victim violates [`DEFAULT_NO_OOM`] unless its machine
    /// allows crashes.
    pub fn out_of_memory(&mut self, machine: &str, incarnation: u32, time_ns: u64, kill: &Kill) {
        if self.allow_crashes.contains(machine) {
            return;
        }
        let origin = Self::kernel_origin(machine, incarnation, time_ns);
        let details = json!({ "kill": kill });
        self.violate(
            DEFAULT_NO_OOM.into(),
            Kind::Never,
            Some(details),
            false,
            &origin,
        );
    }

    /// A fatal-signal report violates [`DEFAULT_NO_CRASH`] unless its
    /// machine allows crashes.
    pub fn crash(&mut self, machine: &str, incarnation: u32, time_ns: u64, report: &crash::Report) {
        if self.allow_crashes.contains(machine) {
            return;
        }
        let origin = Self::kernel_origin(machine, incarnation, time_ns);
        let details = json!({"message": report.message, "signal": report.signal});
        self.violate(
            DEFAULT_NO_CRASH.into(),
            Kind::Never,
            Some(details),
            false,
            &origin,
        );
    }

    /// The run's property findings, or `None` when there are none: no
    /// violation, no required property left unevaluated and no lost
    /// evidence. A requirement counts as unmet only once the run is over,
    /// so read this from the final report.
    pub fn assessment(&self) -> Option<Assessment> {
        let assessment = Assessment {
            violations: self
                .violations
                .iter()
                .map(|(id, first)| Violation {
                    id: id.clone(),
                    kind: first.kind,
                    example: first.example.clone(),
                })
                .collect(),
            unmet_requirements: self
                .properties
                .iter()
                .filter(|(_, seen)| seen.required && !seen.evaluated)
                .map(|(id, _)| id.clone())
                .collect(),
            lost_count: self.lost_count,
            lost_evidence: self.lost_evidence.clone(),
        };
        (!assessment.is_empty()).then_some(assessment)
    }
}

/// A run's test verdict, in the API's terms.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    Pending,
    Incomplete,
    Passed,
    Failed,
}

impl Verdict {
    /// An oracle's exit status: 0 passes, 3 could not decide (incomplete),
    /// anything else fails.
    pub fn of_exit(code: u8) -> Self {
        match code {
            0 => Self::Passed,
            3 => Self::Incomplete,
            _ => Self::Failed,
        }
    }
}

/// What a run's property findings contain, for [`verdict`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Findings {
    pub violated: bool,
    pub unmet: bool,
    pub lost: bool,
}

impl From<&Assessment> for Findings {
    fn from(assessment: &Assessment) -> Self {
        Self {
            violated: !assessment.violations.is_empty(),
            unmet: !assessment.unmet_requirements.is_empty(),
            lost: assessment.lost_count > 0,
        }
    }
}

/// The verdict policy: the oracle's verdict, qualified by the run's
/// property findings.
///
/// A violation fails a completed run whatever the oracle said, even one
/// whose oracle never exited: the violation is definite evidence, a missing
/// exit only missing evidence. A required property never evaluated fails a
/// run whose oracle exited; a run cut short before then stays as the oracle
/// left it, since the workload may not have reached it. Lost evidence makes
/// a passing oracle's run incomplete. Findings arrive with the final report,
/// so a passing oracle stays pending until completion rather than passing
/// first and failing later.
pub fn verdict(
    oracle: Verdict,
    oracle_exited: bool,
    completed: bool,
    findings: Option<Findings>,
) -> Verdict {
    if !completed {
        return if oracle == Verdict::Passed {
            Verdict::Pending
        } else {
            oracle
        };
    }
    let Some(findings) = findings else {
        return oracle;
    };
    if findings.violated || (oracle_exited && findings.unmet) {
        Verdict::Failed
    } else if oracle == Verdict::Passed && findings.lost {
        Verdict::Incomplete
    } else {
        oracle
    }
}

#[cfg(test)]
mod tests;
