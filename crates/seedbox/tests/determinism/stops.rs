//! Where rounds end and the quantum are host settings: neither may change a
//! result.
use crate::session::{err, Session};
use crate::trace::{same_output, Record};
use crate::Outcome;
use futures_util::StreamExt;
use seedbox_api::{EventDelivery, SimulationId};

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

    /// In a server whose runs take a stop policy, each run ends its rounds at
    /// times seeded by its own output directory, so two runs of the baseline's
    /// definition stop differently from each other and from the Driver's own
    /// rounds (`rounds` of them in the baseline). They must match each other
    /// and write the baseline's output.
    pub async fn stop_invariance(&self, baseline: &[Record], rounds: u64) -> Outcome {
        let first = self.submit(&self.document, false).await?;
        let second = self.submit(&self.document, false).await?;
        self.complete(&first).await?;
        self.complete(&second).await?;
        // The policy only adds barriers, so a run it stopped has more.
        let counts = [
            self.boundaries(&first).await?,
            self.boundaries(&second).await?,
        ];
        if counts.iter().any(|&count| count <= rounds) || counts[0] == counts[1] {
            return Err(format!(
                "the stop policy did not take effect: {counts:?} barriers against the baseline's {rounds}"
            ));
        }
        self.same_results(&first, &second).await?;
        for run in [&first, &second] {
            same_output(baseline, &self.logs(run).await?).map_err(|e| {
                format!("{run}, stopped by a policy, differs from the baseline's output:\n{e}")
            })?;
        }
        Ok(())
    }

    /// The last barrier position any of a completed run's events names: how
    /// many times its Driver committed, a host count.
    pub async fn boundaries(&self, id: &SimulationId) -> Result<u64, String> {
        let mut stream = self.client.events_with(id, false).await.map_err(err)?;
        let mut last = 0;
        while let Some(delivery) = stream.next().await {
            match delivery.map_err(err)? {
                EventDelivery::Event { observation, .. } => {
                    if let Some(position) = observation.position {
                        last = last.max(position.boundary_index);
                    }
                }
                EventDelivery::Closed => break,
                EventDelivery::Gap { first, last } => {
                    return Err(format!("{id}: event gap {first}..{last}"));
                }
            }
        }
        Ok(last)
    }
}
