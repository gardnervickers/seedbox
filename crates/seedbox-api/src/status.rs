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

    /// The verdict policy once guest property results exist. Not applied
    /// yet: [`crate::SimulationStatus::verdict`] stays oracle-only until the
    /// engine reports property results, and this function moves beside the
    /// property reducer in `seedbox-engine-core` when it does.
    ///
    /// A violated safety property fails a completed run whatever the oracle
    /// said. Lost or invalid property evidence makes a passing oracle's run
    /// incomplete. Results arrive with the final report, so a passing oracle
    /// stays pending until completion rather than passing first and failing
    /// later. Unmet goals never change a verdict.
    pub fn with_properties(
        oracle: Self,
        phase: Phase,
        properties: Option<crate::PropertyAssessmentStatus>,
    ) -> Self {
        use crate::PropertyAssessmentStatus as AssessmentStatus;
        if phase != Phase::Completed {
            return if oracle == Self::Passed {
                Self::Pending
            } else {
                oracle
            };
        }
        match (oracle, properties) {
            (_, Some(AssessmentStatus::Violated)) => Self::Failed,
            (Self::Passed, Some(AssessmentStatus::Incomplete)) => Self::Incomplete,
            (oracle, _) => oracle,
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
    fn properties_qualify_only_a_passing_oracle_after_completion() {
        use crate::PropertyAssessmentStatus::*;
        use Phase::{Completed, Finalizing, Running};
        use TestVerdict as V;
        for (code, phase, properties, expected) in [
            (Some(0), Completed, None, V::Passed),
            (Some(0), Completed, Some(Passed), V::Passed),
            (Some(0), Completed, Some(Violated), V::Failed),
            (Some(0), Completed, Some(Incomplete), V::Incomplete),
            (Some(0), Finalizing, Some(Passed), V::Pending),
            (Some(0), Running, None, V::Pending),
            (Some(3), Completed, Some(Passed), V::Failed),
            (Some(3), Running, None, V::Failed),
            (None, Completed, Some(Violated), V::Failed),
            (None, Completed, Some(Incomplete), V::Incomplete),
            (None, Running, Some(Violated), V::Pending),
        ] {
            let oracle = V::from_oracle(code, phase);
            assert_eq!(
                V::with_properties(oracle, phase, properties),
                expected,
                "{code:?} {phase:?} {properties:?}"
            );
        }
    }
}
