//! Guest property findings as the public API reports them: the engine's
//! snake_case `seedbox_engine_protocol::property` evidence, converted to the
//! API's camelCase resource style. Only failures are reported: violations
//! with their first example, unmet requirements and lost evidence.
use seedbox_engine_protocol::property as protocol;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PropertyKind {
    Always,
    Never,
}

/// What a run's property findings make of it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PropertyAssessmentStatus {
    Passed,
    /// A property was violated or a requirement was never evaluated.
    Failed,
    /// Nothing failed, but some evidence was lost.
    Incomplete,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PropertyOrigin {
    pub machine: String,
    pub incarnation: u32,
    pub session: u32,
    pub seq: u64,
    pub time_ns: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PropertyExample {
    pub origin: PropertyOrigin,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub details_truncated: bool,
}

/// A violated property and its first violating evaluation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PropertyViolation {
    pub id: String,
    pub kind: PropertyKind,
    pub example: PropertyExample,
}

/// Why some property evidence did not count.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LostPropertyEvidence {
    pub reason: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<PropertyOrigin>,
}

/// A run's guest property findings, beside the oracle verdict.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PropertyAssessment {
    /// Sorted by ID.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub violations: Vec<PropertyViolation>,
    /// Required properties never evaluated, sorted.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unmet_requirements: Vec<String>,
    /// Records lost or refused; the first reasons are in `lostEvidence`.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub lost_count: u64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub lost_evidence: Vec<LostPropertyEvidence>,
}

fn is_zero(count: &u64) -> bool {
    *count == 0
}

impl PropertyAssessment {
    /// A violation or unmet requirement fails; lost evidence alone leaves
    /// the result incomplete.
    pub fn status(&self) -> PropertyAssessmentStatus {
        if !self.violations.is_empty() || !self.unmet_requirements.is_empty() {
            PropertyAssessmentStatus::Failed
        } else if self.lost_count > 0 {
            PropertyAssessmentStatus::Incomplete
        } else {
            PropertyAssessmentStatus::Passed
        }
    }
}

impl From<protocol::Kind> for PropertyKind {
    fn from(kind: protocol::Kind) -> Self {
        match kind {
            protocol::Kind::Always => Self::Always,
            protocol::Kind::Never => Self::Never,
        }
    }
}

impl From<protocol::AssessmentStatus> for PropertyAssessmentStatus {
    fn from(status: protocol::AssessmentStatus) -> Self {
        match status {
            protocol::AssessmentStatus::Passed => Self::Passed,
            protocol::AssessmentStatus::Failed => Self::Failed,
            protocol::AssessmentStatus::Incomplete => Self::Incomplete,
        }
    }
}

impl From<protocol::Origin> for PropertyOrigin {
    fn from(o: protocol::Origin) -> Self {
        Self {
            machine: o.machine,
            incarnation: o.incarnation,
            session: o.session,
            seq: o.seq,
            time_ns: o.time_ns,
        }
    }
}

impl From<protocol::Assessment> for PropertyAssessment {
    fn from(a: protocol::Assessment) -> Self {
        Self {
            violations: a
                .violations
                .into_iter()
                .map(|v| PropertyViolation {
                    id: v.id,
                    kind: v.kind.into(),
                    example: PropertyExample {
                        origin: v.example.origin.into(),
                        details: v.example.details,
                        details_truncated: v.example.details_truncated,
                    },
                })
                .collect(),
            unmet_requirements: a.unmet_requirements,
            lost_count: a.lost_count,
            lost_evidence: a
                .lost_evidence
                .into_iter()
                .map(|e| LostPropertyEvidence {
                    reason: e.reason,
                    origin: e.origin.map(Into::into),
                })
                .collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn engine_evidence_becomes_camel_case_api_findings() {
        let origin = protocol::Origin {
            machine: "n1".into(),
            incarnation: 1,
            session: 2,
            seq: 3,
            time_ns: 4,
        };
        let engine = protocol::Assessment {
            violations: vec![protocol::Violation {
                id: protocol::DEFAULT_NO_OOM.into(),
                kind: protocol::Kind::Never,
                example: protocol::Example {
                    origin: origin.clone(),
                    details: None,
                    details_truncated: true,
                },
            }],
            unmet_requirements: vec!["failover exercised".into()],
            lost_count: 1,
            lost_evidence: vec![protocol::LostEvidence {
                reason: "lost".into(),
                origin: Some(origin),
            }],
        };
        let status = engine.status();
        let api = PropertyAssessment::from(engine);
        assert_eq!(api.status(), status.into());
        assert_eq!(api.status(), PropertyAssessmentStatus::Failed);
        let origin =
            json!({"machine": "n1", "incarnation": 1, "session": 2, "seq": 3, "timeNs": 4});
        let wire = serde_json::to_value(&api).unwrap();
        assert_eq!(
            wire,
            json!({
                "violations": [{"id": "seedbox.no_out_of_memory_kills", "kind": "never",
                    "example": {"origin": origin, "detailsTruncated": true}}],
                "unmetRequirements": ["failover exercised"],
                "lostCount": 1,
                "lostEvidence": [{"reason": "lost", "origin": origin}],
            })
        );
        assert_eq!(
            serde_json::from_value::<PropertyAssessment>(wire).unwrap(),
            api
        );
        let lost = PropertyAssessment {
            lost_count: 2,
            ..Default::default()
        };
        assert_eq!(lost.status(), PropertyAssessmentStatus::Incomplete);
        assert_eq!(
            PropertyAssessment::default().status(),
            PropertyAssessmentStatus::Passed
        );
    }
}
