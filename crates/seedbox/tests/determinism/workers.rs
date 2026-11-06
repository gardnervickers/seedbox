//! Machine worker count is a per-submission performance option
//! (`options.machineWorkers`); it must never change any result.
use crate::session::{err, Session, REAL_TIME};
use crate::trace::{same_output, suffix, Trace};
use crate::{input, Outcome, SECOND};
use seedbox_api::{ComparisonStatus, PlaybackRate, SimulationId};
use serde_json::{json, Value};

/// Single-thread reference runs and what their observers saw.
pub struct References {
    /// Name, the document that reruns it, the reference run and its trace.
    runs: Vec<(&'static str, Value, SimulationId, Trace)>,
}

impl References {
    pub fn ids(&self) -> Vec<&SimulationId> {
        self.runs.iter().map(|(_, _, id, _)| id).collect()
    }
}

/// The shared definition with machines slowed by different factors (so
/// rounds finish at very different host times), and whole-machine
/// pause/resume, process pause/resume and crash/reboot beside the shared
/// program's loss, throttle and heal, and the oracle's partition.
///
/// The original variant also set per-machine CPU speeds and per-call jitter;
/// main's definitions have neither (crates/seedbox-spec/src/definition.rs
/// `Machine`), so throttles stand in for the speed differences.
pub fn lifecycle_document(document: &Value) -> Result<Value, String> {
    let mut resource: seedbox_api::Simulation =
        serde_json::from_value(document.clone()).map_err(err)?;
    resource.spec.program.extend([
        input(
            "oracle-slow",
            SECOND,
            json!({"action": "throttle", "handle": "oracle-slow", "machine": "oracle",
                   "factor_permille": 4000}),
        ),
        input(
            "server-slow",
            SECOND,
            json!({"action": "throttle", "handle": "server-slow", "machine": "server",
                   "factor_permille": 2000}),
        ),
        input(
            "server-fast",
            2 * SECOND + SECOND / 5,
            json!({"action": "heal", "handle": "server-slow"}),
        ),
        input(
            "pause-server",
            2 * SECOND + SECOND / 2,
            json!({"action": "pause", "machine": "server"}),
        ),
        input(
            "resume-server",
            3 * SECOND,
            json!({"action": "resume", "machine": "server"}),
        ),
        input(
            "freeze-server",
            3 * SECOND + SECOND / 2,
            json!({"action": "pause_process", "machine": "server"}),
        ),
        input(
            "thaw-server",
            3 * SECOND + 4 * SECOND / 5,
            json!({"action": "resume_process", "machine": "server"}),
        ),
        input(
            "crash-worker",
            4 * SECOND,
            json!({"action": "crash", "machine": "worker"}),
        ),
        input(
            "reboot-worker",
            4 * SECOND + 3 * SECOND / 5,
            json!({"action": "reboot", "machine": "worker"}),
        ),
    ]);
    resource.spec.program.sort_by_key(|input| input.at_ns);
    serde_json::to_value(&resource).map_err(err)
}

impl Session {
    /// On one machine thread: the baseline, the lifecycle variant, an
    /// interactively controlled and checkpointed lifecycle run, and a branch
    /// from that checkpoint, each with everything its observers saw.
    pub async fn worker_references(&self, baseline: &SimulationId) -> Result<References, String> {
        let lifecycle = lifecycle_document(&self.document)?;
        let varied = self.submit(&lifecycle, false).await?;
        self.complete(&varied).await?;
        let comparison = self.client.compare(baseline, &varied).await.map_err(err)?;
        if comparison.status == ComparisonStatus::Match {
            return Err(format!(
                "{varied}: the lifecycle variant left the run unchanged"
            ));
        }
        let world = self.client.world(&varied).await.map_err(err)?;
        for action in [
            "throttle",
            "pause",
            "resume",
            "pause_process",
            "resume_process",
            "crash",
            "reboot",
        ] {
            if !world.faults.iter().any(|f| f.action == action) {
                return Err(format!("{varied}: no {action} applied: {:?}", world.faults));
            }
        }

        // Live controls at host-chosen boundaries, retained for replay.
        let interactive = self.submit(&lifecycle, true).await?;
        self.live(&interactive).await?;
        // In real time, so the controls land near their targets (positions
        // are sampled every 250 ms).
        self.rate(&interactive, REAL_TIME).await?;
        if !self.reach(&interactive, 2 * SECOND).await? {
            return Err(format!("{interactive} ended before 2 s"));
        }
        self.inject(
            &interactive,
            json!({"action": "partition", "handle": "live-cut",
                   "left": ["oracle"], "right": ["server"]}),
        )
        .await?;
        if !self.reach(&interactive, 5 * SECOND).await? {
            return Err(format!("{interactive} ended before 5 s"));
        }
        self.inject(
            &interactive,
            json!({"action": "heal", "handle": "live-cut"}),
        )
        .await?;
        if !self.reach(&interactive, 9 * SECOND).await? {
            return Err(format!("{interactive} ended before 9 s"));
        }
        let (checkpoint, at) = self.checkpoint(&interactive).await?;
        self.rate(&interactive, PlaybackRate::Unlimited).await?;
        self.complete(&interactive).await?;
        let branched = self.checkpoint_document(&interactive, &checkpoint).await?;
        let branch = self.submit(&branched, false).await?;
        self.complete(&branch).await?;
        // The branch restores the rebooted worker's later incarnation and
        // must continue the captured run's output exactly.
        same_output(
            &suffix(self.logs(&interactive).await?, at),
            &self.logs(&branch).await?,
        )
        .map_err(|e| format!("{branch} output differs from {interactive} after {at} ns:\n{e}"))?;

        // Live admission and checkpoint capture publish host-timed events
        // that a rerun of the retained timeline does not, so the controlled
        // run's observations are those of its single-thread rerun.
        let recorded = self.recorded_document(&interactive).await?;
        let replayed = self.submit(&recorded, false).await?;
        self.complete(&replayed).await?;
        self.same_results(&interactive, &replayed)
            .await
            .map_err(|e| format!("single-thread replay of the controlled run: {e}"))?;

        let mut runs = Vec::new();
        for (name, document, id, observed) in [
            ("baseline", self.document.clone(), baseline, baseline),
            ("lifecycle", lifecycle, &varied, &varied),
            ("interactive", recorded, &interactive, &replayed),
            ("branch", branched, &branch, &branch),
        ] {
            for id in [id, observed] {
                if self.machine_workers(id).await? != 1 {
                    return Err(format!("{id}: the reference did not run on one thread"));
                }
            }
            runs.push((
                name,
                document,
                id.clone(),
                Trace::read(self, observed).await?,
            ));
        }
        Ok(References { runs })
    }

    /// With `workers` machine threads: rerunning each reference (including
    /// its retained live controls) and branching from its checkpoint must
    /// reproduce outcome, witness, full state and fault history, and exactly
    /// the logs, events, faults and timeline of the single-thread references.
    pub async fn worker_invariance(&self, workers: u32, references: &References) -> Outcome {
        for (name, document, reference, trace) in &references.runs {
            let rerun = &self.submit_with(document, false, Some(workers)).await?;
            self.complete(rerun).await?;
            let used = self.machine_workers(rerun).await?;
            if used != workers {
                return Err(format!("{name}: {rerun} ran on {used} machine workers"));
            }
            self.same_results(reference, rerun)
                .await
                .map_err(|e| format!("{name}: {e}"))?;
            trace
                .same(&Trace::read(self, rerun).await?)
                .map_err(|e| format!("{name} {reference} vs {rerun}: {e}"))?;
        }
        Ok(())
    }
}
