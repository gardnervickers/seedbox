//! Contract tests use a test-only backend. No hardware qualification is implied.
use seedbox::{
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
    fn prepare_cancellable(
        &self,
        simulation: &Simulation,
        store: &ContentStore,
        context: &seedbox::preparation::PreparationContext,
    ) -> Result<PreparedInputs> {
        context.check()?;
        self.prepare(simulation, store)
    }
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

impl Fixture {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("compose.yaml");
        fs::write(&source, "name: test\nservices:\n  workload: {image: example/workload:v1}\nx-seedbox:\n  version: 1\n  seed: 1009\n  limits: {simulated_time: 10s, wall_time: 10s}\n  check: {service: workload}\n").unwrap();
        let simulation = config::load(&source, &BTreeMap::new()).unwrap();
        let store = ContentStore::new(directory.path().join("store"));
        let kernel = store.put(b"fixture kernel").unwrap();
        let engine = store.put(b"fixture engine").unwrap();
        let guest = store.put(b"fixture guest").unwrap();
        let manifest = store.put(b"fixture manifest").unwrap();
        let config = store.put(b"fixture image config").unwrap();
        let layer = store.put(b"fixture layer").unwrap();
        let profile = ExecutionProfile {
            backend: "test-only".into(),
            engine,
            witness_mode: WitnessMode::Exits,
            semantic_parameters: BTreeMap::from([("kernel_abi".into(), "v1".into())]),
        };
        let inputs = PreparedInputs {
            kernel,
            profile: profile.clone(),
            placement: BTreeMap::from([("workload".into(), "workload".into())]),
            guests: BTreeMap::from([("workload".into(), guest)]),
            images: BTreeMap::from([(
                "workload".into(),
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
                    },
                },
            )]),
            auxiliary: BTreeMap::new(),
            provenance: BTreeMap::from([("builder".into(), "fixture-v1".into())]),
        };
        let backend = Backend {
            profile,
            performance: BTreeMap::from([("SEEDBOX_UNDO".into(), "256".into())]),
            extra_logs: BTreeMap::new(),
            report: ExecutionReport {
                reason: StopReason::Completed,
                workload_exit: Some(WorkloadExit {
                    service: "workload".into(),
                    code: 0,
                }),
                witness: Some(ContentDigest::of(b"witness")),
                full_state: None,
                faults: vec![],
                detail: None,
                engine_qualification: None,
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
    fn replay(&self, run: &Run) -> Run {
        self.orchestrator
            .replay(&run.directory, &self.backend, &CancellationToken::default())
            .unwrap()
    }
}

#[test]
fn run_retains_inputs_result_logs_and_replay_does_not_prepare_again() {
    let fixture = Fixture::new();
    let first = fixture.run();
    assert_eq!(first.record.outcome, Outcome::Pass);
    assert_eq!(inspect(&first.directory).unwrap(), first.record);
    assert_eq!(first.record.logs.len(), 1);
    fs::remove_file(&fixture.simulation.source).unwrap();
    let fresh = fixture.replay(&first);
    assert_eq!(fresh.record.run_id, first.record.run_id);
    assert_ne!(fresh.directory, first.directory);
    assert_eq!(fresh.record.replay.unwrap().status, ReplayStatus::Match);
    assert_eq!(fixture.preparer.calls.load(Ordering::SeqCst), 1);
    assert_eq!(fixture.backend.calls.load(Ordering::SeqCst), 2);
}

#[test]
fn reproduced_application_failure_is_separate_from_replay_mismatch() {
    let mut fixture = Fixture::new();
    fixture.backend.report.workload_exit.as_mut().unwrap().code = 3;
    let failed = fixture.run();
    let reproduced = fixture.replay(&failed);
    assert_eq!(reproduced.record.outcome, Outcome::Fail);
    assert_eq!(
        reproduced.record.replay.unwrap().status,
        ReplayStatus::Match
    );
    // Two different failing exit codes must not be treated as equal outcomes.
    fixture.backend.report.workload_exit.as_mut().unwrap().code = 4;
    let different_failure = fixture.replay(&failed);
    assert_eq!(different_failure.record.outcome, Outcome::Fail);
    assert_eq!(
        different_failure.record.replay.unwrap().status,
        ReplayStatus::Mismatch
    );
}

#[test]
fn witness_fault_history_and_full_state_comparisons_are_distinct() {
    let mut fixture = Fixture::new();
    let first = fixture.run();
    fixture.backend.report.witness = Some(ContentDigest::of(b"diverged"));
    let replay = fixture.replay(&first).record.replay.unwrap();
    assert_eq!(replay.status, ReplayStatus::Mismatch);
    assert_eq!(replay.witness_matches, Some(false));
    assert_eq!(replay.full_state_matches, None);
    fixture.backend.report.witness = first.record.report.as_ref().unwrap().witness.clone();
    fixture.backend.report.full_state = Some(BTreeMap::from([(
        "workload".into(),
        ContentDigest::of(b"state"),
    )]));
    let with_state = fixture.run();
    let matched = fixture.replay(&with_state).record.replay.unwrap();
    assert_eq!(matched.full_state_matches, Some(true));
    assert_eq!(matched.status, ReplayStatus::Match);
    fixture.backend.report.full_state = None;
    assert_eq!(
        fixture.replay(&with_state).record.replay.unwrap().status,
        ReplayStatus::Mismatch
    );
    fixture.backend.report.faults.push(FaultEvent {
        requested_at_ns: 1,
        applied_at_ns: 2,
        service: "workload".into(),
        action: "restart".into(),
        detail: "test".into(),
    });
    let replay = fixture.replay(&first).record.replay.unwrap();
    assert_eq!(replay.witness_matches, Some(true));
    assert!(!replay.fault_history_matches);
    assert_eq!(replay.status, ReplayStatus::Mismatch);
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
        "workload".into(),
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
            fixture.replay(&run).record.replay.unwrap().status,
            if expected == Outcome::Timeout {
                ReplayStatus::Match
            } else {
                ReplayStatus::NotComparable
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
        .service = "workload".into();
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
    let mut fixture = Fixture::new();
    fixture.simulation.limits.wall_time_ns = 1;
    assert_eq!(fixture.run().record.outcome, Outcome::Incomplete);
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
    let digest = &fixture.preparer.inputs.images["workload"].layers[0];
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
    fixture
        .backend
        .profile
        .semantic_parameters
        .insert("witness".into(), "changed".into());
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
    assert!(fixture
        .orchestrator
        .replay(
            &first.directory,
            &fixture.backend,
            &CancellationToken::default()
        )
        .is_err());
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
    fixture.preparer.inputs.kernel = store.put(b"new kernel").unwrap();
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
        .get_mut("workload")
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
fn content_store_never_silently_repairs_corruption_or_accepts_path_digests() {
    let directory = tempfile::tempdir().unwrap();
    let store = ContentStore::new(directory.path());
    let hash = store.put(b"input").unwrap();
    let copied = tempfile::tempdir().unwrap();
    let private = store.materialize(&hash, copied.path()).unwrap();
    fs::write(&private, b"backend changed its private input").unwrap();
    assert_eq!(store.read(&hash).unwrap(), b"input");
    fs::write(directory.path().join("sha256").join(hash.hex()), b"corrupt").unwrap();
    assert!(store.put(b"input").is_err());
    assert!(serde_json::from_str::<ContentDigest>("\"../../outside\"").is_err());
}

#[test]
fn wall_limit_is_infrastructure_but_simulated_limit_can_reproduce() {
    let mut fixture = Fixture::new();
    let baseline = fixture.run();
    fixture.backend.report.reason = StopReason::WallTimeLimit;
    let replay = fixture.replay(&baseline);
    assert_eq!(replay.record.outcome, Outcome::Incomplete);
    assert_eq!(
        replay.record.replay.unwrap().status,
        ReplayStatus::NotComparable
    );
    fixture.backend.report.reason = StopReason::SimulatedTimeLimit;
    fixture.backend.report.workload_exit = None;
    let timed_out = fixture.run();
    assert_eq!(timed_out.record.outcome, Outcome::Timeout);
    assert_eq!(
        fixture.replay(&timed_out).record.replay.unwrap().status,
        ReplayStatus::Match
    );
    fixture.backend.report.witness = None;
    assert_eq!(
        fixture.replay(&timed_out).record.replay.unwrap().status,
        ReplayStatus::NotComparable
    );
}

#[test]
fn co_located_services_use_one_machine_state_digest_and_complete_placement() {
    let mut fixture = Fixture::new();
    fixture.simulation.services.insert(
        "redis".into(),
        fixture.simulation.services["workload"].clone(),
    );
    fixture.preparer.inputs.images.insert(
        "redis".into(),
        fixture.preparer.inputs.images["workload"].clone(),
    );
    let guest = fixture.preparer.inputs.guests.remove("workload").unwrap();
    fixture.preparer.inputs.guests.insert("vm0".into(), guest);
    fixture.preparer.inputs.placement = BTreeMap::from([
        ("workload".into(), "vm0".into()),
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
        fixture.replay(&run).record.replay.unwrap().status,
        ReplayStatus::Match
    );
    fixture.preparer.inputs.placement.remove("redis");
    let calls = fixture.backend.calls.load(Ordering::SeqCst);
    assert_eq!(fixture.run().record.outcome, Outcome::Error);
    assert_eq!(fixture.backend.calls.load(Ordering::SeqCst), calls);
}

#[test]
fn performance_changes_are_recorded_without_changing_identity_or_bypassing_comparison() {
    let mut fixture = Fixture::new();
    let first = fixture.run();
    fixture
        .backend
        .performance
        .insert("SEEDBOX_UNDO".into(), "16".into());
    let replay = fixture.replay(&first);
    assert_eq!(replay.record.run_id, first.record.run_id);
    assert_ne!(
        replay.record.performance_parameters,
        first.record.performance_parameters
    );
    assert_eq!(replay.record.replay.unwrap().status, ReplayStatus::Match);
    fixture.backend.report.witness = Some(ContentDigest::of(b"bad performance optimization"));
    assert_eq!(
        fixture.replay(&first).record.replay.unwrap().status,
        ReplayStatus::Mismatch
    );
}

#[test]
fn cross_engine_replay_is_explicit_retained_and_still_checks_semantics_and_witness() {
    let mut fixture = Fixture::new();
    let first = fixture.run();
    let engine = ContentStore::new(fixture.directory.path().join("store"))
        .put(b"new engine")
        .unwrap();
    fixture.backend.profile.engine = engine.clone();
    assert_eq!(fixture.replay(&first).record.outcome, Outcome::Error);
    let options = ReplayOptions {
        allow_engine_change: true,
        ..Default::default()
    };
    let replay = fixture
        .orchestrator
        .replay_with_options(&first.directory, &fixture.backend, options.clone())
        .unwrap();
    assert!(replay.record.allow_engine_change);
    assert_ne!(replay.record.run_id, first.record.run_id);
    assert_eq!(replay.record.identity.inputs.profile.engine, engine);
    assert_eq!(replay.record.replay.unwrap().status, ReplayStatus::Match);
    fixture.backend.report.witness = Some(ContentDigest::of(b"regression"));
    assert_eq!(
        fixture
            .orchestrator
            .replay_with_options(&first.directory, &fixture.backend, options.clone())
            .unwrap()
            .record
            .replay
            .unwrap()
            .status,
        ReplayStatus::Mismatch
    );
    fixture
        .backend
        .profile
        .semantic_parameters
        .insert("kernel_abi".into(), "changed".into());
    assert_eq!(
        fixture
            .orchestrator
            .replay_with_options(&first.directory, &fixture.backend, options)
            .unwrap()
            .record
            .outcome,
        Outcome::Error
    );
}

#[test]
fn fault_comparison_uses_acknowledged_effect_time_and_rejects_invalid_acknowledgements() {
    let mut fixture = Fixture::new();
    fixture.backend.report.faults.push(FaultEvent {
        requested_at_ns: 10,
        applied_at_ns: 20,
        service: "workload".into(),
        action: "stop".into(),
        detail: "guest acknowledged".into(),
    });
    let run = fixture.run();
    fixture.backend.report.faults[0].applied_at_ns = 21;
    assert_eq!(
        fixture.replay(&run).record.replay.unwrap().status,
        ReplayStatus::Mismatch
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
fn large_artifacts_stream_with_bounded_reads_and_corrupt_copies_are_not_published() {
    use std::io::{self, Read};
    struct BoundedSource(usize);
    impl Read for BoundedSource {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            assert!(buffer.len() <= 64 * 1024);
            let count = buffer.len().min(self.0);
            buffer[..count].fill(7);
            self.0 -= count;
            Ok(count)
        }
    }
    let directory = tempfile::tempdir().unwrap();
    let store = ContentStore::new(directory.path().join("store"));
    let size = 12 * 1024 * 1024;
    let digest = store.put_reader(BoundedSource(size)).unwrap();
    assert!(store
        .read(&digest)
        .unwrap_err()
        .to_string()
        .contains("read limit"));
    let output = directory.path().join("inputs");
    let path = store.materialize(&digest, &output).unwrap();
    assert_eq!(fs::metadata(path).unwrap().len(), size as u64);
    fs::write(
        directory.path().join("store/sha256").join(digest.hex()),
        b"corrupt",
    )
    .unwrap();
    let bad_output = directory.path().join("bad-inputs");
    assert!(store.materialize(&digest, &bad_output).is_err());
    assert_eq!(fs::read_dir(bad_output).unwrap().count(), 0);
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
    let run = fixture.run();
    assert_eq!(run.record.outcome, Outcome::Error);
    assert!(run.record.error.unwrap().contains("guest rejected fault 1"));
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
    fixture.backend.report.faults[0].action = "stop".into();
    assert_eq!(fixture.run().record.outcome, Outcome::Error);
}

#[test]
fn exploration_freezes_full_closure_and_continues_failure_and_witnessed_timeout() {
    use seedbox::exploration::ExploreOptions;
    struct Sequence<'a>(&'a Fixture);
    impl ExecutionBackend for Sequence<'_> {
        fn profile(&self) -> ExecutionProfile {
            self.0.backend.profile()
        }
        fn performance_parameters(&self) -> BTreeMap<String, String> {
            self.0.backend.performance_parameters()
        }
        fn execute(&self, request: ExecutionRequest<'_>) -> Result<ExecutionReport> {
            let seed = request.seed;
            // Sources disappear after preparation. Every next run must materialize
            // the original complete closure and keep its provenance unchanged.
            if seed == 10 {
                fs::remove_file(&self.0.simulation.source)?;
            }
            let mut report = self.0.backend.execute(request)?;
            if seed == 11 {
                report.workload_exit.as_mut().unwrap().code = 3;
            }
            if seed == 12 {
                report.reason = StopReason::SimulatedTimeLimit;
            }
            if seed == 13 {
                return Err(Error::Execution("real retained backend error".into()));
            }
            Ok(report)
        }
    }
    let fixture = Fixture::new();
    let batch = fixture
        .orchestrator
        .explore(
            &fixture.simulation,
            ExploreOptions {
                seed: Some(10),
                count: 5,
                ..Default::default()
            },
            &fixture.preparer,
            &Sequence(&fixture),
        )
        .unwrap();
    assert_eq!(fixture.preparer.calls.load(Ordering::SeqCst), 1);
    assert_eq!(batch.outcome, Outcome::Error);
    assert_eq!(batch.undispatched_count, 1);
    assert_eq!(
        batch
            .attempts
            .iter()
            .map(|a| a.outcome.clone().unwrap())
            .collect::<Vec<_>>(),
        vec![
            Outcome::Pass,
            Outcome::Fail,
            Outcome::Timeout,
            Outcome::Error
        ]
    );
    for attempt in &batch.attempts {
        let record = inspect(attempt.directory.as_ref().unwrap()).unwrap();
        assert_eq!(record.identity.inputs, fixture.preparer.inputs);
        assert_eq!(record.identity.configuration, fixture.simulation);
        assert_eq!(record.performance_parameters, fixture.backend.performance);
        assert_eq!(record.logs.len(), 1);
    }
    let retained = seedbox::exploration::inspect(&batch.directory).unwrap();
    assert!(retained.finished);
    assert_eq!(retained.outcome, Outcome::Error);
}

#[test]
fn exploration_bounds_fail_before_state_and_huge_count_is_lazy() {
    use seedbox::exploration::ExploreOptions;
    let fixture = Fixture::new();
    for options in [
        ExploreOptions {
            count: 0,
            ..Default::default()
        },
        ExploreOptions {
            seed: Some(u64::MAX),
            count: 2,
            ..Default::default()
        },
    ] {
        assert!(fixture
            .orchestrator
            .explore(
                &fixture.simulation,
                options,
                &fixture.preparer,
                &fixture.backend
            )
            .is_err());
        assert!(!fixture.directory.path().join("runs").exists());
    }
    assert_eq!(
        seedbox::exploration::bounds(
            &fixture.simulation,
            &ExploreOptions {
                seed: Some(u64::MAX),
                ..Default::default()
            }
        )
        .unwrap(),
        (u64::MAX, u64::MAX)
    );
    let token = CancellationToken::default();
    token.cancel();
    let batch = fixture
        .orchestrator
        .explore(
            &fixture.simulation,
            ExploreOptions {
                seed: Some(0),
                count: u64::MAX,
                cancellation: token,
            },
            &fixture.preparer,
            &fixture.backend,
        )
        .unwrap();
    assert_eq!(batch.outcome, Outcome::Incomplete);
    assert!(batch.attempts.is_empty());
    assert_eq!(batch.undispatched_count, u64::MAX);
    assert_eq!(fixture.preparer.calls.load(Ordering::SeqCst), 0);
}

#[test]
fn exploration_corrupt_frozen_blob_retains_error_without_repreparing() {
    use seedbox::exploration::ExploreOptions;
    struct Corrupt<'a>(&'a Fixture);
    impl ExecutionBackend for Corrupt<'_> {
        fn profile(&self) -> ExecutionProfile {
            self.0.backend.profile()
        }
        fn performance_parameters(&self) -> BTreeMap<String, String> {
            self.0.backend.performance_parameters()
        }
        fn execute(&self, request: ExecutionRequest<'_>) -> Result<ExecutionReport> {
            let report = self.0.backend.execute(request)?;
            fs::write(
                self.0
                    .directory
                    .path()
                    .join("store/sha256")
                    .join(self.0.preparer.inputs.kernel.hex()),
                b"corrupted actual retained bytes",
            )?;
            Ok(report)
        }
    }
    let fixture = Fixture::new();
    let batch = fixture
        .orchestrator
        .explore(
            &fixture.simulation,
            ExploreOptions {
                count: 3,
                ..Default::default()
            },
            &fixture.preparer,
            &Corrupt(&fixture),
        )
        .unwrap();
    assert_eq!(batch.outcome, Outcome::Error);
    assert_eq!(batch.attempts.len(), 2);
    assert_eq!(fixture.preparer.calls.load(Ordering::SeqCst), 1);
    assert_eq!(fixture.backend.calls.load(Ordering::SeqCst), 1);
    let error = inspect(batch.attempts[1].directory.as_ref().unwrap()).unwrap();
    assert_eq!(error.outcome, Outcome::Error);
    assert!(error.error.unwrap().contains("integrity"));
}

#[test]
fn exploration_active_run_is_durable_incomplete_then_cancellation_stops_dispatch() {
    use seedbox::exploration::ExploreOptions;
    use std::sync::{mpsc, Mutex};
    struct Blocked<'a> {
        fixture: &'a Fixture,
        ready: mpsc::Sender<std::path::PathBuf>,
        resume: Mutex<mpsc::Receiver<()>>,
    }
    impl ExecutionBackend for Blocked<'_> {
        fn profile(&self) -> ExecutionProfile {
            self.fixture.backend.profile()
        }
        fn performance_parameters(&self) -> BTreeMap<String, String> {
            self.fixture.backend.performance_parameters()
        }
        fn execute(&self, request: ExecutionRequest<'_>) -> Result<ExecutionReport> {
            self.ready
                .send(request.output.parent().unwrap().to_owned())
                .unwrap();
            self.resume.lock().unwrap().recv().unwrap();
            self.fixture.backend.execute(request)
        }
    }
    let fixture = Fixture::new();
    let token = CancellationToken::default();
    let (ready_tx, ready_rx) = mpsc::channel();
    let (resume_tx, resume_rx) = mpsc::channel();
    let backend = Blocked {
        fixture: &fixture,
        ready: ready_tx,
        resume: Mutex::new(resume_rx),
    };
    std::thread::scope(|scope| {
        let cancellation = token.clone();
        let task = scope.spawn(|| {
            fixture
                .orchestrator
                .explore(
                    &fixture.simulation,
                    ExploreOptions {
                        count: 3,
                        cancellation,
                        ..Default::default()
                    },
                    &fixture.preparer,
                    &backend,
                )
                .unwrap()
        });
        let directory = ready_rx.recv().unwrap();
        let before = inspect(&directory).unwrap();
        assert_eq!(before.outcome, Outcome::Incomplete);
        let exploration = directory
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .parent()
            .unwrap();
        let partial = seedbox::exploration::inspect(exploration).unwrap();
        assert!(!partial.finished);
        assert_eq!(partial.outcome, Outcome::Incomplete);
        assert_eq!(partial.attempts.len(), 1);
        token.cancel();
        resume_tx.send(()).unwrap();
        let batch = task.join().unwrap();
        assert_eq!(batch.outcome, Outcome::Incomplete);
        assert_eq!(batch.undispatched_count, 2);
        assert_eq!(inspect(&directory).unwrap().outcome, Outcome::Incomplete);
    });
}

#[test]
fn exploration_checkpoint_failure_leaves_real_committed_run_and_no_later_dispatch() {
    use seedbox::exploration::ExploreOptions;
    struct BreakCheckpoint<'a>(&'a Fixture);
    impl ExecutionBackend for BreakCheckpoint<'_> {
        fn profile(&self) -> ExecutionProfile {
            self.0.backend.profile()
        }
        fn performance_parameters(&self) -> BTreeMap<String, String> {
            self.0.backend.performance_parameters()
        }
        fn execute(&self, request: ExecutionRequest<'_>) -> Result<ExecutionReport> {
            let exploration = request
                .output
                .parent()
                .unwrap()
                .parent()
                .unwrap()
                .parent()
                .unwrap()
                .parent()
                .unwrap();
            fs::rename(
                exploration.join("exploration.json"),
                exploration.join("before.json"),
            )?;
            fs::create_dir(exploration.join("exploration.json"))?;
            self.0.backend.execute(request)
        }
    }
    let fixture = Fixture::new();
    assert!(fixture
        .orchestrator
        .explore(
            &fixture.simulation,
            ExploreOptions {
                count: 3,
                ..Default::default()
            },
            &fixture.preparer,
            &BreakCheckpoint(&fixture)
        )
        .is_err());
    assert_eq!(fixture.backend.calls.load(Ordering::SeqCst), 1);
    let batch = fs::read_dir(fixture.directory.path().join("runs"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let before: seedbox::exploration::Exploration =
        serde_json::from_slice(&fs::read(batch.join("before.json")).unwrap()).unwrap();
    assert!(!before.finished);
    assert_eq!(before.outcome, Outcome::Incomplete);
    let run = fs::read_dir(&before.attempts[0].subtree)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    assert_eq!(inspect(&run).unwrap().outcome, Outcome::Pass);
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
fn actual_vmm_admission_rejects_old_proof_before_execution_and_engine_override() {
    let fixture = Fixture::new();
    let original = fixture.run();
    let manifest = fs::read(original.directory.join("manifest.json")).unwrap();
    let engine = fixture.directory.path().join("retained-engine");
    fs::write(&engine, b"fixture engine").unwrap();
    let store = ContentStore::new(fixture.directory.path().join("store"));
    let executor = seedbox::vmm::VmmExecutor::new(
        &engine,
        &store,
        None,
        512,
        WitnessMode::Exits,
        seedbox::counter::CounterProfile::Zen,
    )
    .unwrap();
    for allow_engine_change in [false, true] {
        let rejected = fixture
            .orchestrator
            .replay_with_options(
                &original.directory,
                &executor,
                ReplayOptions {
                    allow_engine_change,
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(rejected.record.outcome, Outcome::Error);
        assert_eq!(
            rejected.record.replay.unwrap().status,
            ReplayStatus::NotComparable
        );
        assert!(rejected
            .record
            .error
            .unwrap()
            .contains("host admission proof missing"));
        assert!(!rejected.directory.join("output").exists());
        assert_eq!(
            fs::read(original.directory.join("manifest.json")).unwrap(),
            manifest
        );
    }
    assert_eq!(fixture.preparer.calls.load(Ordering::SeqCst), 1);
}

#[test]
fn exploration_actual_unsupported_profile_refuses_before_all_preparation() {
    let fixture = Fixture::new();
    let engine = fixture.directory.path().join("unadmitted-engine");
    fs::write(&engine, b"fixture engine").unwrap();
    let store = ContentStore::new(fixture.directory.path().join("store"));
    let executor = seedbox::vmm::VmmExecutor::new(
        &engine,
        &store,
        None,
        512,
        WitnessMode::Exits,
        seedbox::counter::CounterProfile::Zen,
    )
    .unwrap();
    let batch = fixture
        .orchestrator
        .explore(
            &fixture.simulation,
            seedbox::exploration::ExploreOptions {
                count: 3,
                ..Default::default()
            },
            &fixture.preparer,
            &executor,
        )
        .unwrap();
    assert_eq!(fixture.preparer.calls.load(Ordering::SeqCst), 0);
    assert_eq!(fixture.backend.calls.load(Ordering::SeqCst), 0);
    assert_eq!(batch.outcome, Outcome::Error);
    assert!(batch.finished);
    assert!(batch.inputs.is_none());
    assert!(batch.attempts.is_empty());
    assert_eq!(batch.undispatched_count, 3);
    assert!(batch
        .error
        .as_ref()
        .unwrap()
        .contains("outside the admitted host policy"));
    assert!(!batch.directory.join("attempts").exists());
    assert!(!batch.preparation_directory.join("setup.json").exists());
    assert_eq!(
        seedbox::exploration::inspect(&batch.directory)
            .unwrap()
            .error,
        batch.error
    );
}

#[test]
fn exploration_admission_drift_stops_with_retained_partial_evidence() {
    for fail_at in [1, 2, 3] {
        let fixture = Fixture::new();
        let backend = AdmissionBackend {
            backend: &fixture.backend,
            checks: AtomicUsize::new(0),
            fail_at,
        };
        let batch = fixture
            .orchestrator
            .explore(
                &fixture.simulation,
                seedbox::exploration::ExploreOptions {
                    count: 3,
                    ..Default::default()
                },
                &fixture.preparer,
                &backend,
            )
            .unwrap();
        let prepared = usize::from(fail_at > 1);
        let attempts = fail_at - 1;
        assert_eq!(fixture.preparer.calls.load(Ordering::SeqCst), prepared);
        assert_eq!(
            fixture.backend.calls.load(Ordering::SeqCst),
            fail_at.saturating_sub(2)
        );
        assert_eq!(batch.attempts.len(), attempts);
        assert_eq!(batch.undispatched_count, 3 - attempts as u64);
        assert_eq!(batch.outcome, Outcome::Error);
        assert!(batch.finished);
        assert_eq!(
            batch.inputs,
            (prepared == 1).then(|| fixture.preparer.inputs.clone())
        );
        if let Some(attempt) = batch.attempts.last() {
            let run = inspect(attempt.directory.as_ref().unwrap()).unwrap();
            assert_eq!(run.outcome, Outcome::Error);
            assert!(run.error.as_ref().unwrap().contains("host drift"));
            assert_eq!(run.identity.inputs, fixture.preparer.inputs);
        }
        let retained = seedbox::exploration::inspect(&batch.directory).unwrap();
        assert_eq!(retained.outcome, batch.outcome);
        assert_eq!(retained.attempts.len(), attempts);
    }
}

#[test]
fn exploration_checks_inputs_first_and_releases_preflight_scope_before_preparing() {
    use std::sync::{atomic::AtomicBool, Mutex};
    struct CheckedPreparer<'a> {
        preparer: &'a Preparer,
        validated: &'a AtomicBool,
        lease: &'a Mutex<()>,
        reject: bool,
    }
    impl PreparationBackend for CheckedPreparer<'_> {
        fn validate_runtime_inputs(&self) -> Result<()> {
            self.validated.store(true, Ordering::SeqCst);
            if self.reject {
                return Err(Error::Integrity("unsupported runtime inputs".into()));
            }
            Ok(())
        }
        fn prepare(&self, simulation: &Simulation, store: &ContentStore) -> Result<PreparedInputs> {
            assert!(self.validated.load(Ordering::SeqCst));
            let _available = self
                .lease
                .try_lock()
                .expect("preflight retained its transient lease");
            self.preparer.prepare(simulation, store)
        }
        fn prepare_cancellable(
            &self,
            simulation: &Simulation,
            store: &ContentStore,
            context: &seedbox::preparation::PreparationContext,
        ) -> Result<PreparedInputs> {
            context.check()?;
            self.prepare(simulation, store)
        }
    }
    struct ScopedBackend<'a> {
        backend: &'a Backend,
        validated: &'a AtomicBool,
        lease: &'a Mutex<()>,
        checks: AtomicUsize,
    }
    impl ExecutionBackend for ScopedBackend<'_> {
        fn profile(&self) -> ExecutionProfile {
            self.backend.profile()
        }
        fn performance_parameters(&self) -> BTreeMap<String, String> {
            self.backend.performance_parameters()
        }
        fn preflight(&self, _: Option<&std::path::Path>) -> Result<()> {
            assert!(self.validated.load(Ordering::SeqCst));
            let _owned = self.lease.lock().unwrap();
            self.checks.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
        fn execute(&self, request: ExecutionRequest<'_>) -> Result<ExecutionReport> {
            let _available = self
                .lease
                .try_lock()
                .expect("preflight retained its transient lease");
            self.backend.execute(request)
        }
    }
    for reject in [true, false] {
        let fixture = Fixture::new();
        let validated = AtomicBool::new(false);
        let lease = Mutex::new(());
        let preparer = CheckedPreparer {
            preparer: &fixture.preparer,
            validated: &validated,
            lease: &lease,
            reject,
        };
        let backend = ScopedBackend {
            backend: &fixture.backend,
            validated: &validated,
            lease: &lease,
            checks: AtomicUsize::new(0),
        };
        let batch = fixture
            .orchestrator
            .explore(
                &fixture.simulation,
                seedbox::exploration::ExploreOptions {
                    count: 3,
                    ..Default::default()
                },
                &preparer,
                &backend,
            )
            .unwrap();
        assert_eq!(
            fixture.preparer.calls.load(Ordering::SeqCst),
            usize::from(!reject)
        );
        assert_eq!(
            backend.checks.load(Ordering::SeqCst),
            if reject { 0 } else { 4 }
        );
        assert_eq!(
            fixture.backend.calls.load(Ordering::SeqCst),
            if reject { 0 } else { 3 }
        );
        assert_eq!(
            batch.outcome,
            if reject {
                Outcome::Error
            } else {
                Outcome::Pass
            }
        );
        if !reject {
            for attempt in &batch.attempts {
                assert_eq!(
                    inspect(attempt.directory.as_ref().unwrap())
                        .unwrap()
                        .identity
                        .inputs,
                    fixture.preparer.inputs
                );
            }
        }
    }
}

#[test]
fn timed_indexes_survive_success_backend_error_cancellation_and_retention_omissions() {
    for (fail, cancel, omit) in [
        (false, false, false),
        (true, false, false),
        (false, true, false),
        (false, false, true),
    ] {
        let mut f = Fixture::new();
        f.backend.fail = fail;
        f.backend.cancel = cancel;
        let mut index = b"SBLOG001".to_vec();
        for v in [10u64, 0, 0, 0, 3] {
            index.extend_from_slice(&v.to_le_bytes());
        }
        f.backend
            .extra_logs
            .insert("machines/workload.console".into(), vec![0, 255, 10]);
        f.backend
            .extra_logs
            .insert("machines/workload.console.chunks".into(), index.clone());
        if omit {
            f.orchestrator = f.orchestrator.with_log_policy(LogPolicy {
                max_file_bytes: 4,
                max_total_bytes: 100,
            });
        }
        let run = f.run();
        assert_eq!(
            run.record.outcome,
            if fail {
                Outcome::Error
            } else if cancel {
                Outcome::Incomplete
            } else {
                Outcome::Pass
            }
        );
        let mut reader = seedbox::logs::Reader::open(&run.directory, &Default::default()).unwrap();
        if omit {
            assert!(run
                .record
                .omitted_logs
                .contains_key("machines/workload.console.chunks"));
            assert!(reader.report.chunks.is_empty());
            assert!(reader
                .report
                .omissions
                .contains_key("machines/workload.console.chunks"));
        } else {
            assert_eq!(
                run.record.logs["machines/workload.console.chunks"],
                ContentDigest::of(&index)
            );
            let mut bytes = Vec::new();
            reader.copy_chunk(0, &mut bytes).unwrap();
            assert_eq!(bytes, [0, 255, 10]);
        }
    }
}
