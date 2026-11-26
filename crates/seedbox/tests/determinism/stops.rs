//! Where rounds end and the quantum are host settings: neither may change a
//! result.
use crate::session::{err, within, Session};
use crate::trace::{same_output, Record};
use crate::Outcome;
use seedbox_api::{ControlCommand, SimulationId};

/// Quanta a stepped run takes, one host step each, before it plays to the end.
const STEPS: usize = 20;
/// The stop policy seeds of the two runs [`Session::stop_invariance`] makes.
const STOP_SEEDS: [u64; 2] = [4242, 4243];

impl Session {
    // ---- QuantumInvariance --------------------------------------------------------

    /// The baseline's definition with another `simulator.quantum` must run to
    /// the same results: the quantum caps rounds and paces playback, and
    /// where rounds end changes nothing a machine does.
    pub async fn quantum_invariance(&self, baseline: &SimulationId, quantum_ns: u64) -> Outcome {
        let mut document = self.document.clone();
        let simulator = document
            .pointer_mut("/spec/simulator/quantumNs")
            .ok_or("the resolved document has no simulator quantum")?;
        if simulator.as_u64() == Some(quantum_ns) {
            return Err(format!("the baseline already runs with {quantum_ns} ns"));
        }
        *simulator = quantum_ns.into();
        let other = self.submit(&document, false).await?;
        self.complete(&other).await?;
        self.same_results(baseline, &other).await
    }

    // ---- StopPolicyInvariance -----------------------------------------------------

    /// Two runs of the baseline's definition whose submissions name stop
    /// policy seeds end their rounds at times drawn from them, so they stop
    /// differently from each other and from the Driver's own rounds, whose
    /// barrier counts after each of the first steps are `reference`. Each
    /// must match the baseline and write its output.
    pub async fn stop_invariance(
        &self,
        baseline: &SimulationId,
        output: &[Record],
        reference: &[u64],
    ) -> Outcome {
        let (first, first_counts) = self.stepped(Some(STOP_SEEDS[0])).await?;
        let (second, second_counts) = self.stepped(Some(STOP_SEEDS[1])).await?;
        // Without a policy every run commits at the same barriers. A policy
        // only adds barriers, so one that acted shows more, and two runs
        // seeded apart show different ones.
        let more = |counts: &[u64]| counts.last() > reference.last();
        if !more(&first_counts) || !more(&second_counts) || first_counts == second_counts {
            return Err(format!(
                "the stop policy did not take effect: barriers after each step \
                 {first_counts:?} and {second_counts:?}, and the Driver's own {reference:?}"
            ));
        }
        for (run, seed) in [(&first, STOP_SEEDS[0]), (&second, STOP_SEEDS[1])] {
            // The worker names the seed it drew its stops from, which shows it
            // took the policy; submitting the seed again repeats the stops.
            self.stop_seed(run, seed)?;
            let checked = async {
                self.same_results(baseline, run).await?;
                same_output(output, &self.logs(run).await?)
                    .map_err(|e| format!("differs from the baseline's output:\n{e}"))
            };
            checked
                .await
                .map_err(|error| format!("{run}, stopped by policy seed {seed}: {error}"))?;
        }
        self.same_results(&first, &second).await
    }

    /// Check that the run's worker said, on its retained stderr, that it drew
    /// its stops from `seed`.
    fn stop_seed(&self, id: &SimulationId, seed: u64) -> Outcome {
        let said = self.cli(&[
            "show",
            &id.to_string(),
            "--view",
            "diagnostics",
            "--file",
            "vmm.stderr",
        ])?;
        let notice = format!("Driver stop policy seed {seed}");
        if String::from_utf8_lossy(&said)
            .lines()
            .any(|line| line.ends_with(&notice))
        {
            Ok(())
        } else {
            Err(format!(
                "{id}: its worker did not name stop policy seed {seed}"
            ))
        }
    }

    /// Run the definition paused, its submission naming `stop_policy`, step
    /// its first [`STEPS`] quanta, then play it to the end. Returns the run
    /// and how many barriers its Driver had committed after each step: a host
    /// count, which only a stop policy changes.
    pub async fn stepped(
        &self,
        stop_policy: Option<u64>,
    ) -> Result<(SimulationId, Vec<u64>), String> {
        let mut document = self.document.clone();
        if let Some(seed) = stop_policy {
            document["options"]["stopPolicy"] = seed.into();
        }
        let id = self.submit(&document, true).await?;
        self.live(&id).await?;
        let mut counts = Vec::with_capacity(STEPS);
        for _ in 0..STEPS {
            self.control(&id, ControlCommand::Step).await?;
            // The acknowledgement admits the quantum; it has run once the
            // worker no longer reports a step in progress.
            let count = within(120, || format!("{id}: step did not commit"), async {
                loop {
                    let state = self.client.playback(&id).await.map_err(err)?.state;
                    if state.terminal {
                        return Err(format!("{id} ended within {STEPS} steps"));
                    }
                    if !state.stepping {
                        return Ok(state.position.boundary_index);
                    }
                }
            })
            .await?;
            counts.push(count);
        }
        self.finish(&id).await?;
        self.complete(&id).await?;
        Ok((id, counts))
    }
}
