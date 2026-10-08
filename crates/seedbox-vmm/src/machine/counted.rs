//! The counter timebase: guest progress is the host perf count of retired
//! guest conditional branches. A run is bounded by an overflow interrupt, and
//! the final approach to a hard stop is made by breakpoint stepping, or by an
//! undo-able overflow aimed close to it.

use super::Machine;
use crate::boundary::missed_soft_stop;
use crate::counter::Counter;
use crate::timebase::{Judge, Prepared, Run, Timebase, Verdict};

impl Timebase for Counter {
    const BRANCHES: bool = true;

    fn prepare(&mut self, m: &mut Machine, remaining: u64, _soft: u64) -> Result<Prepared, String> {
        let undo = m.undo.is_some();
        let threshold = if undo { m.fm } else { m.margin };
        if remaining > threshold {
            m.set_stepping(false)?;
            let wake = if (m.adaptive_margin || undo) && remaining > m.profile.approach {
                // Approach a hard stop in two overflows: the first measures
                // the branch rate of the code actually running near it.
                remaining - m.profile.approach
            } else {
                // Final approach. With a checkpoint, aim close and undo an
                // overshoot rather than step through a worst-case skid.
                if undo && !m.undo.as_ref().unwrap().valid {
                    m.checkpoint()?;
                }
                remaining - threshold
            };
            let now = self.last();
            if wake >= crate::counter::NEVER {
                self.disarm();
                m.armed_wake = 0;
            } else {
                self.arm(now, now + wake);
                m.armed_wake = wake;
            }
        } else {
            if !m.stepping {
                self.disarm();
            }
            m.arm_step()?;
        }
        Ok(Prepared::Run)
    }

    fn start(&mut self, _syscalls: u64) -> u64 {
        self.last()
    }

    fn read(&mut self, _syscalls: u64) -> u64 {
        self.read_fast()
    }

    fn inject_guard(&self) -> u64 {
        super::INJECT_GUARD
    }

    fn judge(&mut self, j: Judge<'_>, run: &Run) -> Result<Verdict, String> {
        let Run { delta, remaining, rem_soft, windowed, window, interrupted, debug, run_ns, syscalled } = *run;
        // A free run that reaches the hard count, even by a natural exit
        // exactly there, has passed the counted stop: stepping would have
        // stopped before that instruction.
        let overshoot = delta > remaining || (!j.stepping && delta == remaining);
        // With the yield window closed, a run must not pass `soft`: a
        // syscall there would have been the stop.
        let soft_miss = missed_soft_stop(
            windowed,
            window,
            delta,
            rem_soft,
            interrupted,
            j.stepping,
        );
        if (overshoot || soft_miss) && j.undo_valid {
            if overshoot && *j.fm >= j.profile.undo_max {
                return Err(format!(
                    "overshoot on {}: {} branches past stop target at final margin {}",
                    j.name,
                    delta - remaining,
                    j.fm
                ));
            }
            // A rollback costs far less than stepping, so grow slowly.
            *j.fm = (*j.fm * 2).min(j.profile.undo_max);
            if !overshoot {
                j.stats.soft_undos += 1;
            }
            return Ok(Verdict::Rollback);
        }
        // Changing the margin must never change where a stop lands. A
        // paravirtual guest counts syscalls: a run without one skipped no
        // stop. Otherwise the run fails.
        if soft_miss && !overshoot {
            j.stats.soft_misses += 1;
            if !j.pv || syscalled {
                return Err(format!(
                    "soft overshoot on {}: {} branches past (or an ambiguous overflow at) a soft stop before its syscall window opened",
                    j.name,
                    delta - rem_soft
                ));
            }
            j.stats.soft_misses_clean += 1;
        }
        if overshoot {
            return Ok(Verdict::Overshoot);
        }
        if interrupted && !j.stepping {
            let skid = delta.saturating_sub(j.armed_wake);
            j.stats.skid_max = j.stats.skid_max.max(skid);
            j.stats.skids.push(skid);
        }
        // Overflow skid is roughly constant in host time, so its size in
        // branches follows the guest's recent branch rate (Go's GC scan loop
        // is several times denser than ordinary code). Size the margin from
        // that rate. Performance only: stops land on the same count.
        if j.adaptive_margin && !j.stepping && run_ns > 20_000 && delta > 0 {
            let rate = delta as f64 / run_ns as f64; // branches per host ns
            *j.margin = ((rate * j.profile.skid_ns) as u64).clamp(j.profile.margin_min, j.profile.approach);
        }
        let mut legal = !interrupted;
        if debug {
            legal = delta == remaining;
        }
        Ok(Verdict::Advance { legal })
    }
}
