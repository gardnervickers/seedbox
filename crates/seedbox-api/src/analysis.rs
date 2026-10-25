//! Narrowing a failed simulation: how often its failure recurs when its later
//! draws, or one of its faults, change.
//!
//! Each arm reruns the source's own program under derived seeds. A
//! `reseedFrom` arm adds an anchor and reseed at its time, so draws before it
//! stay the source's; a `without` arm drops one fault with its teardown, as
//! deriving a timeline that leaves those entries out does. Rates describe
//! these runs, never a proven cause. Nothing is shrunk: the program is only
//! ever narrowed by these comparisons. No server runs analyses yet: requests
//! are refused.
//!
//! `POST /v1/simulations/{id}/analyses` takes an [`AnalysisRequest`] and
//! returns its [`Analysis`]; `GET /v1/simulations/{id}/analyses/{request_id}`
//! reads it. Request IDs are scoped to the source simulation.
use crate::{ExecutionOptions, SimulationId};
use serde::{Deserialize, Serialize};

/// Runs per arm.
pub const MAX_ANALYSIS_SAMPLES: u32 = 64;
/// Time arms.
pub const MAX_ANALYSIS_TIMES: u32 = 32;
/// Runs one analysis may submit, every arm included.
pub const MAX_ANALYSIS_RUNS: u32 = 512;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AnalysisRequest {
    pub request_id: String,
    /// Applied to every run.
    #[serde(default)]
    pub options: ExecutionOptions,
    /// Runs per arm, each under its own derived seeds.
    pub samples: u32,
    /// Time arms, evenly spaced before the source ended.
    pub times: u32,
    /// Add an arm per external fault, run without it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub faults: bool,
    /// The property whose violation counts as the same failure; the oracle
    /// failure when absent and the source failed its oracle, otherwise its
    /// first violated property.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub property: Option<String>,
}

impl AnalysisRequest {
    /// Bounds that need no source. The fault arms' count is known only from
    /// the source's program, so the run total is checked again then.
    pub fn validate(&self) -> Result<(), String> {
        if !(1..=MAX_ANALYSIS_SAMPLES).contains(&self.samples) {
            return Err(format!(
                "analysis samples must be 1 to {MAX_ANALYSIS_SAMPLES}"
            ));
        }
        if self.times > MAX_ANALYSIS_TIMES {
            return Err(format!("analysis times are at most {MAX_ANALYSIS_TIMES}"));
        }
        if (1 + self.times) * self.samples > MAX_ANALYSIS_RUNS {
            return Err(format!(
                "an analysis runs at most {MAX_ANALYSIS_RUNS} simulations"
            ));
        }
        Ok(())
    }
}

/// What an arm changes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum AnalysisArmKind {
    /// Every draw varies; the program is the source's.
    Baseline,
    /// Draws before `atNs` stay the source's; every later draw varies.
    ReseedFrom { at_ns: u64 },
    /// The program without these inputs: one external fault, its teardown
    /// and its heals. Every draw varies.
    Without { keys: Vec<String>, summary: String },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AnalysisArm {
    pub kind: AnalysisArmKind,
    /// Its runs under derived seeds.
    pub runs: Vec<SimulationId>,
    /// Runs that completed with a verdict.
    pub completed: u32,
    /// Completed runs that failed the same way as the source.
    pub failed: u32,
    /// Completed runs that failed another way.
    pub other: u32,
    /// Runs without a verdict: incomplete evidence, or no completion.
    pub incomplete: u32,
    /// The 95% Wilson interval of `failed` of `completed`, per mille.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interval_per_mille: Option<[u32; 2]>,
    /// A `without` arm's run under the source's own seed: the source minus
    /// that fault and nothing else.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exact: Option<SimulationId>,
    /// Whether that run failed the same way, once it completed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exact_failed: Option<bool>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Analysis {
    pub source: SimulationId,
    pub request_id: String,
    /// What counts as the same failure: a property ID or `oracle`.
    pub target: String,
    /// The source's simulated end time.
    pub end_ns: u64,
    pub arms: Vec<AnalysisArm>,
    pub stop_requested: bool,
    pub error: Option<String>,
}

impl Analysis {
    /// Every run has a verdict or never will, or the analysis failed.
    pub fn complete(&self) -> bool {
        self.error.is_some()
            || self
                .arms
                .iter()
                .all(|arm| arm.completed + arm.incomplete == arm.runs.len() as u32)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn requests_are_bounded_and_arms_say_what_they_change() {
        let request: AnalysisRequest = serde_json::from_value(
            json!({"requestId": "why", "samples": 8, "times": 4, "faults": true}),
        )
        .unwrap();
        request.validate().unwrap();
        assert_eq!(request.property, None);
        for (samples, times, error) in [
            (0, 4, "samples must be 1 to 64"),
            (65, 0, "samples must be"),
            (8, 33, "times are at most 32"),
            (64, 8, "at most 512"),
        ] {
            let invalid = AnalysisRequest {
                samples,
                times,
                ..request.clone()
            };
            assert!(invalid.validate().unwrap_err().contains(error));
        }
        for wire in [
            json!({"kind": "baseline"}),
            json!({"kind": "reseedFrom", "atNs": 12_000_000_000u64}),
            json!({"kind": "without", "keys": ["gen-3", "gen-3-end"], "summary": "partition db1 at 4s"}),
        ] {
            let kind: AnalysisArmKind = serde_json::from_value(wire.clone()).unwrap();
            assert_eq!(serde_json::to_value(kind).unwrap(), wire);
        }
    }

    #[test]
    fn an_analysis_completes_when_every_run_is_accounted_for() {
        let arm = AnalysisArm {
            kind: AnalysisArmKind::Baseline,
            runs: vec!["k7m2p9qx".parse().unwrap(), "a7m2p9qx".parse().unwrap()],
            completed: 1,
            failed: 1,
            other: 0,
            incomplete: 0,
            interval_per_mille: None,
            exact: None,
            exact_failed: None,
        };
        let mut analysis = Analysis {
            source: "k7m2p9qx".parse().unwrap(),
            request_id: "why".into(),
            target: "oracle".into(),
            end_ns: 40_000_000_000,
            arms: vec![arm],
            stop_requested: false,
            error: None,
        };
        assert!(!analysis.complete());
        analysis.arms[0].incomplete = 1;
        assert!(analysis.complete());
        let wire = serde_json::to_value(&analysis).unwrap();
        assert!(wire["arms"][0].get("intervalPerMille").is_none());
        assert_eq!(serde_json::from_value::<Analysis>(wire).unwrap(), analysis);
    }
}
