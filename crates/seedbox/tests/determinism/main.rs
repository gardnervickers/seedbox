//! Whole-run determinism oracles over the public server API.
//!
//! One resolved definition (harness/fixtures/determinism plus external inputs)
//! runs once as the baseline. Each oracle then runs the same definition under a
//! different host arrangement, or an edited copy of it, and checks the server's
//! comparison of witness, full state and fault history, or the unchanged prefix
//! of logs, events and faults. Any host leak into the simulated world fails it.
//! The last oracles cross a server session: in a second server with a fresh
//! state directory, fresh resolutions must repeat the first session's output,
//! and a checkpoint file exported from the first must branch.
//! The first session's submissions run on 3 machine workers and the second's
//! at the server's default. A separate test runs references on one
//! thread, then reruns and branches them with 2, 3 and 8 workers.
//!
//! Each test prints and records (under `SEEDBOX_TEST_REPORT_DIR/determinism`)
//! every oracle's verdict and wall time, and on a failure the observations of
//! each simulation the failure names.
#![cfg(any(
    all(target_os = "linux", target_arch = "x86_64"),
    all(target_os = "macos", target_arch = "aarch64")
))]

mod cross_session;
mod oracles;
mod session;
mod trace;
mod workers;

use seedbox_api::SimulationId;
use seedbox_spec::definition::Override;
use seedbox_test::process::Guarded;
use serde_json::{json, Value};
use session::{Server, Session};
use std::{
    future::Future,
    path::{Path, PathBuf},
    process::Command,
    time::Instant,
};

const SEED: u64 = 4242;
const SECOND: u64 = 1_000_000_000;
type Outcome = Result<(), String>;

fn input(id: &str, at_ns: u64, action: Value) -> Override {
    Override {
        id: id.into(),
        at_ns,
        action,
        timeout_ns: seedbox_spec::definition::default_timeout_ns(),
    }
}

/// External inputs of the shared definition. `lossy` keeps seeded loss and
/// jitter active from 5 s to 15 s; the oracle partitions and heals at 7–8 s.
fn program() -> Vec<Override> {
    vec![
        input(
            "lossy",
            5 * SECOND,
            json!({"action": "shape", "handle": "lossy", "from": ["worker"],
                   "to": ["server"], "latency_ns": 3_000_000, "jitter_ns": 2_000_000,
                   "loss_ppm": 100_000}),
        ),
        input(
            "slow",
            10 * SECOND,
            json!({"action": "throttle", "handle": "slow", "machine": "worker",
                   "factor_permille": 3000}),
        ),
        input(
            "unshape",
            15 * SECOND,
            json!({"action": "heal", "handle": "lossy"}),
        ),
    ]
}

/// One test's server state, verdicts and timings.
struct Run {
    cli: PathBuf,
    directory: tempfile::TempDir,
    /// State directory of the server now running.
    live: PathBuf,
    reports: PathBuf,
    lines: Vec<String>,
    failures: Vec<String>,
}

impl Run {
    fn new(name: &str) -> Self {
        let cli = std::env::var_os("SEEDBOX_TEST_CLI")
            .expect("harness/check.sh supplies the current release CLI");
        let reports = std::env::var_os("SEEDBOX_TEST_REPORT_DIR")
            .expect("set a fresh SEEDBOX_TEST_REPORT_DIR");
        let reports = Path::new(&reports).join("determinism").join(name);
        std::fs::create_dir_all(&reports).unwrap();
        // macOS limits Unix socket paths to 104 bytes.
        let directory = tempfile::Builder::new()
            .prefix("seedbox-oracles-")
            .tempdir_in("/tmp")
            .unwrap();
        Self {
            cli: cli.into(),
            live: directory.path().to_owned(),
            directory,
            reports,
            lines: Vec::new(),
            failures: Vec::new(),
        }
    }

    fn state(&self, name: &str) -> PathBuf {
        self.directory.path().join(name)
    }

    fn server(&mut self, state: &Path) -> Result<Server, String> {
        self.live = state.to_owned();
        Server::start(&self.cli, state)
    }

    fn session(&self, state: &Path, workers: Option<u32>) -> Session {
        Session::new(&self.cli, state, workers)
    }

    /// Run one oracle, recording its verdict and wall time.
    async fn oracle<T>(
        &mut self,
        name: &str,
        check: impl Future<Output = Result<T, String>>,
    ) -> Option<T> {
        let started = Instant::now();
        let result = check.await;
        let line = format!(
            "determinism: {name}: {} in {:.1}s",
            if result.is_ok() { "PASS" } else { "FAIL" },
            started.elapsed().as_secs_f64()
        );
        eprintln!("{line}");
        self.lines.push(line);
        match result {
            Ok(value) => Some(value),
            Err(error) => {
                eprintln!("determinism: {name}: {error}");
                self.capture(name, &error);
                self.failures.push(format!("{name}: {error}"));
                None
            }
        }
    }

    /// Record a failed deletion of an oracle's simulations.
    async fn release(&mut self, release: impl Future<Output = Outcome>) {
        if let Err(error) = release.await {
            eprintln!("determinism: cleanup: {error}");
            self.failures.push(format!("cleanup: {error}"));
        }
    }

    /// While the server still runs (it removes its runs on shutdown), save
    /// the definition, world, timeline, events and output tail of every
    /// simulation a failure names.
    fn capture(&self, oracle: &str, error: &str) {
        let mut seen = std::collections::BTreeSet::new();
        let ids = error
            .split(|c: char| !c.is_ascii_alphanumeric())
            .filter_map(|word| word.parse::<SimulationId>().ok())
            .filter(|id| seen.insert(id.clone()));
        let directory = self
            .reports
            .join(oracle.replace(|c: char| !c.is_ascii_alphanumeric(), "-"));
        for id in ids {
            for (kind, args) in [
                ("status", vec!["show", id.as_str(), "--json"]),
                (
                    "definition",
                    vec!["show", "--view", "definition", id.as_str(), "--json"],
                ),
                (
                    "world",
                    vec!["show", "--view", "world", id.as_str(), "--json"],
                ),
                (
                    "timeline",
                    vec!["show", "--view", "timeline", id.as_str(), "--json"],
                ),
                ("events", vec!["events", id.as_str()]),
                ("logs", vec!["logs", id.as_str(), "--tail", "200"]),
            ] {
                let output = Command::new(&self.cli)
                    .args(&args)
                    .env_remove("SEEDBOX_VMM")
                    .env("SEEDBOX_STATE", &self.live)
                    .guarded_output();
                let text = match output {
                    Ok(output) => [output.stdout, output.stderr].concat(),
                    Err(error) => error.to_string().into_bytes(),
                };
                let _ = std::fs::create_dir_all(&directory);
                let _ = std::fs::write(directory.join(format!("{id}-{kind}.txt")), text);
            }
        }
    }

    /// Write the summary and panic on failure; evidence is in the report
    /// directory, and the server state (only its cache survives shutdown)
    /// is removed.
    fn finish(self) {
        let mut summary = self.lines.join("\n");
        summary.push('\n');
        for failure in &self.failures {
            summary.push_str(&format!("\nFAIL {failure}\n"));
        }
        if self.failures.is_empty() {
            std::fs::write(self.reports.join("summary.txt"), summary).unwrap();
            return;
        }
        summary.push_str(&format!("\nevidence: {}\n", self.reports.display()));
        std::fs::write(self.reports.join("summary.txt"), &summary).unwrap();
        panic!("{summary}");
    }
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

#[test]
fn whole_run_determinism_oracles() {
    let mut run = Run::new("whole-run");
    let state = run.state("state");
    let mut server = Some(run.server(&state).unwrap());
    runtime().block_on(async {
        let mut session = run.session(&state, Some(3));
        let Some(baseline) = run.oracle("baseline", session.baseline()).await else {
            return;
        };
        eprintln!("determinism: baseline {baseline}");
        run.oracle("PerfSettingInvariance", session.perf_settings(&baseline))
            .await;
        run.release(session.release(&[&baseline])).await;
        run.oracle(
            "ConcurrentSimulationIsolation",
            session.concurrent_isolation(&baseline),
        )
        .await;
        run.release(session.release(&[&baseline])).await;
        run.oracle(
            "CheckpointTransparencyAndBranchIdentity",
            session.checkpoint_transparency(&baseline),
        )
        .await;
        run.release(session.release(&[&baseline])).await;
        run.oracle(
            "EditedTimelinePrefix",
            session.edited_timeline_prefix(&baseline),
        )
        .await;
        run.release(session.release(&[&baseline])).await;
        run.oracle("PreviewPurity", session.preview_purity(&baseline))
            .await;
        run.release(session.release(&[&baseline])).await;
        run.oracle(
            "AcceptedFaultSurvivesDisconnect",
            session.accepted_fault_survives_disconnect(&baseline),
        )
        .await;
        run.release(session.release(&[&baseline])).await;
        // Cross-session oracles: export from this session, then shut it down.
        // Checkpoint file paths are resolved through macOS's /tmp symlink.
        let root = run.directory.path().canonicalize().unwrap();
        let Some(carried) = run
            .oracle("cross-session export", session.carry(&root))
            .await
        else {
            return;
        };
        drop(server.take());
        // Only the carried output and checkpoint file remain; free the first session's disk.
        if let Err(error) = std::fs::remove_dir_all(&state) {
            run.failures
                .push(format!("remove first session state: {error}"));
        }
        let second = run.state("state2");
        server = match run.server(&second) {
            Ok(server) => Some(server),
            Err(error) => {
                run.failures.push(format!("second session: {error}"));
                return;
            }
        };
        let fresh = run.session(&second, None);
        run.oracle("TwinFreshRunsCrossSession", fresh.twin_fresh_runs(&carried))
            .await;
        run.release(fresh.release(&[])).await;
        run.oracle(
            "ImportedCheckpointExecutes",
            fresh.imported_checkpoint(&carried),
        )
        .await;
    });
    drop(server);
    run.finish();
}

/// Machine worker count is a submission performance option. References run
/// on one thread; reruns with 2, 3 and 8 workers (more than the three
/// machines) must reproduce every one of them exactly.
#[test]
fn machine_worker_count_never_changes_results() {
    let mut run = Run::new("machine-workers");
    let state = run.state("state");
    let server = run.server(&state).unwrap();
    runtime().block_on(async {
        let mut session = run.session(&state, Some(1));
        let Some(baseline) = run.oracle("baseline", session.baseline()).await else {
            return;
        };
        let Some(references) = run
            .oracle("references", session.worker_references(&baseline))
            .await
        else {
            return;
        };
        for workers in [2, 3, 8] {
            run.oracle(
                &format!("MachineWorkerInvariance({workers} workers)"),
                session.worker_invariance(workers, &references),
            )
            .await;
            run.release(session.release(&references.ids())).await;
        }
    });
    drop(server);
    run.finish();
}

/// Full-state evidence of machines that shut down (the oracle, and every
/// machine when it exits) and of a crashed machine must be collected, so two
/// runs of one definition compare with full state.
#[test]
fn full_state_evidence_of_stopped_machines() {
    let mut run = Run::new("full-state");
    let state = run.state("state");
    let server = run.server(&state).unwrap();
    runtime().block_on(async {
        let mut session = run.session(&state, Some(1));
        let Some(baseline) = run.oracle("baseline", session.baseline()).await else {
            return;
        };
        run.oracle("rerun with full state", async {
            let rerun = session.submit(&session.document, false).await?;
            session.complete(&rerun).await?;
            session.same_results(&baseline, &rerun).await
        })
        .await;
        run.oracle("crash and reboot with full state", async {
            let lifecycle = workers::lifecycle_document(&session.document)?;
            let first = session.submit(&lifecycle, false).await?;
            session.complete(&first).await?;
            let second = session.submit(&lifecycle, false).await?;
            session.complete(&second).await?;
            session.same_results(&first, &second).await
        })
        .await;
    });
    drop(server);
    run.finish();
}
