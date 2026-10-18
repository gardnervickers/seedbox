//! Read-only logical execution evidence; not an opaque machine snapshot hash.
#[cfg(test)]
mod tests;
use super::*;
use crate::persistence::{CodecError, Encoder, Wire};
use std::io::Write;

impl<M> Barrier<'_, M>
where
    M: Machine + Send + 'static,
    M::Context: 'static,
    M::Error: Send + 'static,
{
    /// Write canonical Driver-owned policy state at this stopped boundary.
    ///
    /// Versioned evidence includes continuing RNG cursors, logical machine
    /// lifecycle/time/input state, fault/controller and network policy/queues,
    /// pending scheduled work and unstarted definitions. It excludes native
    /// machine bytes, callbacks, worker resources, wall-clock playback pacing,
    /// already-published host-drain queues and frame-observation buffers.
    /// No machine callback, native capture, allocation of a checkpoint or RNG
    /// draw occurs. An externally supplied RNG has no cursor evidence and fails.
    /// This is one component of a run witness, not CPU or RAM evidence; callers
    /// must combine it with actual stopped native execution evidence.
    // TODO: Re-evaluate: authoritative policy is not the event transcript or a
    // serialized whole-Driver checkpoint. The narrow stopped writer permits
    // streaming into a digest without cloning native images or exposing fields.
    pub fn write_policy_evidence(
        &self,
        output: &mut impl Write,
    ) -> std::result::Result<(), CodecError> {
        self.driver.execution.write_policy_evidence(output)
    }
}

impl<M> ExecutionState<M> {
    fn write_policy_evidence(
        &self,
        output: &mut impl Write,
    ) -> std::result::Result<(), CodecError> {
        // The writer can see execution only, never host intent or observation.
        let ExecutionState {
            clock,
            rng,
            quantum_ns,
            quantum_end,
            machines,
            events,
            boundary_index,
            completed_quanta,
            quantum_committed,
            failed,
            stopped,
            oracle,
            finished,
            oracle_stops,
            control,
            inputs,
            trigger_marker,
            definitions,
            network,
        } = self;
        let SimulationRng::Seeded(rng) = rng else {
            return Err(CodecError::Invalid(
                "policy evidence requires the owned seeded Driver RNG",
            ));
        };
        output.write_all(b"SBXPOLE\0")?;
        let mut e = Encoder { output };
        2u32.put(&mut e)?;
        Timestamp::from_nanos(clock.now()).put(&mut e)?;
        rng.put(&mut e)?;
        quantum_ns.put(&mut e)?;
        quantum_end.put(&mut e)?;
        (machines.len() as u64).put(&mut e)?;
        for entry in machines {
            let Entry {
                machine: _,
                rng,
                lifecycle,
                time,
                position,
                timer,
                reached,
                stopped,
                inbox,
                staged,
                last_ended_published,
            } = entry;
            rng.put(&mut e)?;
            lifecycle.put(&mut e)?;
            time.put(&mut e)?;
            position.put(&mut e)?;
            timer.put(&mut e)?;
            reached.put(&mut e)?;
            stopped.put(&mut e)?;
            inbox.put(&mut e)?;
            staged.put(&mut e)?;
            last_ended_published.put(&mut e)?;
        }
        events.put(&mut e)?;
        boundary_index.put(&mut e)?;
        completed_quanta.put(&mut e)?;
        quantum_committed.put(&mut e)?;
        // Temporary host pause/speed/step controls do not change simulated
        // execution identity. Permanent stop is continuing logical authority.
        stopped.put(&mut e)?;
        failed.put(&mut e)?;
        oracle.put(&mut e)?;
        finished.put(&mut e)?;
        oracle_stops.put(&mut e)?;
        control.put(&mut e)?;
        inputs.put(&mut e)?;
        trigger_marker.put(&mut e)?;
        definitions.put(&mut e)?;
        network.policy_evidence(&mut e)
    }
}
