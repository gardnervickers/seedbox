//! Contract tests use a test-only backend. No hardware qualification is implied.
use seedbox_application::{
    config::{self, Simulation},
    execution::*,
    image::Launch,
    orchestration::*,
    store::{ContentDigest, ContentStore},
    Error, Result,
};
use std::{
    collections::BTreeMap,
    fs,
    sync::atomic::{AtomicUsize, Ordering},
};

struct Fixture {
    directory: tempfile::TempDir,
    simulation: Simulation,
    orchestrator: Orchestrator,
    preparer: Preparer,
    backend: Backend,
}

struct Preparer {
    inputs: PreparedInputs,
    calls: AtomicUsize,
}
impl PreparationBackend for Preparer {
    fn prepare(&self, _: &Simulation, _: &ContentStore) -> Result<PreparedInputs> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(self.inputs.clone())
    }
}

struct Backend {
    profile: ExecutionProfile,
    performance: BTreeMap<String, String>,
    extra_logs: BTreeMap<String, Vec<u8>>,
    report: ExecutionReport,
    calls: AtomicUsize,
    fail: bool,
    cancel: bool,
}
impl ExecutionBackend for Backend {
    fn profile(&self) -> ExecutionProfile {
        self.profile.clone()
    }
    fn performance_parameters(&self) -> BTreeMap<String, String> {
        self.performance.clone()
    }
    fn execute(&self, request: ExecutionRequest<'_>) -> Result<ExecutionReport> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        for (digest, path) in request.artifacts {
            assert_eq!(ContentDigest::of(&fs::read(path)?), *digest);
        }
        assert_eq!(request.prepared.profile, self.profile);
        assert_eq!(request.performance_parameters, &self.performance);
        fs::write(
            request.output.join("workload.console"),
            format!(
                "seed={} timer_jitter={} stop_tolerance={}\n",
                request.seed, request.schedule.timer_jitter_ns, request.schedule.stop_tolerance_ns
            ),
        )?;
        for (name, bytes) in &self.extra_logs {
            let path = request.output.join(name);
            fs::create_dir_all(path.parent().unwrap())?;
            fs::write(path, bytes)?;
        }
        if self.cancel {
            request.cancellation.cancel();
        }
        if self.fail {
            return Err(Error::Execution("injected backend failure".into()));
        }
        Ok(self.report.clone())
    }
}

/// The fixture's backend with only `execute` replaced, so profile and
/// performance parameters stay those of the fixture.
struct Executing<'a, F> {
    backend: &'a Backend,
    execute: F,
}
impl<F> ExecutionBackend for Executing<'_, F>
where
    F: Fn(ExecutionRequest<'_>) -> Result<ExecutionReport> + Send + Sync,
{
    fn profile(&self) -> ExecutionProfile {
        self.backend.profile()
    }
    fn performance_parameters(&self) -> BTreeMap<String, String> {
        self.backend.performance_parameters()
    }
    fn execute(&self, request: ExecutionRequest<'_>) -> Result<ExecutionReport> {
        (self.execute)(request)
    }
}

impl Fixture {
    fn executing<F>(&self, execute: F) -> Executing<'_, F>
    where
        F: Fn(ExecutionRequest<'_>) -> Result<ExecutionReport> + Send + Sync,
    {
        Executing {
            backend: &self.backend,
            execute,
        }
    }
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("seedbox.yaml");
        fs::write(
            &source,
            "name: test\nmachines: {}\noracle: {image: example/workload:v1}\n",
        )
        .unwrap();
        let mut simulation = config::load(&source, &BTreeMap::new()).unwrap();
        simulation.seed = Some(1009);
        simulation.limits.simulated_time_ns = 10_000_000_000;
        simulation.limits.wall_time_ns = 10_000_000_000;
        let store = ContentStore::new(directory.path().join("store"));
        let kernel = store.put(b"fixture kernel").unwrap();
        let engine = store.put(b"fixture engine").unwrap();
        let guest = store.put(b"fixture guest").unwrap();
        let manifest = store.put(b"fixture manifest").unwrap();
        let config = store.put(b"fixture image config").unwrap();
        let layer = store.put(b"fixture layer").unwrap();
        let profile = ExecutionProfile {
            backend: "test-only".into(),
            witness_mode: WitnessMode::Exits,
            semantic_parameters: SemanticParameters::new(),
        };
        let inputs = PreparedInputs {
            machine_memory_mb: BTreeMap::from([("workload".into(), 512)]),
            wall_clock_epoch: None,
            profile: profile.clone(),
            placement: BTreeMap::from([("oracle".into(), "oracle".into())]),
            guests: BTreeMap::from([("oracle".into(), guest)]),
            images: BTreeMap::from([(
                "oracle".into(),
                PreparedImage {
                    manifest,
                    config,
                    layers: vec![layer],
                    launch: Launch {
                        argv: vec!["/app".into()],
                        environment: BTreeMap::new(),
                        user: "".into(),
                        working_dir: "/".into(),
                        volumes: vec![],
                        disk_volumes: vec![],
                    },
                },
            )]),
            auxiliary: BTreeMap::from([
                ("driver-world/executable".into(), engine),
                ("fixture/kernel".into(), kernel),
            ]),
            provenance: BTreeMap::from([("builder".into(), "fixture-v1".into())]),
        };
        let backend = Backend {
            profile,
            performance: BTreeMap::from([("SEEDBOX_WATCHDOG_MS".into(), "20".into())]),
            extra_logs: BTreeMap::new(),
            report: ExecutionReport {
                host_wait_ns: 0,
                simulated_time_ns: None,
                reason: StopReason::Completed,
                workload_exit: Some(WorkloadExit {
                    service: "oracle".into(),
                    code: 0,
                }),
                witness: Some(ContentDigest::of(b"witness")),
                full_state: None,
                faults: vec![],
                detail: None,
            },
            calls: AtomicUsize::new(0),
            fail: false,
            cancel: false,
        };
        let orchestrator = Orchestrator::new(store, directory.path().join("runs"));
        Self {
            directory,
            simulation,
            orchestrator,
            preparer: Preparer {
                inputs,
                calls: AtomicUsize::new(0),
            },
            backend,
        }
    }
    fn run(&self) -> Run {
        self.orchestrator
            .run(
                &self.simulation,
                RunOptions::default(),
                &self.preparer,
                &self.backend,
            )
            .unwrap()
    }
    fn replay(&self, run: &Run) -> Replayed {
        replay_retained(&self.orchestrator, &run.directory, &self.backend).unwrap()
    }
}

/// A fresh execution of a retained identity and its comparison with the
/// baseline, as the server replays: no preparation, only retained inputs.
struct Replayed {
    directory: std::path::PathBuf,
    record: RunRecord,
    comparison: SimulationComparison,
}

fn replay_retained(
    orchestrator: &Orchestrator,
    directory: &std::path::Path,
    executor: &dyn ExecutionBackend,
) -> Result<Replayed> {
    let baseline = inspect(directory)?;
    let identity = &baseline.identity;
    let fresh = orchestrator
        .clone()
        .with_program(identity.program.clone())
        .with_interventions(identity.interventions.clone())
        .run_prepared(
            &identity.configuration,
            RunOptions {
                seed: Some(identity.seed),
                ..Default::default()
            },
            identity.inputs.clone(),
            executor,
        )?;
    Ok(Replayed {
        // Server identities are irrelevant to retained evidence comparison.
        comparison: compare(
            (&"k7m2p9qx".parse().unwrap(), &baseline),
            (&"k7m2p9qy".parse().unwrap(), &fresh.record),
        ),
        directory: fresh.directory,
        record: fresh.record,
    })
}

#[test]
fn run_retains_inputs_result_logs_and_replay_does_not_prepare_again() {
    for epoch in [None, Some(0), Some(1791500000)] {
        let mut fixture = Fixture::new();
        fixture.preparer.inputs.wall_clock_epoch = epoch;
        let first = fixture.run();
        assert_eq!(first.record.identity.inputs.wall_clock_epoch, epoch);
        assert_eq!(first.record.outcome, Outcome::Pass);
        assert_eq!(inspect(&first.directory).unwrap(), first.record);
        assert_eq!(first.record.logs.len(), 1);
        fs::remove_file(&fixture.simulation.source).unwrap();
        let fresh = fixture.replay(&first);
        assert_eq!(fresh.record.run_id, first.record.run_id);
        assert_ne!(fresh.directory, first.directory);
        assert_eq!(fresh.comparison.status, ComparisonStatus::Match);
        assert_eq!(fixture.preparer.calls.load(Ordering::SeqCst), 1);
        assert_eq!(fixture.backend.calls.load(Ordering::SeqCst), 2);
    }
}

#[test]
fn reproduced_application_failure_is_separate_from_replay_mismatch() {
    let mut fixture = Fixture::new();
    fixture.backend.report.workload_exit.as_mut().unwrap().code = 3;
    let failed = fixture.run();
    let reproduced = fixture.replay(&failed);
    assert_eq!(reproduced.record.outcome, Outcome::Fail);
    assert_eq!(reproduced.comparison.status, ComparisonStatus::Match);
    // Two different failing exit codes must not be treated as equal outcomes.
    fixture.backend.report.workload_exit.as_mut().unwrap().code = 4;
    let different_failure = fixture.replay(&failed);
    assert_eq!(different_failure.record.outcome, Outcome::Fail);
    assert_eq!(
        different_failure.comparison.status,
        ComparisonStatus::Mismatch
    );
}

#[test]
fn witness_fault_history_and_full_state_comparisons_are_distinct() {
    let mut fixture = Fixture::new();
    let first = fixture.run();
    fixture.backend.report.witness = Some(ContentDigest::of(b"diverged"));
    let replay = fixture.replay(&first).comparison;
    assert_eq!(replay.status, ComparisonStatus::Mismatch);
    assert_eq!(replay.witness_matches, Some(false));
    assert_eq!(replay.full_state_matches, None);
    fixture.backend.report.witness = first.record.report.as_ref().unwrap().witness.clone();
    fixture.backend.report.full_state = Some(BTreeMap::from([(
        "oracle".into(),
        ContentDigest::of(b"state"),
    )]));
    let with_state = fixture.run();
    let matched = fixture.replay(&with_state).comparison;
    assert_eq!(matched.full_state_matches, Some(true));
    assert_eq!(matched.status, ComparisonStatus::Match);
    fixture.backend.report.full_state = None;
    assert_eq!(
        fixture.replay(&with_state).comparison.status,
        ComparisonStatus::Mismatch
    );
    fixture.backend.report.faults.push(FaultEvent {
        requested_at_ns: 1,
        applied_at_ns: 2,
        service: "oracle".into(),
        action: "restart".into(),
        detail: "test".into(),
    });
    let replay = fixture.replay(&first).comparison;
    assert_eq!(replay.witness_matches, Some(true));
    assert!(!replay.fault_history_matches);
    assert_eq!(replay.status, ComparisonStatus::Mismatch);
}

#[test]
fn requested_full_state_requires_a_digest_for_every_machine() {
    let mut fixture = Fixture::new();
    fixture.backend.profile.witness_mode = WitnessMode::FullState;
    fixture.preparer.inputs.profile = fixture.backend.profile.clone();
    assert_eq!(fixture.run().record.outcome, Outcome::Error);
    fixture.backend.report.full_state = Some(BTreeMap::new());
    assert_eq!(fixture.run().record.outcome, Outcome::Error);
    fixture.backend.report.full_state = Some(BTreeMap::from([(
        "oracle".into(),
        ContentDigest::of(b"state"),
    )]));
    assert_eq!(fixture.run().record.outcome, Outcome::Pass);
}

#[test]
fn limits_cancellation_and_missing_completion_cannot_pass() {
    let mut fixture = Fixture::new();
    for (reason, expected) in [
        (StopReason::SimulatedTimeLimit, Outcome::Timeout),
        (StopReason::WallTimeLimit, Outcome::Incomplete),
        (StopReason::Cancelled, Outcome::Incomplete),
        (StopReason::Quiescent, Outcome::Incomplete),
        (StopReason::Error, Outcome::Error),
    ] {
        fixture.backend.report.reason = reason;
        let run = fixture.run();
        assert_eq!(run.record.outcome, expected);
        assert_eq!(
            fixture.replay(&run).comparison.status,
            if expected == Outcome::Timeout {
                ComparisonStatus::Match
            } else {
                ComparisonStatus::NotComparable
            }
        );
    }
    fixture.backend.report.reason = StopReason::Completed;
    fixture.backend.report.workload_exit = None;
    assert_eq!(fixture.run().record.outcome, Outcome::Incomplete);
    fixture.backend.report.workload_exit = Some(WorkloadExit {
        service: "wrong-service".into(),
        code: 0,
    });
    assert_eq!(fixture.run().record.outcome, Outcome::Error);
    fixture
        .backend
        .report
        .workload_exit
        .as_mut()
        .unwrap()
        .service = "oracle".into();
    fixture.backend.report.witness = None;
    assert_eq!(fixture.run().record.outcome, Outcome::Error);
}

#[test]
fn unexpected_backend_failure_and_cancellation_retain_logs() {
    let mut fixture = Fixture::new();
    fixture.backend.fail = true;
    let run = fixture.run();
    assert_eq!(run.record.outcome, Outcome::Error);
    assert!(run
        .record
        .error
        .unwrap()
        .contains("injected backend failure"));
    assert_eq!(run.record.logs.len(), 1);
    fixture.backend.fail = false;
    fixture.backend.cancel = true;
    assert_eq!(fixture.run().record.outcome, Outcome::Incomplete);
    let token = CancellationToken::default();
    token.cancel();
    let calls_before = fixture.backend.calls.load(Ordering::SeqCst);
    let cancelled = fixture
        .orchestrator
        .run(
            &fixture.simulation,
            RunOptions {
                seed: None,
                cancellation: token,
                wall_time_ns: None,
                machine_workers: None,
            },
            &fixture.preparer,
            &fixture.backend,
        )
        .unwrap();
    assert_eq!(cancelled.record.outcome, Outcome::Incomplete);
    assert_eq!(fixture.backend.calls.load(Ordering::SeqCst), calls_before);
}

#[test]
fn backend_cannot_report_success_after_wall_deadline() {
    let fixture = Fixture::new();
    let run = fixture
        .orchestrator
        .run(
            &fixture.simulation,
            RunOptions {
                wall_time_ns: Some(1),
                ..Default::default()
            },
            &fixture.preparer,
            &fixture.backend,
        )
        .unwrap();
    assert_eq!(run.record.outcome, Outcome::Incomplete);
}

#[test]
fn runtime_timeout_does_not_change_the_experiment_identity() {
    let fixture = Fixture::new();
    let first = fixture.run();
    let second = fixture
        .orchestrator
        .run(
            &fixture.simulation,
            RunOptions {
                wall_time_ns: Some(60_000_000_000),
                ..Default::default()
            },
            &fixture.preparer,
            &fixture.backend,
        )
        .unwrap();
    assert_eq!(first.record.run_id, second.record.run_id);
    assert_eq!(first.record.identity, second.record.identity);
}

#[test]
fn reported_playback_wait_is_internal_to_the_execution_report() {
    let fixture = Fixture::new();
    let executor = fixture.executing(|_| {
        let mut report = fixture.backend.report.clone();
        report.host_wait_ns = 150_000_000;
        Ok(report)
    });
    let run = fixture
        .orchestrator
        .run_prepared(
            &fixture.simulation,
            // Exact deadline exclusion is proved with injected time in the
            // production-used private decision helper; this exercises retained
            // outcome/JSON composition without a host-speed threshold.
            RunOptions::default(),
            fixture.preparer.inputs.clone(),
            &executor,
        )
        .unwrap();
    assert_eq!(run.record.outcome, Outcome::Pass);
    assert!(!serde_json::to_value(run.record.report)
        .unwrap()
        .as_object()
        .unwrap()
        .contains_key("host_wait_ns"));
}

#[test]
fn unexpected_panic_leaves_an_inspectable_incomplete_attempt() {
    struct PanickingBackend(ExecutionProfile);
    impl ExecutionBackend for PanickingBackend {
        fn profile(&self) -> ExecutionProfile {
            self.0.clone()
        }
        fn execute(&self, _: ExecutionRequest<'_>) -> Result<ExecutionReport> {
            panic!("injected interruption")
        }
    }
    let fixture = Fixture::new();
    let backend = PanickingBackend(fixture.backend.profile.clone());
    assert!(std::panic::catch_unwind(|| fixture.orchestrator.run(
        &fixture.simulation,
        RunOptions::default(),
        &fixture.preparer,
        &backend
    ))
    .is_err());
    let attempts: Vec<_> = fs::read_dir(fixture.directory.path().join("runs"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    assert_eq!(attempts.len(), 1);
    assert_eq!(inspect(&attempts[0]).unwrap().outcome, Outcome::Incomplete);
}

#[test]
fn corruption_missing_artifacts_and_profile_drift_prevent_execution() {
    let fixture = Fixture::new();
    let first = fixture.run();
    let digest = &fixture.preparer.inputs.images["oracle"].layers[0];
    let path = fixture
        .directory
        .path()
        .join("store/sha256")
        .join(digest.hex());
    fs::write(&path, b"corrupt").unwrap();
    let replay = fixture.replay(&first);
    assert_eq!(replay.record.outcome, Outcome::Error);
    assert!(replay.record.error.unwrap().contains("hash mismatch"));
    assert_eq!(fixture.backend.calls.load(Ordering::SeqCst), 1);
    fs::remove_file(path).unwrap();
    assert_eq!(fixture.replay(&first).record.outcome, Outcome::Error);
    assert_eq!(fixture.backend.calls.load(Ordering::SeqCst), 1);

    let mut fixture = Fixture::new();
    let first = fixture.run();
    fixture.backend.profile.semantic_parameters.torn_writes = true;
    assert_eq!(fixture.replay(&first).record.outcome, Outcome::Error);
    assert_eq!(fixture.backend.calls.load(Ordering::SeqCst), 1);
}

#[test]
fn manifest_corruption_is_rejected_before_a_fresh_attempt() {
    let fixture = Fixture::new();
    let first = fixture.run();
    let path = first.directory.join("manifest.json");
    let mut manifest: serde_json::Value =
        serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    manifest["record"]["outcome"] = "FAIL".into();
    fs::write(path, serde_json::to_vec(&manifest).unwrap()).unwrap();
    assert!(inspect(&first.directory).is_err());
    assert!(replay_retained(&fixture.orchestrator, &first.directory, &fixture.backend).is_err());
    assert_eq!(fixture.backend.calls.load(Ordering::SeqCst), 1);
}

#[test]
fn seed_kernel_build_and_configuration_changes_create_new_identities() {
    let mut fixture = Fixture::new();
    let first = fixture.run();
    let overridden = fixture
        .orchestrator
        .run(
            &fixture.simulation,
            RunOptions {
                seed: Some(0),
                ..Default::default()
            },
            &fixture.preparer,
            &fixture.backend,
        )
        .unwrap();
    assert_eq!(overridden.record.identity.seed, 0);
    assert_ne!(first.record.run_id, overridden.record.run_id);
    let store = ContentStore::new(fixture.directory.path().join("store"));
    fixture
        .preparer
        .inputs
        .auxiliary
        .insert("fixture/kernel".into(), store.put(b"new kernel").unwrap());
    let new_kernel = fixture.run();
    assert_ne!(first.record.run_id, new_kernel.record.run_id);
    fixture
        .preparer
        .inputs
        .provenance
        .insert("builder".into(), "fixture-v2".into());
    let new_build = fixture.run();
    assert_ne!(new_kernel.record.run_id, new_build.record.run_id);
    fixture
        .simulation
        .services
        .get_mut("oracle")
        .unwrap()
        .environment
        .insert("VARIABLE".into(), Some("new".into()));
    assert_ne!(new_build.record.run_id, fixture.run().record.run_id);
    fixture.simulation.seed = None;
    let count = fixture.preparer.calls.load(Ordering::SeqCst);
    assert!(fixture
        .orchestrator
        .run(
            &fixture.simulation,
            RunOptions::default(),
            &fixture.preparer,
            &fixture.backend
        )
        .is_err());
    assert_eq!(fixture.preparer.calls.load(Ordering::SeqCst), count);
}

#[test]
fn concurrent_runs_share_content_but_never_output_directories() {
    let fixture = Fixture::new();
    std::thread::scope(|scope| {
        let handles: Vec<_> = (0..4).map(|_| scope.spawn(|| fixture.run())).collect();
        let runs: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        for (i, run) in runs.iter().enumerate() {
            assert_eq!(run.record.outcome, Outcome::Pass);
            assert_eq!(run.record.run_id, runs[0].record.run_id);
            assert_eq!(inspect(&run.directory).unwrap(), run.record);
            for other in &runs[..i] {
                assert_ne!(run.directory, other.directory);
            }
        }
    });
}

#[test]
fn wall_limit_is_infrastructure_but_simulated_limit_can_reproduce() {
    let mut fixture = Fixture::new();
    let baseline = fixture.run();
    fixture.backend.report.reason = StopReason::WallTimeLimit;
    let replay = fixture.replay(&baseline);
    assert_eq!(replay.record.outcome, Outcome::Incomplete);
    assert_eq!(replay.comparison.status, ComparisonStatus::NotComparable);
    fixture.backend.report.reason = StopReason::SimulatedTimeLimit;
    fixture.backend.report.workload_exit = None;
    let timed_out = fixture.run();
    assert_eq!(timed_out.record.outcome, Outcome::Timeout);
    assert_eq!(
        fixture.replay(&timed_out).comparison.status,
        ComparisonStatus::Match
    );
    fixture.backend.report.witness = None;
    assert_eq!(
        fixture.replay(&timed_out).comparison.status,
        ComparisonStatus::NotComparable
    );
}

#[test]
fn co_located_services_use_one_machine_state_digest_and_complete_placement() {
    let mut fixture = Fixture::new();
    fixture.simulation.services.insert(
        "redis".into(),
        fixture.simulation.services["oracle"].clone(),
    );
    fixture.preparer.inputs.images.insert(
        "redis".into(),
        fixture.preparer.inputs.images["oracle"].clone(),
    );
    let guest = fixture.preparer.inputs.guests.remove("oracle").unwrap();
    fixture.preparer.inputs.guests.insert("vm0".into(), guest);
    fixture.preparer.inputs.placement = BTreeMap::from([
        ("oracle".into(), "vm0".into()),
        ("redis".into(), "vm0".into()),
    ]);
    fixture.backend.profile.witness_mode = WitnessMode::FullState;
    fixture.preparer.inputs.profile = fixture.backend.profile.clone();
    fixture.backend.report.full_state = Some(BTreeMap::from([(
        "vm0".into(),
        ContentDigest::of(b"vm state"),
    )]));
    let run = fixture.run();
    assert_eq!(run.record.outcome, Outcome::Pass);
    assert_eq!(
        fixture.replay(&run).comparison.status,
        ComparisonStatus::Match
    );
    fixture.preparer.inputs.placement.remove("redis");
    let calls = fixture.backend.calls.load(Ordering::SeqCst);
    assert_eq!(fixture.run().record.outcome, Outcome::Error);
    assert_eq!(fixture.backend.calls.load(Ordering::SeqCst), calls);
}

#[test]
fn project_workers_change_capacity_without_changing_identity_and_quantum_is_retained() {
    let mut fixture = Fixture::new();
    fixture.simulation.simulator.quantum_ns = std::num::NonZeroU64::new(2_500_000).unwrap();
    fixture
        .backend
        .performance
        .insert("SEEDBOX_MACHINE_WORKERS".into(), "1".into());
    let first = fixture.run();
    fixture.simulation.simulator.machine_workers = 4.try_into().unwrap();
    fixture
        .backend
        .performance
        .insert("SEEDBOX_MACHINE_WORKERS".into(), "4".into());
    let parallel = fixture.run();
    assert_eq!(first.record.run_id, parallel.record.run_id);
    assert_eq!(
        parallel.record.performance_parameters["SEEDBOX_MACHINE_WORKERS"],
        "4"
    );
    assert_eq!(
        parallel
            .record
            .identity
            .configuration
            .simulator
            .machine_workers
            .get(),
        1
    );
    let executor = fixture.executing(|request| {
        assert_eq!(request.quantum_ns.get(), 2_500_000);
        Ok(fixture.backend.report.clone())
    });
    let replay = replay_retained(&fixture.orchestrator, &parallel.directory, &executor).unwrap();
    assert_eq!(replay.comparison.status, ComparisonStatus::Match);
    fixture.simulation.simulator.quantum_ns = std::num::NonZeroU64::new(5_000_000).unwrap();
    assert_ne!(fixture.run().record.run_id, first.record.run_id);
}

#[test]
fn performance_changes_are_recorded_without_changing_identity_or_bypassing_comparison() {
    let mut fixture = Fixture::new();
    let first = fixture.run();
    fixture
        .backend
        .performance
        .insert("SEEDBOX_WATCHDOG_MS".into(), "200".into());
    let replay = fixture.replay(&first);
    assert_eq!(replay.record.run_id, first.record.run_id);
    assert_ne!(
        replay.record.performance_parameters,
        first.record.performance_parameters
    );
    assert_eq!(replay.comparison.status, ComparisonStatus::Match);
    fixture.backend.report.witness = Some(ContentDigest::of(b"bad performance optimization"));
    assert_eq!(
        fixture.replay(&first).comparison.status,
        ComparisonStatus::Mismatch
    );
}

#[test]
fn fault_comparison_uses_acknowledged_effect_time_and_rejects_invalid_acknowledgements() {
    let mut fixture = Fixture::new();
    fixture.backend.report.faults.push(FaultEvent {
        requested_at_ns: 10,
        applied_at_ns: 20,
        service: "oracle".into(),
        action: "stop".into(),
        detail: "guest acknowledged".into(),
    });
    let run = fixture.run();
    fixture.backend.report.faults[0].applied_at_ns = 21;
    assert_eq!(
        fixture.replay(&run).comparison.status,
        ComparisonStatus::Mismatch
    );
    fixture.backend.report.faults[0].applied_at_ns = 9;
    assert_eq!(fixture.run().record.outcome, Outcome::Error);
}

#[test]
fn schedule_controls_reach_executor_and_change_identity() {
    let mut fixture = Fixture::new();
    let first = fixture.run();
    fixture.simulation.schedule.timer_jitter_ns = 123;
    fixture.simulation.schedule.stop_tolerance_ns = 456;
    let second = fixture.run();
    assert_ne!(first.record.run_id, second.record.run_id);
    assert!(
        fs::read_to_string(second.directory.join("output/workload.console"))
            .unwrap()
            .contains("timer_jitter=123 stop_tolerance=456")
    );
}

#[test]
fn oversized_logs_are_explicitly_omitted_from_store_and_preserved_in_attempt() {
    let mut fixture = Fixture::new();
    fixture.orchestrator = fixture.orchestrator.with_log_policy(LogPolicy {
        max_file_bytes: 1,
        max_total_bytes: 100,
    });
    let run = fixture.run();
    assert_eq!(run.record.outcome, Outcome::Pass);
    assert!(run.record.logs.is_empty());
    assert!(run.record.omitted_logs.contains_key("workload.console"));
    assert!(run.directory.join("output/workload.console").is_file());
    fixture.orchestrator = fixture.orchestrator.with_log_policy(LogPolicy {
        max_file_bytes: 100,
        max_total_bytes: 1,
    });
    assert!(fixture.run().record.logs.is_empty());
    fixture.backend.extra_logs = BTreeMap::from([
        ("a.trace".into(), vec![0; 40]),
        ("b.trace".into(), vec![0; 40]),
    ]);
    fixture.orchestrator = fixture.orchestrator.with_log_policy(LogPolicy {
        max_file_bytes: 100,
        max_total_bytes: 70,
    });
    let limited = fixture.run();
    assert!(limited.record.logs.contains_key("a.trace"));
    assert!(limited.record.omitted_logs.contains_key("b.trace"));
}

#[test]
fn early_termination_preserves_reason_and_specific_fault_error() {
    let mut fixture = Fixture::new();
    for (reason, outcome) in [
        (StopReason::Cancelled, Outcome::Incomplete),
        (StopReason::WallTimeLimit, Outcome::Incomplete),
        (StopReason::SimulatedTimeLimit, Outcome::Timeout),
    ] {
        fixture.backend.report.reason = reason;
        assert_eq!(fixture.run().record.outcome, outcome);
    }
    fixture.backend.report.reason = StopReason::Error;
    fixture.backend.report.detail = Some("guest rejected fault 1".into());
    for cancel in [false, true] {
        fixture.backend.cancel = cancel;
        let run = fixture.run();
        assert_eq!(run.record.outcome, Outcome::Error);
        assert!(run.record.error.unwrap().contains("guest rejected fault 1"));
        assert_eq!(run.record.report.unwrap().workload_exit.unwrap().code, 0);
    }
}

#[test]
fn network_effects_have_no_single_service_target() {
    let mut fixture = Fixture::new();
    fixture.backend.report.faults.push(FaultEvent {
        requested_at_ns: 10,
        applied_at_ns: 10,
        service: String::new(),
        action: "partition".into(),
        detail: "partition applied".into(),
    });
    assert_eq!(fixture.run().record.outcome, Outcome::Pass);
    for action in ["shape", "drop_connection", "heal"] {
        fixture.backend.report.faults[0].action = action.into();
        assert_eq!(fixture.run().record.outcome, Outcome::Pass, "{action}");
    }
    fixture.backend.report.faults[0].action = "stop".into();
    assert_eq!(fixture.run().record.outcome, Outcome::Error);
}

#[test]
fn machine_lifecycle_effects_pass_and_replay_with_their_service() {
    let mut fixture = Fixture::new();
    for (i, action) in ["pause", "resume", "crash", "reboot"].iter().enumerate() {
        fixture.backend.report.faults.push(FaultEvent {
            requested_at_ns: i as u64 + 1,
            applied_at_ns: i as u64 + 1,
            service: "oracle".into(),
            action: (*action).into(),
            detail: "machine lifecycle".into(),
        });
    }
    let run = fixture.run();
    assert_eq!(run.record.outcome, Outcome::Pass);
    assert_eq!(
        fixture.replay(&run).comparison.status,
        ComparisonStatus::Match
    );
    fixture.backend.report.faults[0].service.clear();
    assert_eq!(fixture.run().record.outcome, Outcome::Error);
}

struct AdmissionBackend<'a> {
    backend: &'a Backend,
    checks: AtomicUsize,
    fail_at: usize,
}
impl ExecutionBackend for AdmissionBackend<'_> {
    fn profile(&self) -> ExecutionProfile {
        self.backend.profile()
    }
    fn performance_parameters(&self) -> BTreeMap<String, String> {
        self.backend.performance_parameters()
    }
    fn preflight(&self, _: Option<&std::path::Path>) -> Result<()> {
        let call = self.checks.fetch_add(1, Ordering::SeqCst) + 1;
        if call == self.fail_at {
            return Err(Error::Setup("injected host drift/cleanup rejection".into()));
        }
        Ok(())
    }
    fn execute(&self, request: ExecutionRequest<'_>) -> Result<ExecutionReport> {
        self.backend.execute(request)
    }
}

#[test]
fn rejected_preflight_never_prepares_and_post_preparation_drift_never_executes() {
    for fail_at in [1, 2] {
        let fixture = Fixture::new();
        let backend = AdmissionBackend {
            backend: &fixture.backend,
            checks: AtomicUsize::new(0),
            fail_at,
        };
        let result = fixture.orchestrator.run(
            &fixture.simulation,
            RunOptions::default(),
            &fixture.preparer,
            &backend,
        );
        assert_eq!(fixture.backend.calls.load(Ordering::SeqCst), 0);
        if fail_at == 1 {
            assert!(result.unwrap_err().to_string().contains("host drift"));
            assert_eq!(fixture.preparer.calls.load(Ordering::SeqCst), 0);
        } else {
            let run = result.unwrap();
            assert_eq!(fixture.preparer.calls.load(Ordering::SeqCst), 1);
            assert_eq!(run.record.outcome, Outcome::Error);
            assert!(run.record.error.as_ref().unwrap().contains("host drift"));
            assert_eq!(inspect(&run.directory).unwrap(), run.record);
        }
    }
}

#[test]
fn torn_write_policy_changes_identity_and_replay_requires_the_recorded_policy() {
    let mut fixture = Fixture::new();
    let off = fixture.run();
    fixture.backend.profile.semantic_parameters.torn_writes = true;
    fixture.preparer.inputs.profile = fixture.backend.profile.clone();
    let on = fixture.run();
    assert_ne!(on.record.run_id, off.record.run_id);
    assert_eq!(
        fixture.replay(&on).comparison.status,
        ComparisonStatus::Match
    );
    assert_eq!(fixture.replay(&off).record.outcome, Outcome::Error);
    fixture.backend.profile.semantic_parameters.torn_writes = false;
    assert_eq!(fixture.replay(&on).record.outcome, Outcome::Error);
    assert_eq!(
        fixture.replay(&off).comparison.status,
        ComparisonStatus::Match
    );
}

#[test]
fn materialize_trusts_only_unchanged_objects_this_store_hashed() {
    use std::os::unix::fs::MetadataExt;
    use std::time::{Duration, UNIX_EPOCH};

    let directory = tempfile::tempdir().unwrap();
    let store = ContentStore::new(directory.path().join("store"));
    let digest = store.put(b"retained guest bytes").unwrap();
    let object = directory.path().join("store/sha256").join(digest.hex());
    // Hashed by put, so this copy skips the hash and keeps the bytes.
    let copy = store
        .materialize(&digest, &directory.path().join("a"))
        .unwrap();
    assert_eq!(fs::read(copy).unwrap(), b"retained guest bytes");
    // Keep the inode and length: trust must be invalidated by changed metadata,
    // not object replacement or a size mismatch. Explicitly choose a different
    // mtime instead of assuming a rewrite advances ctime after a host delay.
    let before = fs::metadata(&object).unwrap();
    fs::write(&object, b"retained guest byteX").unwrap();
    let modified = if before.modified().unwrap() == UNIX_EPOCH {
        UNIX_EPOCH + Duration::from_secs(86_400)
    } else {
        UNIX_EPOCH
    };
    fs::OpenOptions::new()
        .write(true)
        .open(&object)
        .unwrap()
        .set_modified(modified)
        .unwrap();
    let after = fs::metadata(&object).unwrap();
    assert_eq!(
        (after.dev(), after.ino(), after.len()),
        (before.dev(), before.ino(), before.len()),
    );
    assert_ne!(after.modified().unwrap(), before.modified().unwrap());
    eprintln!(
        "same-inode corruption precondition: length={} unchanged, explicitly chosen mtime changed",
        after.len()
    );
    let bad = directory.path().join("b");
    assert!(store.materialize(&digest, &bad).is_err());
    assert_eq!(fs::read_dir(bad).unwrap().count(), 0);
    // A new store (another process) never trusts an earlier verification.
    fs::write(&object, b"retained guest bytes").unwrap();
    let fresh = ContentStore::new(directory.path().join("store"));
    fs::write(&object, b"retained guest byteX").unwrap();
    assert!(fresh
        .materialize(&digest, &directory.path().join("c"))
        .is_err());
}

#[test]
fn only_the_backend_runtime_inputs_are_copied_into_an_attempt() {
    struct Selective<'a>(&'a Fixture);
    impl ExecutionBackend for Selective<'_> {
        fn profile(&self) -> ExecutionProfile {
            self.0.backend.profile()
        }
        fn performance_parameters(&self) -> BTreeMap<String, String> {
            self.0.backend.performance_parameters()
        }
        fn runtime_inputs<'a>(&self, inputs: &'a PreparedInputs) -> Vec<&'a ContentDigest> {
            vec![
                inputs.executable().unwrap(),
                &inputs.auxiliary["fixture/kernel"],
            ]
        }
        fn execute(&self, request: ExecutionRequest<'_>) -> Result<ExecutionReport> {
            let inputs = &self.0.preparer.inputs;
            assert_eq!(request.artifacts.keys().collect::<Vec<_>>(), {
                let mut keys = vec![
                    inputs.executable().unwrap(),
                    &inputs.auxiliary["fixture/kernel"],
                ];
                keys.sort();
                keys
            });
            self.0.backend.execute(request)
        }
    }
    let fixture = Fixture::new();
    let run = fixture
        .orchestrator
        .run(
            &fixture.simulation,
            RunOptions::default(),
            &fixture.preparer,
            &Selective(&fixture),
        )
        .unwrap();
    assert_eq!(run.record.outcome, Outcome::Pass);
    assert_eq!(
        fs::read_dir(run.directory.join("inputs")).unwrap().count(),
        2
    );
}

#[test]
fn put_file_of_a_retained_object_reads_it_without_rewriting_it() {
    use std::os::unix::fs::MetadataExt;
    use std::time::{Duration, UNIX_EPOCH};
    let directory = tempfile::tempdir().unwrap();
    let source = directory.path().join("engine");
    fs::write(&source, b"retained engine bytes").unwrap();
    let store = ContentStore::new(directory.path().join("store"));
    let digest = store.put_file(&source).unwrap();
    let object = directory.path().join("store/sha256").join(digest.hex());
    let before = fs::metadata(&object).unwrap();
    // Another process (a fresh store) verifies the existing object and keeps it.
    let fresh = ContentStore::new(directory.path().join("store"));
    assert_eq!(fresh.put_file(&source).unwrap(), digest);
    let after = fs::metadata(&object).unwrap();
    assert_eq!(
        (before.ino(), before.ctime_nsec()),
        (after.ino(), after.ctime_nsec())
    );
    // A corrupted existing object is never repaired or accepted.
    fs::write(&object, b"retained engine byteX").unwrap();
    // Make invalidation observable even if the writes share a timestamp tick.
    let modified = if after.modified().unwrap() == UNIX_EPOCH {
        UNIX_EPOCH + Duration::from_secs(86_400)
    } else {
        UNIX_EPOCH
    };
    fs::OpenOptions::new()
        .write(true)
        .open(&object)
        .unwrap()
        .set_modified(modified)
        .unwrap();
    assert_ne!(
        fs::metadata(&object).unwrap().modified().unwrap(),
        after.modified().unwrap()
    );
    assert!(fresh.put_file(&source).is_err());
    assert!(ContentStore::new(directory.path().join("store"))
        .put_file(&source)
        .is_err());
}

#[test]
fn a_runtime_input_is_hashed_again_after_it_changes() {
    let directory = tempfile::tempdir().unwrap();
    let source = directory.path().join("engine");
    fs::write(&source, b"first engine").unwrap();
    let first = seedbox_driver_execution::admission::file_digest(&source).unwrap();
    assert_eq!(
        seedbox_driver_execution::admission::file_digest(&source).unwrap(),
        first
    );
    let store = ContentStore::new(directory.path().join("store"));
    assert_eq!(store.put_file(&source).unwrap(), first);
    // Rewritten in place at the same length, then replaced under the same name.
    fs::write(&source, b"other engine").unwrap();
    let second = seedbox_driver_execution::admission::file_digest(&source).unwrap();
    assert_eq!(second, ContentDigest::of(b"other engine"));
    assert_eq!(store.put_file(&source).unwrap(), second);
    let replacement = directory.path().join("replacement");
    fs::write(&replacement, b"third").unwrap();
    fs::rename(&replacement, &source).unwrap();
    assert_eq!(
        seedbox_driver_execution::admission::file_digest(&source).unwrap(),
        ContentDigest::of(b"third")
    );
}

#[test]
fn oracle_report_is_observed_before_a_failed_final_record_write() {
    let fixture = Fixture::new();
    let mut exit = None;
    let executor = fixture.executing(|request| {
        // Force only the final manifest replacement to fail, after execution
        // has returned its authoritative oracle report.
        let manifest = request.output.parent().unwrap().join("manifest.json");
        fs::remove_file(&manifest)?;
        fs::create_dir(&manifest)?;
        Ok(fixture.backend.report.clone())
    });
    let result = fixture.orchestrator.run_prepared_observed(
        &fixture.simulation,
        RunOptions::default(),
        fixture.preparer.inputs.clone(),
        &executor,
        |progress| {
            if let RunProgress::Finalizing { report } = progress {
                exit = report.and_then(|r| r.workload_exit.clone());
            }
            Ok(())
        },
    );
    assert!(result.is_err());
    assert_eq!(exit, fixture.backend.report.workload_exit);
}

#[test]
fn interactive_history_is_sealed_as_portable_overrides_and_replays_without_journal() {
    use seedbox_engine_protocol::{
        host_control::BoundaryPosition,
        intervention::{History, Intervention},
    };
    let fixture = Fixture::new();
    let history = History {
        entries: vec![Intervention {
            request_id: "stop-workload".into(),
            position: BoundaryPosition {
                boundary_index: 2,
                completed_rounds: 1,
                simulated_time_ns: 100,
            },
            plan: seedbox_engine_protocol::control::Plan {
                id: 1,
                at_ns: 100,
                deadline_ns: 200,
                action: seedbox_engine_protocol::control::Action::Stop {
                    machine: "oracle".into(),
                },
            },
        }],
        ..History::default()
    };
    let backend = fixture.executing(|request| {
        let path = request.intervention_history.unwrap();
        if let Some(retained) = request.replay_interventions {
            assert_eq!(retained, &history);
            assert_eq!(seedbox_artifacts::interventions::read(path)?, history);
        } else {
            let mut journal = seedbox_artifacts::interventions::Journal::open(path)?;
            journal.append(history.entries[0].clone())?;
        }
        fixture.backend.execute(request)
    });
    let first = fixture
        .orchestrator
        .run(
            &fixture.simulation,
            RunOptions::default(),
            &fixture.preparer,
            &backend,
        )
        .unwrap();
    assert_eq!(first.record.identity.interventions, None);
    assert_eq!(first.record.identity.program.len(), 1);
    assert_eq!(first.record.identity.program[0].key, "stop-workload");
    assert_eq!(first.record.identity.program[0].at_ns, 100);
    assert_eq!(first.record.identity.program[0].timeout_ns, 100);
    assert_eq!(inspect(&first.directory).unwrap(), first.record);
    fs::remove_file(first.directory.join("interventions.json")).unwrap();
    let replay = replay_retained(&fixture.orchestrator, &first.directory, &backend).unwrap();
    assert_eq!(replay.record.run_id, first.record.run_id);
    assert_eq!(replay.comparison.status, ComparisonStatus::Match);
    assert_eq!(fixture.backend.calls.load(Ordering::SeqCst), 2);
    let empty = Orchestrator::new(
        ContentStore::new(fixture.directory.path().join("empty-store")),
        fixture.directory.path().join("missing-history-attempts"),
    );
    let missing = replay_retained(&empty, &first.directory, &backend).unwrap();
    assert_eq!(missing.record.outcome, Outcome::Error);
    assert!(missing.record.error.is_some());
    assert_eq!(fixture.backend.calls.load(Ordering::SeqCst), 2);
}

#[test]
fn resume_preflight_runs_before_interventions_or_execution_inputs_are_materialized() {
    struct Refuse<'a>(&'a Backend);
    impl ExecutionBackend for Refuse<'_> {
        fn profile(&self) -> ExecutionProfile {
            self.0.profile()
        }
        fn preflight_resume(
            &self,
            checkpoint: Option<&std::path::Path>,
            _: Option<&config::Control>,
        ) -> Result<()> {
            assert_eq!(
                checkpoint.unwrap(),
                std::path::Path::new("unsupported-checkpoint")
            );
            Err(Error::Setup("resume refused by backend policy".into()))
        }
        fn runtime_inputs<'a>(&self, _: &'a PreparedInputs) -> Vec<&'a ContentDigest> {
            panic!("input selection must not follow a refused resume")
        }
        fn execute(&self, _: ExecutionRequest<'_>) -> Result<ExecutionReport> {
            panic!("refused resume must not execute")
        }
    }
    let fixture = Fixture::new();
    let run = fixture
        .orchestrator
        .clone()
        .with_checkpoint("unsupported-checkpoint".into())
        .run(
            &fixture.simulation,
            RunOptions::default(),
            &fixture.preparer,
            &Refuse(&fixture.backend),
        )
        .unwrap();
    assert!(run
        .record
        .error
        .unwrap()
        .contains("resume refused by backend policy"));
    assert!(!run.directory.join("inputs").exists());
    assert!(!run.directory.join("output").exists());
    assert!(!run.directory.join("interventions.json").exists());
}

mod admission_ordering {
    use app::{Error, Result};
    use seedbox_application as app;
    use std::{collections::BTreeMap, path::Path};
    #[test]
    fn preflight_refusal_is_before_application_preparation_or_execution() {
        use seedbox_application::{
            config,
            execution::*,
            orchestration::{Orchestrator, RunOptions},
            store::ContentStore,
        };
        use std::sync::atomic::{AtomicUsize, Ordering};
        struct Refused;
        impl ExecutionBackend for Refused {
            fn profile(&self) -> ExecutionProfile {
                ExecutionProfile {
                    backend: "fixture".into(),
                    witness_mode: WitnessMode::Exits,
                    semantic_parameters: SemanticParameters::new(),
                }
            }
            fn preflight(&self, _: Option<&Path>) -> Result<()> {
                Err(Error::Setup("cannot open /dev/kvm: EACCES (13)".into()))
            }
            fn execute(&self, _: ExecutionRequest<'_>) -> Result<ExecutionReport> {
                panic!("must not execute after host refusal")
            }
        }
        struct Prepare(AtomicUsize);
        impl PreparationBackend for Prepare {
            fn prepare(&self, _: &config::Simulation, _: &ContentStore) -> Result<PreparedInputs> {
                self.0.fetch_add(1, Ordering::SeqCst);
                panic!("must not prepare after host refusal")
            }
        }
        let root = tempfile::tempdir().unwrap();
        let prepare = Prepare(AtomicUsize::new(0));
        let simulation = config::parse(
            "name: host-refusal\nmachines: {}\noracle: {image: fixture:synthetic}\n",
            &root.path().join("seedbox.yaml"),
            &BTreeMap::new(),
        )
        .unwrap();
        let orchestrator = Orchestrator::new(
            ContentStore::new(root.path().join("objects")),
            root.path().join("runs"),
        );
        let error = orchestrator
            .run(
                &simulation,
                RunOptions {
                    seed: Some(1),
                    ..Default::default()
                },
                &prepare,
                &Refused,
            )
            .unwrap_err()
            .to_string();
        assert!(error.contains("EACCES (13)"));
        assert_eq!(prepare.0.load(Ordering::SeqCst), 0);
    }
}
