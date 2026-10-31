use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Phase {
    Queued,
    Preparing,
    Running,
    Finalizing,
    Completed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Stage {
    Preparing,
    Booting,
    Executing,
    Finalizing,
}

/// Monotonic host duration, unrelated to the guest's simulated clock.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct StageTiming {
    pub elapsed_wall_ns: u64,
    /// The server observed the entire interval. After a server crash, an
    /// interrupted stage retains its last durable lower bound with false here.
    pub complete: bool,
}

/// Why execution stopped. This never supplies or overrides the test verdict.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "reason", rename_all = "camelCase", deny_unknown_fields)]
pub enum ExecutionTermination {
    OracleExited,
    Stopped,
    SimulatedTimeLimit,
    WallTimeLimit,
    PreparationTimeLimit,
    Quiescent,
    Interrupted,
    Error { message: String },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum TestVerdict {
    Pending,
    Incomplete,
    Passed,
    Failed,
}

impl TestVerdict {
    /// Only a committed exit from the designated workload oracle determines
    /// pass/fail. Execution errors, evidence comparisons and cleanup cannot
    /// alter an observed result.
    pub fn from_oracle(exit_code: Option<u8>, phase: Phase) -> Self {
        match exit_code {
            Some(0) => Self::Passed,
            Some(_) => Self::Failed,
            None if phase == Phase::Completed => Self::Incomplete,
            None => Self::Pending,
        }
    }

    /// The verdict policy once guest property findings exist. Not applied
    /// yet: [`crate::SimulationStatus::verdict`] stays oracle-only until the
    /// engine reports property findings.
    ///
    /// A violated property fails a completed run whatever the oracle said,
    /// even one whose oracle never exited: the violation is definite
    /// evidence. A required property never evaluated fails a run whose
    /// oracle exited; a run cut short before its oracle exited stays as the
    /// oracle left it, since the workload may not have reached it. Lost
    /// evidence makes a passing oracle's run incomplete. Findings arrive with
    /// the final report, so a passing oracle stays pending until completion
    /// rather than passing first and failing later.
    pub fn with_properties(
        exit_code: Option<u8>,
        phase: Phase,
        properties: Option<&crate::PropertyAssessment>,
    ) -> Self {
        let oracle = Self::from_oracle(exit_code, phase);
        if phase != Phase::Completed {
            return if oracle == Self::Passed {
                Self::Pending
            } else {
                oracle
            };
        }
        let Some(properties) = properties else {
            return oracle;
        };
        if !properties.violations.is_empty()
            || (exit_code.is_some() && !properties.unmet_requirements.is_empty())
        {
            Self::Failed
        } else if oracle == Self::Passed && properties.lost_count > 0 {
            Self::Incomplete
        } else {
            oracle
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oracle_is_the_only_source_of_pass_fail() {
        for phase in [
            Phase::Queued,
            Phase::Preparing,
            Phase::Running,
            Phase::Finalizing,
            Phase::Completed,
        ] {
            assert_eq!(
                TestVerdict::from_oracle(Some(0), phase),
                TestVerdict::Passed
            );
            for code in 1..=255 {
                assert_eq!(
                    TestVerdict::from_oracle(Some(code), phase),
                    TestVerdict::Failed
                );
            }
        }
        assert_eq!(
            TestVerdict::from_oracle(None, Phase::Running),
            TestVerdict::Pending
        );
        assert_eq!(
            TestVerdict::from_oracle(None, Phase::Completed),
            TestVerdict::Incomplete
        );
    }

    #[test]
    fn property_findings_qualify_the_oracle_after_completion() {
        use crate::PropertyAssessment;
        use Phase::{Completed, Finalizing, Running};
        use TestVerdict as V;
        let violated = PropertyAssessment {
            violations: vec![crate::PropertyViolation {
                id: "p".into(),
                kind: crate::PropertyKind::Always,
                example: crate::PropertyExample {
                    origin: crate::PropertyOrigin {
                        machine: "n1".into(),
                        incarnation: 0,
                        session: 1,
                        seq: 1,
                        time_ns: 0,
                    },
                    details: None,
                    details_truncated: false,
                },
            }],
            ..Default::default()
        };
        let unmet = PropertyAssessment {
            unmet_requirements: vec!["p".into()],
            ..Default::default()
        };
        let lost = PropertyAssessment {
            lost_count: 1,
            ..Default::default()
        };
        let none = PropertyAssessment::default();
        for (code, phase, properties, expected) in [
            (Some(0), Completed, None, V::Passed),
            (Some(0), Completed, Some(&none), V::Passed),
            (Some(0), Completed, Some(&violated), V::Failed),
            (Some(0), Completed, Some(&unmet), V::Failed),
            (Some(0), Completed, Some(&lost), V::Incomplete),
            (Some(0), Finalizing, Some(&none), V::Pending),
            (Some(0), Running, None, V::Pending),
            (Some(3), Completed, Some(&none), V::Failed),
            (Some(3), Completed, Some(&unmet), V::Failed),
            (Some(3), Running, None, V::Failed),
            (None, Completed, Some(&violated), V::Failed),
            (None, Completed, Some(&unmet), V::Incomplete),
            (None, Completed, Some(&lost), V::Incomplete),
            (None, Running, Some(&violated), V::Pending),
        ] {
            assert_eq!(
                V::with_properties(code, phase, properties),
                expected,
                "{code:?} {phase:?} {properties:?}"
            );
        }
    }
}
