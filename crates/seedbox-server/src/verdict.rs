//! The test verdict the server stores in every simulation's status: the
//! oracle's verdict qualified by the guest property findings under the
//! engine's policy, so `seedbox-api` stays data-only.
use seedbox_api::{Phase, SimulationStatus, TestVerdict};
use seedbox_engine_core::properties::{self as policy, Findings, Verdict};

/// The verdict `status` earns: see [`policy::verdict`].
pub fn of(status: &SimulationStatus) -> TestVerdict {
    let oracle = match TestVerdict::from_oracle(status.oracle_exit_code, status.phase) {
        TestVerdict::Pending => Verdict::Pending,
        TestVerdict::Incomplete => Verdict::Incomplete,
        TestVerdict::Passed => Verdict::Passed,
        TestVerdict::Failed => Verdict::Failed,
    };
    let findings = status.properties.as_ref().map(|p| Findings {
        violated: !p.violations.is_empty(),
        unmet: !p.unmet_requirements.is_empty(),
        lost: p.lost_count > 0,
    });
    match policy::verdict(
        oracle,
        status.oracle_exit_code.is_some(),
        status.phase == Phase::Completed,
        findings,
    ) {
        Verdict::Pending => TestVerdict::Pending,
        Verdict::Incomplete => TestVerdict::Incomplete,
        Verdict::Passed => TestVerdict::Passed,
        Verdict::Failed => TestVerdict::Failed,
    }
}

/// Recompute the stored verdict; every catalogue write goes through this.
pub(crate) fn qualify(status: &mut SimulationStatus) {
    status.verdict = of(status);
}

#[cfg(test)]
mod tests {
    use super::*;
    use seedbox_api::PropertyAssessment;

    #[test]
    fn the_stored_verdict_follows_the_oracle_and_properties() {
        let unmet = PropertyAssessment {
            unmet_requirements: vec!["failover exercised".into()],
            ..Default::default()
        };
        let lost = PropertyAssessment {
            lost_count: 1,
            ..Default::default()
        };
        for (phase, exit, properties, expected) in [
            (Phase::Running, None, None, TestVerdict::Pending),
            (Phase::Running, Some(0), None, TestVerdict::Pending),
            (Phase::Completed, Some(0), None, TestVerdict::Passed),
            (Phase::Completed, Some(23), None, TestVerdict::Failed),
            (Phase::Completed, Some(3), None, TestVerdict::Incomplete),
            (Phase::Completed, None, None, TestVerdict::Incomplete),
            (Phase::Completed, Some(0), Some(&unmet), TestVerdict::Failed),
            (Phase::Completed, Some(3), Some(&unmet), TestVerdict::Failed),
            (
                Phase::Completed,
                None,
                Some(&unmet),
                TestVerdict::Incomplete,
            ),
            (
                Phase::Completed,
                Some(0),
                Some(&lost),
                TestVerdict::Incomplete,
            ),
        ] {
            let mut status = SimulationStatus {
                phase,
                oracle_exit_code: exit,
                properties: properties.cloned(),
                // A supplied verdict never stands.
                verdict: TestVerdict::Passed,
                ..Default::default()
            };
            qualify(&mut status);
            assert_eq!(
                status.verdict, expected,
                "{phase:?} {exit:?} {properties:?}"
            );
        }
    }
}
