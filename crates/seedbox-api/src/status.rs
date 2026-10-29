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
    /// The oracle's own verdict: only a committed exit from the designated
    /// workload oracle determines it. Execution errors, evidence comparisons
    /// and cleanup cannot alter an observed result; the server qualifies it
    /// by the guest property findings into [`crate::SimulationStatus::verdict`].
    /// Exit status 3 means the oracle could not decide, so it is incomplete.
    pub fn from_oracle(exit_code: Option<u8>, phase: Phase) -> Self {
        match exit_code {
            Some(0) => Self::Passed,
            // The oracle could not decide: neither a pass nor a bug.
            Some(3) => Self::Incomplete,
            Some(_) => Self::Failed,
            None if phase == Phase::Completed => Self::Incomplete,
            None => Self::Pending,
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
                let expected = if code == 3 {
                    TestVerdict::Incomplete
                } else {
                    TestVerdict::Failed
                };
                assert_eq!(TestVerdict::from_oracle(Some(code), phase), expected);
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
}
