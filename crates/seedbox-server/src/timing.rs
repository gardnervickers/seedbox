//! Monotonic, server-owned stage accounting. Reads project elapsed time without
//! writing the catalogue or manufacturing a resource revision.
use seedbox_api::{SimulationStatus, Stage, StageTiming};
use std::time::Instant;

#[derive(Clone, Copy, Default)]
pub(crate) struct StageClock(Option<(Stage, Instant, u64)>);

impl StageClock {
    pub fn project(&self, status: &mut SimulationStatus, now: Instant) {
        if let Some((stage, started, base)) = self.0 {
            let elapsed = u64::try_from(now.duration_since(started).as_nanos()).unwrap_or(u64::MAX);
            status.stage_timings.insert(
                stage,
                StageTiming {
                    elapsed_wall_ns: base.saturating_add(elapsed),
                    complete: false,
                },
            );
        }
    }

    /// Prepare the next clock alongside a durable mutation; the caller installs
    /// it only after the catalogue write succeeds.
    pub fn transition(&self, status: &mut SimulationStatus, now: Instant) -> Self {
        self.project(status, now);
        if self.0.map(|(stage, _, _)| stage) == status.stage {
            return *self;
        }
        if let Some((stage, _, _)) = self.0 {
            if let Some(timing) = status.stage_timings.get_mut(&stage) {
                timing.complete = true;
            }
        }
        Self(status.stage.map(|stage| {
            let timing = status.stage_timings.entry(stage).or_insert(StageTiming {
                elapsed_wall_ns: 0,
                complete: false,
            });
            timing.complete = false;
            (stage, now, timing.elapsed_wall_ns)
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn reads_do_not_double_count_and_failure_closes_the_current_stage() {
        let start = Instant::now();
        let mut status = SimulationStatus {
            stage: Some(Stage::Preparing),
            ..Default::default()
        };
        let clock = StageClock::default().transition(&mut status, start);
        let at = |ns| start + Duration::from_nanos(ns);
        clock.project(&mut status, at(10));
        clock.project(&mut status, at(20));
        assert_eq!(status.stage_timings[&Stage::Preparing].elapsed_wall_ns, 20);
        status.stage = Some(Stage::Executing);
        let clock = clock.transition(&mut status, at(30));
        assert_eq!(
            status.stage_timings[&Stage::Preparing],
            StageTiming {
                elapsed_wall_ns: 30,
                complete: true
            }
        );
        clock.project(&mut status, at(50));
        status.stage = None;
        let clock = clock.transition(&mut status, at(60));
        clock.project(&mut status, at(100));
        assert_eq!(
            status.stage_timings[&Stage::Executing],
            StageTiming {
                elapsed_wall_ns: 30,
                complete: true
            }
        );
    }
}
