//! Guest property results as the public API reports them: the engine's
//! snake_case `seedbox_engine_protocol::property` evidence, converted to the
//! API's camelCase resource style.
use seedbox_engine_protocol::property as protocol;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PropertyKind {
    Always,
    AlwaysOrUnreachable,
    Sometimes,
    Reachable,
    Unreachable,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PropertyStatus {
    Passed,
    Violated,
    Unmet,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PropertyAssessmentStatus {
    Passed,
    Violated,
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

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PropertyEvidenceError {
    pub error: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<PropertyOrigin>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PropertyResult {
    pub id: String,
    pub kind: PropertyKind,
    pub status: PropertyStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    pub passes: u64,
    pub failures: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub example: Option<PropertyExample>,
}

/// A run's guest property results, beside the oracle verdict.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PropertyAssessment {
    pub status: PropertyAssessmentStatus,
    pub violated: u32,
    pub unmet: u32,
    pub evidence_complete: bool,
    pub error_count: u64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub errors: Vec<PropertyEvidenceError>,
    pub properties: Vec<PropertyResult>,
}

impl From<protocol::Kind> for PropertyKind {
    fn from(kind: protocol::Kind) -> Self {
        match kind {
            protocol::Kind::Always => Self::Always,
            protocol::Kind::AlwaysOrUnreachable => Self::AlwaysOrUnreachable,
            protocol::Kind::Sometimes => Self::Sometimes,
            protocol::Kind::Reachable => Self::Reachable,
            protocol::Kind::Unreachable => Self::Unreachable,
        }
    }
}

impl From<protocol::PropertyStatus> for PropertyStatus {
    fn from(status: protocol::PropertyStatus) -> Self {
        match status {
            protocol::PropertyStatus::Passed => Self::Passed,
            protocol::PropertyStatus::Violated => Self::Violated,
            protocol::PropertyStatus::Unmet => Self::Unmet,
        }
    }
}

impl From<protocol::AssessmentStatus> for PropertyAssessmentStatus {
    fn from(status: protocol::AssessmentStatus) -> Self {
        match status {
            protocol::AssessmentStatus::Passed => Self::Passed,
            protocol::AssessmentStatus::Violated => Self::Violated,
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
            status: a.status.into(),
            violated: a.violated,
            unmet: a.unmet,
            evidence_complete: a.evidence_complete,
            error_count: a.error_count,
            errors: a
                .errors
                .into_iter()
                .map(|e| PropertyEvidenceError {
                    error: e.error,
                    origin: e.origin.map(Into::into),
                })
                .collect(),
            properties: a
                .properties
                .into_iter()
                .map(|p| PropertyResult {
                    id: p.id,
                    kind: p.kind.into(),
                    status: p.status.into(),
                    message: p.message,
                    passes: p.passes,
                    failures: p.failures,
                    example: p.example.map(|e| PropertyExample {
                        origin: e.origin.into(),
                        details: e.details,
                        details_truncated: e.details_truncated,
                    }),
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
    fn engine_evidence_becomes_camel_case_api_results() {
        let origin = protocol::Origin {
            machine: "n1".into(),
            incarnation: 1,
            session: 2,
            seq: 3,
            time_ns: 4,
        };
        let engine = protocol::Assessment {
            status: protocol::AssessmentStatus::Violated,
            violated: 1,
            unmet: 0,
            evidence_complete: false,
            error_count: 1,
            errors: vec![protocol::EvidenceError {
                error: "lost".into(),
                origin: Some(origin.clone()),
            }],
            properties: vec![protocol::PropertyResult {
                id: protocol::DEFAULT_NO_OOM.into(),
                kind: protocol::Kind::AlwaysOrUnreachable,
                status: protocol::PropertyStatus::Violated,
                message: None,
                passes: 0,
                failures: 1,
                example: Some(protocol::Example {
                    origin,
                    details: None,
                    details_truncated: true,
                }),
            }],
        };
        let api = PropertyAssessment::from(engine);
        let origin =
            json!({"machine": "n1", "incarnation": 1, "session": 2, "seq": 3, "timeNs": 4});
        let wire = serde_json::to_value(&api).unwrap();
        assert_eq!(
            wire,
            json!({
                "status": "violated", "violated": 1, "unmet": 0, "evidenceComplete": false,
                "errorCount": 1, "errors": [{"error": "lost", "origin": origin}],
                "properties": [{"id": "seedbox.no_out_of_memory_kills",
                    "kind": "alwaysOrUnreachable", "status": "violated", "passes": 0,
                    "failures": 1, "example": {"origin": origin, "detailsTruncated": true}}]
            })
        );
        assert_eq!(
            serde_json::from_value::<PropertyAssessment>(wire).unwrap(),
            api
        );
    }
}
