//! Contract tests use a test-only backend. No hardware qualification is implied.
use seedbox::{
    config::{self, Simulation},
    execution::{self, *},
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
            backend: execution::Backend::Other("test-only".into()),
            engine,
            witness_mode: WitnessMode::Exits,
            semantic_parameters: SemanticParameters::new(512),
        };
        let inputs = PreparedInputs {
            wall_clock_epoch: None,
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
                        disk_volumes: vec![],
                    },
                },
            )]),
            auxiliary: BTreeMap::new(),
            provenance: BTreeMap::from([("builder".into(), "fixture-v1".into())]),
        };
        let backend = Backend {
            profile,
            performance: BTreeMap::from([("SEEDBOX_WATCHDOG_MS".into(), "20".into())]),
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
        assert_eq!(fresh.record.replay.unwrap().status, ReplayStatus::Match);
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

mod page_evidence_tests {
    use super::*;

    fn full_state_fixture() -> (Fixture, serde_json::Value) {
        let mut f = Fixture::new();
        f.backend.profile.witness_mode = WitnessMode::FullState;
        f.preparer.inputs.profile = f.backend.profile.clone();
        let raw = serde_json::json!({
            "report_version": 1, "seed": 1009, "outcome": "end_machine_exited",
            "world_witness": ContentDigest::of(b"world"),
            "machines": [{"name": "workload", "state": "Exited(0)", "time_ns": 100, "infrastructure_error": false,
                "guest_exit_code": 0, "witness": ContentDigest::of(b"machine"),
                "state_digest": ContentDigest::of(b"state"),
                "state_digest_parts": {"ram": ContentDigest::of(b"RAM"), "regs": ContentDigest::of(b"regs"), "events": "none"}}]
        });
        update(&mut f, &raw, vec![0; 1024 * 1024]);
        (f, raw)
    }

    fn update(f: &mut Fixture, raw: &serde_json::Value, pages: Vec<u8>) {
        let digest: ContentDigest =
            serde_json::from_value(raw["machines"][0]["state_digest"].clone()).unwrap();
        f.backend.report.full_state = Some(BTreeMap::from([("workload".into(), digest)]));
        f.backend.report.witness =
            Some(serde_json::from_value(raw["world_witness"].clone()).unwrap());
        f.backend
            .extra_logs
            .insert("vmm.json".into(), serde_json::to_vec(raw).unwrap());
        f.backend
            .extra_logs
            .insert("machines/workload.pages".into(), pages);
    }

    #[test]
    fn full_state_replay_adds_page_evidence_without_changing_verdicts_or_old_json_fields() {
        let (mut f, mut raw) = full_state_fixture();
        let first = f.run();
        assert!(first.record.logs.contains_key("machines/workload.pages"));
        assert_eq!(
            fs::metadata(first.directory.join("output/machines/workload.pages"))
                .unwrap()
                .len(),
            1024 * 1024
        );
        let matched = f.replay(&first);
        let old = serde_json::to_value(matched.record.replay.unwrap()).unwrap();
        assert_eq!(old["status"], "MATCH");
        assert!(old.get("ram_pages").is_none());
        raw["world_witness"] = serde_json::to_value(ContentDigest::of(b"new world")).unwrap();
        raw["machines"][0]["state_digest"] =
            serde_json::to_value(ContentDigest::of(b"new state")).unwrap();
        raw["machines"][0]["state_digest_parts"]["ram"] =
            serde_json::to_value(ContentDigest::of(b"new RAM")).unwrap();
        let mut pages = vec![0; 1024 * 1024];
        pages[24..32].copy_from_slice(&42u64.to_le_bytes());
        update(&mut f, &raw, pages);
        let replay = f.replay(&first);
        assert_eq!(first.record.run_id, replay.record.run_id);
        assert_eq!(inspect(&replay.directory).unwrap(), replay.record);
        let comparison = replay.record.replay.unwrap();
        assert_eq!(comparison.status, ReplayStatus::Mismatch);
        assert_eq!(comparison.full_state_matches, Some(false));
        let report = comparison.ram_pages.as_ref().unwrap();
        assert!(report.limitations.is_empty());
        assert_eq!(report.machines["workload"].differing_pfns, 1);
        assert_eq!(report.machines["workload"].first_pfns, [3]);
        let new = serde_json::to_value(comparison).unwrap();
        for field in old.as_object().unwrap().keys() {
            assert!(new.get(field).is_some());
        }
        // Malformed optional evidence keeps the same semantic MISMATCH.
        update(&mut f, &raw, vec![0; 7]);
        let comparison = f.replay(&first).record.replay.unwrap();
        assert_eq!(comparison.status, ReplayStatus::Mismatch);
        assert_eq!(comparison.full_state_matches, Some(false));
        assert!(comparison.ram_pages.unwrap().limitations[0].contains("page hash length"));
    }
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
    fixture.backend.profile.semantic_parameters.page_timer = true;
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
fn unsynced_put_replaces_an_object_a_host_crash_truncated() {
    // Unsynced objects can survive a host crash renamed into place but empty
    // or short. The same bytes put again must not fail on that object forever.
    let directory = tempfile::tempdir().unwrap();
    let store = ContentStore::new(directory.path());
    let bytes = b"{\"identity\":1}";
    let digest = store.put_unsynced(bytes).unwrap();
    let object = directory.path().join("sha256").join(digest.hex());
    std::fs::write(&object, b"").unwrap();
    assert!(store.verify(&digest).is_err());
    assert_eq!(store.put_unsynced(bytes).unwrap(), digest);
    store.verify(&digest).unwrap();
    std::fs::write(&object, b"").unwrap();
    let file = directory.path().join("log");
    std::fs::write(&file, bytes).unwrap();
    assert_eq!(store.put_file_unsynced(&file).unwrap(), digest);
    store.verify(&digest).unwrap();
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
        .insert("SEEDBOX_WATCHDOG_MS".into(), "200".into());
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
    fixture.backend.profile.semantic_parameters.engine = Some(seedbox::engine::kvm_descriptor(
        seedbox::engine::ExecutableKind::InternalWorker,
        "0.2.0-replacement".into(),
    ));
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
    assert_eq!(
        replay.record.identity.inputs.profile,
        fixture.backend.profile
    );
    assert_eq!(replay.record.replay.unwrap().status, ReplayStatus::Match);
    // Switching back removes stale combined-worker dispatch metadata.
    let combined_run = replay.directory;
    fixture.backend.profile.semantic_parameters.engine = None;
    let standalone_replay = fixture
        .orchestrator
        .replay_with_options(&combined_run, &fixture.backend, options.clone())
        .unwrap();
    assert_eq!(
        standalone_replay.record.identity.inputs.profile,
        fixture.backend.profile
    );
    assert_eq!(
        standalone_replay.record.replay.unwrap().status,
        ReplayStatus::Match
    );
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
    fixture.backend.profile.semantic_parameters.memory_mb = 1024;
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
            service: "workload".into(),
            action: (*action).into(),
            detail: "machine lifecycle".into(),
        });
    }
    let run = fixture.run();
    assert_eq!(run.record.outcome, Outcome::Pass);
    assert_eq!(
        fixture.replay(&run).record.replay.unwrap().status,
        ReplayStatus::Match
    );
    fixture.backend.report.faults[0].service.clear();
    assert_eq!(fixture.run().record.outcome, Outcome::Error);
}

#[test]
fn exploration_freezes_full_closure_and_continues_failure_and_witnessed_timeout() {
    use seedbox::exploration::ExploreOptions;
    let fixture = Fixture::new();
    let sequence = fixture.executing(|request| {
        let seed = request.seed;
        // Sources disappear after preparation. Every next run must materialize
        // the original complete closure and keep its provenance unchanged.
        if seed == 10 {
            fs::remove_file(&fixture.simulation.source)?;
        }
        let mut report = fixture.backend.execute(request)?;
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
    });
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
            &sequence,
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
                workers: 1,
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
    let fixture = Fixture::new();
    let corrupt = fixture.executing(|request| {
        let report = fixture.backend.execute(request)?;
        fs::write(
            fixture
                .directory
                .path()
                .join("store/sha256")
                .join(fixture.preparer.inputs.kernel.hex()),
            b"corrupted actual retained bytes",
        )?;
        Ok(report)
    });
    let batch = fixture
        .orchestrator
        .explore(
            &fixture.simulation,
            ExploreOptions {
                count: 3,
                ..Default::default()
            },
            &fixture.preparer,
            &corrupt,
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
    let fixture = Fixture::new();
    let token = CancellationToken::default();
    let (ready_tx, ready_rx) = mpsc::channel();
    let (resume_tx, resume_rx) = mpsc::channel::<()>();
    let resume = Mutex::new(resume_rx);
    let backend = fixture.executing(|request| {
        ready_tx
            .send(request.output.parent().unwrap().to_owned())
            .unwrap();
        resume.lock().unwrap().recv().unwrap();
        fixture.backend.execute(request)
    });
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
        let partial = seedbox::exploration::inspect(batch_of(&directory)).unwrap();
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
    let fixture = Fixture::new();
    let break_checkpoint = fixture.executing(|request| {
        let exploration = batch_of(request.output.parent().unwrap());
        fs::rename(
            exploration.join("exploration.json"),
            exploration.join("before.json"),
        )?;
        fs::create_dir(exploration.join("exploration.json"))?;
        fixture.backend.execute(request)
    });
    assert!(fixture
        .orchestrator
        .explore(
            &fixture.simulation,
            ExploreOptions {
                count: 3,
                ..Default::default()
            },
            &fixture.preparer,
            &break_checkpoint
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
fn actual_vmm_admission_rejects_an_invalid_retained_profile_before_execution() {
    let mut fixture = Fixture::new();
    // The test backend records memory the VMM cannot admit.
    fixture.backend.profile.semantic_parameters.memory_mb = 32;
    fixture.preparer.inputs.profile = fixture.backend.profile.clone();
    let original = fixture.run();
    let manifest = fs::read(original.directory.join("manifest.json")).unwrap();
    let engine = fixture.directory.path().join("retained-engine");
    fs::write(&engine, b"fixture engine").unwrap();
    let store = ContentStore::new(fixture.directory.path().join("store"));
    let executor =
        seedbox::vmm::VmmExecutor::new(&engine, &store, None, 512, WitnessMode::Exits).unwrap();
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
        let e = rejected.record.error.unwrap();
        assert!(
            e.contains("admitted profile requires memory_mb in 64..65536"),
            "{e}"
        );
        assert!(!rejected.directory.join("output").exists());
        assert_eq!(
            fs::read(original.directory.join("manifest.json")).unwrap(),
            manifest
        );
    }
    assert_eq!(fixture.preparer.calls.load(Ordering::SeqCst), 1);
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

// Controlled completions exercise the real coordinator and retained lifecycle.
// Channels, rather than host sleep ordering, determine which attempt finishes.
// Allow loaded portable hosts time to schedule threads and retain artifacts.
const WORKER_WAIT: std::time::Duration = std::time::Duration::from_secs(60);

fn worker_fixture() -> Fixture {
    let mut fixture = Fixture::new();
    // A worker may wait through several control exchanges. The fixture's
    // execution limit must not turn a slow test handshake into INCOMPLETE.
    fixture.simulation.limits.wall_time_ns = 600_000_000_000;
    fixture
}

#[derive(Clone, Copy)]
enum WorkerControl {
    Complete,
    CleanupUnverified,
    WaitForCancellation,
    BreakCheckpoint,
}
struct ControlledWorkers<'a> {
    fixture: &'a Fixture,
    capacity: u32,
    ready: std::sync::mpsc::Sender<(u64, std::path::PathBuf)>,
    controls: BTreeMap<u64, std::sync::Mutex<std::sync::mpsc::Receiver<WorkerControl>>>,
    active: AtomicUsize,
    peak: AtomicUsize,
    seeds: std::sync::Mutex<Vec<u64>>,
}
impl ExecutionBackend for ControlledWorkers<'_> {
    fn preflight_workers(&self, workers: u32) -> Result<()> {
        if workers > self.capacity {
            return Err(Error::Setup(
                "controlled physical-core capacity unavailable".into(),
            ));
        }
        Ok(())
    }
    fn profile(&self) -> ExecutionProfile {
        self.fixture.backend.profile()
    }
    fn performance_parameters(&self) -> BTreeMap<String, String> {
        self.fixture.backend.performance_parameters()
    }
    fn execute(&self, request: ExecutionRequest<'_>) -> Result<ExecutionReport> {
        struct Active<'a>(&'a AtomicUsize);
        impl Drop for Active<'_> {
            fn drop(&mut self) {
                self.0.fetch_sub(1, Ordering::SeqCst);
            }
        }
        let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
        let _active = Active(&self.active);
        self.peak.fetch_max(active, Ordering::SeqCst);
        let mut seeds = self.seeds.lock().unwrap();
        assert!(!seeds.contains(&request.seed), "duplicate dispatch");
        seeds.push(request.seed);
        drop(seeds);
        self.ready
            .send((request.seed, request.output.parent().unwrap().to_owned()))
            .unwrap();
        let command = self.controls[&request.seed]
            .lock()
            .unwrap()
            .recv_timeout(WORKER_WAIT)
            .expect("test control deadline");
        match command {
            WorkerControl::CleanupUnverified => {
                return Err(Error::Execution("owned cleanup-unverified".into()))
            }
            WorkerControl::WaitForCancellation => {
                let deadline = std::time::Instant::now() + WORKER_WAIT;
                while !request.cancellation.is_cancelled() {
                    assert!(
                        std::time::Instant::now() < deadline,
                        "owned cancellation was not forwarded"
                    );
                    std::thread::yield_now();
                }
            }
            WorkerControl::BreakCheckpoint => {
                let batch = batch_of(request.output.parent().unwrap());
                fs::rename(batch.join("exploration.json"), batch.join("before.json"))?;
                fs::create_dir(batch.join("exploration.json"))?;
            }
            WorkerControl::Complete => {}
        }
        let seed = request.seed;
        let mut report = self.fixture.backend.execute(request)?;
        report.witness = Some(ContentDigest::of(&seed.to_le_bytes()));
        if seed % 2 == 1 {
            report.workload_exit.as_mut().unwrap().code = 1;
        }
        Ok(report)
    }
}
type ReadyWorkers = std::sync::mpsc::Receiver<(u64, std::path::PathBuf)>;
type WorkerPermits = BTreeMap<u64, std::sync::mpsc::Sender<WorkerControl>>;
fn controlled_workers(
    fixture: &Fixture,
    first: u64,
    count: u64,
    capacity: u32,
) -> (ControlledWorkers<'_>, ReadyWorkers, WorkerPermits) {
    let (ready, received) = std::sync::mpsc::channel();
    let mut controls = BTreeMap::new();
    let mut senders = BTreeMap::new();
    for seed in first..first + count {
        let (sender, receiver) = std::sync::mpsc::channel();
        controls.insert(seed, std::sync::Mutex::new(receiver));
        senders.insert(seed, sender);
    }
    (
        ControlledWorkers {
            fixture,
            capacity,
            ready,
            controls,
            active: AtomicUsize::new(0),
            peak: AtomicUsize::new(0),
            seeds: Default::default(),
        },
        received,
        senders,
    )
}
fn worker_ready(
    receiver: &std::sync::mpsc::Receiver<(u64, std::path::PathBuf)>,
) -> (u64, std::path::PathBuf) {
    receiver
        .recv_timeout(WORKER_WAIT)
        .expect("worker did not start")
}
/// The exploration directory three levels above one of its run directories.
fn batch_of(run: &std::path::Path) -> &std::path::Path {
    run.parent().unwrap().parent().unwrap().parent().unwrap()
}

#[test]
fn parallel_completion_keeps_seed_indexes_and_one_full_closure_with_serial_equivalence() {
    use seedbox::exploration::ExploreOptions;
    let fixture = worker_fixture();
    let (backend, ready, senders) = controlled_workers(&fixture, 10, 6, 3);
    let batch = std::thread::scope(|scope| {
        let task = scope.spawn(|| {
            fixture
                .orchestrator
                .explore(
                    &fixture.simulation,
                    ExploreOptions {
                        seed: Some(10),
                        count: 6,
                        workers: 3,
                        ..Default::default()
                    },
                    &fixture.preparer,
                    &backend,
                )
                .unwrap()
        });
        let started = [
            worker_ready(&ready),
            worker_ready(&ready),
            worker_ready(&ready),
        ];
        let mut initial: Vec<_> = started.iter().map(|(seed, _)| *seed).collect();
        initial.sort();
        assert_eq!(initial, vec![10, 11, 12]);
        let partial = seedbox::exploration::inspect(batch_of(&started[0].1)).unwrap();
        assert_eq!(partial.workers, 3);
        assert_eq!(partial.peak_running, 3);
        assert_eq!(partial.counts.unfinished, 3);
        assert_eq!(partial.undispatched_count, 3);
        assert!(partial.attempts.iter().all(|a| !a.finished));
        // Preparation sources can vanish: each later dispatch must use the same
        // already captured bytes/profile/configuration rather than rebuilding.
        fs::remove_file(&fixture.simulation.source).unwrap();
        for seed in 12..15 {
            senders[&seed].send(WorkerControl::Complete).unwrap();
            assert_eq!(worker_ready(&ready).0, seed + 1);
        }
        for seed in [15, 11, 10] {
            senders[&seed].send(WorkerControl::Complete).unwrap();
        }
        task.join().unwrap()
    });
    assert_eq!(fixture.preparer.calls.load(Ordering::SeqCst), 1);
    assert_eq!(backend.peak.load(Ordering::SeqCst), 3);
    assert_eq!(backend.active.load(Ordering::SeqCst), 0);
    assert_eq!(batch.counts.pass, 3);
    assert_eq!(batch.counts.fail, 3);
    assert_eq!(batch.undispatched_count, 0);
    assert_eq!(batch.outcome, Outcome::Fail);
    assert_eq!(
        batch
            .attempts
            .iter()
            .map(|a| (a.index, a.seed))
            .collect::<Vec<_>>(),
        (0..6).map(|index| (index, 10 + index)).collect::<Vec<_>>()
    );
    assert!(batch.attempts.iter().all(|a| a.finished));
    let duration: f64 = batch
        .attempts
        .iter()
        .map(|a| a.elapsed_seconds.unwrap())
        .sum();
    assert!((duration - batch.active_attempt_seconds).abs() < 1e-9);
    assert!(batch.active_attempt_seconds >= batch.attempts[0].elapsed_seconds.unwrap());
    let (serial, _serial_ready, permits) = controlled_workers(&fixture, 10, 6, 1);
    for sender in permits.values() {
        sender.send(WorkerControl::Complete).unwrap();
    }
    let serial_batch = fixture
        .orchestrator
        .explore(
            &fixture.simulation,
            ExploreOptions {
                seed: Some(10),
                count: 6,
                ..Default::default()
            },
            &fixture.preparer,
            &serial,
        )
        .unwrap();
    assert_eq!(serial_batch.workers, 1);
    assert_eq!(serial.peak.load(Ordering::SeqCst), 1);
    for (a, b) in batch.attempts.iter().zip(&serial_batch.attempts) {
        let parallel = inspect(a.directory.as_ref().unwrap()).unwrap();
        let serial = inspect(b.directory.as_ref().unwrap()).unwrap();
        assert_eq!(parallel.identity, serial.identity);
        assert_eq!(parallel.run_id, serial.run_id);
        assert_eq!(parallel.report, serial.report);
        assert_eq!(parallel.outcome, serial.outcome);
        assert_eq!(
            parallel.performance_parameters,
            serial.performance_parameters
        );
        assert_eq!(parallel.identity.inputs, fixture.preparer.inputs);
    }
}

#[test]
fn parallel_cleanup_uncertainty_cancels_owned_sibling_without_dispatching_pending_seeds() {
    use seedbox::exploration::ExploreOptions;
    let fixture = worker_fixture();
    let caller = CancellationToken::default();
    let (backend, ready, senders) = controlled_workers(&fixture, 10, 5, 2);
    let batch = std::thread::scope(|scope| {
        let task = scope.spawn(|| {
            fixture
                .orchestrator
                .explore(
                    &fixture.simulation,
                    ExploreOptions {
                        seed: Some(10),
                        count: 5,
                        workers: 2,
                        cancellation: caller.clone(),
                    },
                    &fixture.preparer,
                    &backend,
                )
                .unwrap()
        });
        worker_ready(&ready);
        worker_ready(&ready);
        senders[&11]
            .send(WorkerControl::WaitForCancellation)
            .unwrap();
        senders[&10].send(WorkerControl::CleanupUnverified).unwrap();
        task.join().unwrap()
    });
    assert!(
        !caller.is_cancelled(),
        "internal failure must not modify caller token"
    );
    assert_eq!(backend.active.load(Ordering::SeqCst), 0);
    assert_eq!(backend.seeds.lock().unwrap().len(), 2);
    assert_eq!(batch.counts.error, 1);
    assert_eq!(batch.counts.incomplete, 1);
    assert_eq!(batch.undispatched_count, 3);
    assert_eq!(batch.outcome, Outcome::Error);
    assert!(batch.attempts[0]
        .error
        .as_ref()
        .unwrap()
        .contains("cleanup-unverified"));
    assert_eq!(
        inspect(batch.attempts[1].directory.as_ref().unwrap())
            .unwrap()
            .report
            .unwrap()
            .reason,
        StopReason::Cancelled
    );
    assert!(batch.finished && batch.attempts.iter().all(|a| a.finished));
}

#[test]
fn parallel_staggered_external_cancellation_keeps_completed_running_and_pending_attribution() {
    use seedbox::exploration::ExploreOptions;
    let fixture = worker_fixture();
    let caller = CancellationToken::default();
    let (backend, ready, senders) = controlled_workers(&fixture, 10, 6, 2);
    let batch = std::thread::scope(|scope| {
        let task = scope.spawn(|| {
            fixture
                .orchestrator
                .explore(
                    &fixture.simulation,
                    ExploreOptions {
                        seed: Some(10),
                        count: 6,
                        workers: 2,
                        cancellation: caller.clone(),
                    },
                    &fixture.preparer,
                    &backend,
                )
                .unwrap()
        });
        worker_ready(&ready);
        worker_ready(&ready);
        senders[&10].send(WorkerControl::Complete).unwrap();
        let (seed, directory) = worker_ready(&ready);
        assert_eq!(seed, 12);
        let partial = seedbox::exploration::inspect(batch_of(&directory)).unwrap();
        assert_eq!(partial.attempts[0].outcome, Some(Outcome::Pass));
        assert!(partial.attempts[0].finished);
        assert!(!partial.attempts[1].finished && !partial.attempts[2].finished);
        assert_eq!(partial.undispatched_count, 3);
        caller.cancel();
        for seed in [11, 12] {
            senders[&seed]
                .send(WorkerControl::WaitForCancellation)
                .unwrap();
        }
        task.join().unwrap()
    });
    assert_eq!(backend.active.load(Ordering::SeqCst), 0);
    assert_eq!(batch.counts.pass, 1);
    assert_eq!(batch.counts.incomplete, 2);
    assert_eq!(batch.undispatched_count, 3);
    assert_eq!(batch.outcome, Outcome::Incomplete);
    for attempt in &batch.attempts[1..] {
        assert_eq!(
            inspect(attempt.directory.as_ref().unwrap())
                .unwrap()
                .report
                .unwrap()
                .reason,
            StopReason::Cancelled
        );
    }
}

#[test]
fn parallel_checkpoint_failure_unwinds_owned_work_and_preserves_independent_manifests() {
    use seedbox::exploration::ExploreOptions;
    let fixture = worker_fixture();
    let (backend, ready, senders) = controlled_workers(&fixture, 10, 5, 2);
    let paths = std::thread::scope(|scope| {
        let task = scope.spawn(|| {
            fixture.orchestrator.explore(
                &fixture.simulation,
                ExploreOptions {
                    seed: Some(10),
                    count: 5,
                    workers: 2,
                    ..Default::default()
                },
                &fixture.preparer,
                &backend,
            )
        });
        let paths = [worker_ready(&ready), worker_ready(&ready)];
        senders[&11]
            .send(WorkerControl::WaitForCancellation)
            .unwrap();
        senders[&10].send(WorkerControl::BreakCheckpoint).unwrap();
        assert!(task.join().unwrap().is_err());
        paths
    });
    assert_eq!(backend.active.load(Ordering::SeqCst), 0);
    assert_eq!(backend.seeds.lock().unwrap().len(), 2);
    let before: seedbox::exploration::Exploration =
        serde_json::from_slice(&fs::read(batch_of(&paths[0].1).join("before.json")).unwrap())
            .unwrap();
    assert!(!before.finished);
    assert_eq!(before.outcome, Outcome::Incomplete);
    assert_eq!(before.undispatched_count, 3);
    assert!(before.attempts.iter().all(|a| !a.finished));
    for (seed, path) in paths {
        let record = inspect(&path).unwrap();
        assert_eq!(record.identity.seed, seed);
        assert_eq!(
            record.outcome,
            if seed == 10 {
                Outcome::Pass
            } else {
                Outcome::Incomplete
            }
        );
    }
}

#[test]
fn worker_capacity_and_serial_default_refuse_before_preparation_and_bounds_are_lazy() {
    use seedbox::exploration::ExploreOptions;
    let fixture = Fixture::new();
    for workers in [0, 33] {
        assert!(fixture
            .orchestrator
            .explore(
                &fixture.simulation,
                ExploreOptions {
                    workers,
                    ..Default::default()
                },
                &fixture.preparer,
                &fixture.backend
            )
            .is_err());
    }
    assert!(!fixture.directory.path().join("runs").exists());
    let unsupported = fixture
        .orchestrator
        .explore(
            &fixture.simulation,
            ExploreOptions {
                workers: 2,
                ..Default::default()
            },
            &fixture.preparer,
            &fixture.backend,
        )
        .unwrap();
    assert!(unsupported.error.unwrap().contains("only one worker"));
    let (backend, _, _) = controlled_workers(&fixture, 10, 2, 1);
    let capacity = fixture
        .orchestrator
        .explore(
            &fixture.simulation,
            ExploreOptions {
                seed: Some(10),
                count: 2,
                workers: 2,
                ..Default::default()
            },
            &fixture.preparer,
            &backend,
        )
        .unwrap();
    assert_eq!(capacity.outcome, Outcome::Error);
    assert_eq!(capacity.undispatched_count, 2);
    assert!(capacity.attempts.is_empty() && capacity.inputs.is_none());
    assert_eq!(fixture.preparer.calls.load(Ordering::SeqCst), 0);
    assert_eq!(fixture.backend.calls.load(Ordering::SeqCst), 0);
    // The concrete backend rejects pinned parallel requests before touching host
    // admission, even on a machine that cannot qualify its fake engine.
    let engine = fixture.directory.path().join("fake-engine");
    fs::write(&engine, b"fixture engine").unwrap();
    let executor = seedbox::vmm::VmmExecutor::new(
        &engine,
        &ContentStore::new(fixture.directory.path().join("store")),
        Some(4),
        512,
        WitnessMode::Exits,
    )
    .unwrap();
    assert!(executor
        .preflight_workers(2)
        .unwrap_err()
        .to_string()
        .contains("omit --cpu"));
}

#[test]
fn checkpoint_version_refusal_preserves_old_bytes_and_ordinary_run_inspection() {
    let fixture = Fixture::new();
    let batch = fixture
        .orchestrator
        .explore(
            &fixture.simulation,
            Default::default(),
            &fixture.preparer,
            &fixture.backend,
        )
        .unwrap();
    let path = batch.directory.join("exploration.json");
    let v2 = fs::read(&path).unwrap();
    for version in [1, 3] {
        let mut old: serde_json::Value = serde_json::from_slice(&v2).unwrap();
        old["version"] = version.into();
        if version == 1 {
            for name in ["workers", "peak_running", "active_attempt_seconds"] {
                old.as_object_mut().unwrap().remove(name);
            }
            for attempt in old["attempts"].as_array_mut().unwrap() {
                attempt.as_object_mut().unwrap().remove("index");
                attempt.as_object_mut().unwrap().remove("finished");
            }
        }
        let bytes = serde_json::to_vec(&old).unwrap();
        fs::write(&path, &bytes).unwrap();
        assert!(seedbox::exploration::inspect(&batch.directory)
            .unwrap_err()
            .to_string()
            .contains("unsupported exploration checkpoint version"));
        assert_eq!(fs::read(&path).unwrap(), bytes);
        assert_eq!(
            inspect(batch.attempts[0].directory.as_ref().unwrap())
                .unwrap()
                .outcome,
            Outcome::Pass
        );
    }
    fs::write(&path, v2).unwrap();
    let read = seedbox::exploration::inspect(&batch.directory).unwrap();
    assert_eq!(read.version, 2);
    assert_eq!(read.workers, 1);
    assert_eq!(read.attempts[0].index, 0);
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
        fixture.replay(&on).record.replay.unwrap().status,
        ReplayStatus::Match
    );
    assert_eq!(fixture.replay(&off).record.outcome, Outcome::Error);
    fixture.backend.profile.semantic_parameters.torn_writes = false;
    assert_eq!(fixture.replay(&on).record.outcome, Outcome::Error);
    assert_eq!(
        fixture.replay(&off).record.replay.unwrap().status,
        ReplayStatus::Match
    );
}

#[test]
fn checkpoint_identity_replays_retained_semantics() {
    let mut fixture = Fixture::new();
    let parent = fixture.run();
    fixture.backend.profile.semantic_parameters.checkpoint = Some(Checkpoint {
        at_ns: 500,
        branch_seed: 42,
    });
    fixture.preparer.inputs.profile = fixture.backend.profile.clone();
    let branch = fixture.run();
    assert_eq!(branch.record.outcome, Outcome::Pass);
    assert_ne!(branch.record.run_id, parent.record.run_id);
    let replay = fixture.replay(&branch);
    assert_eq!(replay.record.run_id, branch.record.run_id);
    assert_eq!(replay.record.replay.unwrap().status, ReplayStatus::Match);
    fixture.backend.profile.semantic_parameters.checkpoint = Some(Checkpoint {
        at_ns: 500,
        branch_seed: 43,
    });
    fixture.preparer.inputs.profile = fixture.backend.profile.clone();
    let other = fixture.run();
    assert_ne!(other.record.run_id, branch.record.run_id);
}

#[test]
fn kvm_executor_profile_records_only_its_settings_and_replays() {
    use seedbox::vmm::VmmExecutor;
    let mut f = Fixture::new();
    let store = ContentStore::new(f.directory.path().join("store"));
    let engine = f.directory.path().join("engine");
    fs::write(&engine, b"fixture engine").unwrap();
    let executor = VmmExecutor::new(&engine, &store, None, 1024, WitnessMode::Exits).unwrap();
    assert_eq!(
        executor.profile().semantic_parameters,
        SemanticParameters::new(1024)
    );
    f.backend.profile = executor.profile();
    f.preparer.inputs.profile = executor.profile();
    let before = f.run();
    let json = serde_json::to_value(&before.record.identity.inputs.profile).unwrap();
    assert!(json["semantic_parameters"].get("timebase").is_none());
    assert!(json["semantic_parameters"].get("clock").is_none());
    let replay = f
        .orchestrator
        .replay(&before.directory, &f.backend, &CancellationToken::default())
        .unwrap();
    assert_eq!(replay.record.replay.unwrap().status, ReplayStatus::Match);
    assert_eq!(replay.record.run_id, before.record.run_id);
}

#[test]
fn replay_refuses_settings_drift_before_execution() {
    let page_timer = |p: &mut SemanticParameters| p.page_timer = true;
    let torn = |p: &mut SemanticParameters| p.torn_writes = true;
    let none = |_: &mut SemanticParameters| {};
    type Edit<'a> = &'a dyn Fn(&mut SemanticParameters);
    let cases: [(Edit, Edit, &str); 2] = [
        (
            &page_timer,
            &none,
            "executor page_timer does not match the recorded run",
        ),
        (
            &none,
            &torn,
            "executor torn_writes does not match the recorded run",
        ),
    ];
    for (recorded, requested, expected) in cases {
        let mut f = Fixture::new();
        f.backend.profile.backend = execution::Backend::Kvm;
        recorded(&mut f.backend.profile.semantic_parameters);
        f.preparer.inputs.profile = f.backend.profile.clone();
        let run = f.run();
        f.backend.profile.semantic_parameters = SemanticParameters::new(512);
        requested(&mut f.backend.profile.semantic_parameters);
        for allow_engine_change in [false, true] {
            let error = f
                .orchestrator
                .replay_with_options(
                    &run.directory,
                    &f.backend,
                    ReplayOptions {
                        allow_engine_change,
                        ..Default::default()
                    },
                )
                .unwrap_err()
                .to_string();
            assert!(error.contains(expected), "{error}");
            assert_eq!(f.backend.calls.load(Ordering::SeqCst), 1);
            assert_eq!(
                fs::read_dir(f.directory.path().join("runs"))
                    .unwrap()
                    .count(),
                1
            );
        }
    }
}

#[test]
fn page_timer_is_recorded_only_when_enabled_and_replays_from_the_profile() {
    use seedbox::{engine_adapter::recorded_page_timer, vmm::VmmExecutor};
    let mut f = Fixture::new();
    let store = ContentStore::new(f.directory.path().join("store"));
    let engine = f.directory.path().join("engine");
    fs::write(&engine, b"fixture engine").unwrap();
    let executor = VmmExecutor::new(&engine, &store, None, 1024, WitnessMode::Exits).unwrap();
    f.backend.profile = executor.profile();
    f.preparer.inputs.profile = executor.profile();
    let before = f.run();
    for off in [
        executor.clone().with_page_timer(false),
        executor
            .clone()
            .with_page_timer(true)
            .with_page_timer(false),
    ] {
        f.backend.profile = off.profile();
        f.preparer.inputs.profile = off.profile();
        let run = f.run();
        assert_eq!(run.record.run_id, before.record.run_id);
        assert_eq!(
            serde_json::to_vec(&run.record.identity).unwrap(),
            serde_json::to_vec(&before.record.identity).unwrap()
        );
        assert!(!recorded_page_timer(&off.profile()));
    }
    let on = executor.with_page_timer(true);
    f.backend.profile = on.profile();
    f.preparer.inputs.profile = on.profile();
    let run = f.run();
    assert_ne!(run.record.run_id, before.record.run_id);
    assert!(
        run.record
            .identity
            .inputs
            .profile
            .semantic_parameters
            .page_timer
    );
    assert!(recorded_page_timer(
        &inspect(&run.directory).unwrap().identity.inputs.profile
    ));
    assert_eq!(
        f.replay(&run).record.replay.unwrap().status,
        ReplayStatus::Match
    );
}

#[test]
fn deferred_tx_is_semantic_only_when_on_and_replay_cannot_switch_modes() {
    use seedbox::vmm::VmmExecutor;
    let mut f = Fixture::new();
    let store = ContentStore::new(f.directory.path().join("store"));
    let engine = f.directory.path().join("engine");
    fs::write(&engine, b"fixture engine").unwrap();
    let off = VmmExecutor::new(&engine, &store, None, 1024, WitnessMode::Exits).unwrap();
    assert_eq!(off.profile(), off.clone().with_deferred_tx(false).profile());
    let on = off.clone().with_deferred_tx(true);
    assert!(on.profile().semantic_parameters.deferred_tx);
    assert_eq!(on.clone().with_deferred_tx(false).profile(), off.profile());
    let mut ids = Vec::new();
    for recorded in [off.clone(), on.clone()] {
        f.backend.profile = recorded.profile();
        f.preparer.inputs.profile = recorded.profile();
        let run = f.run();
        let replay = f
            .orchestrator
            .replay(&run.directory, &f.backend, &CancellationToken::default())
            .unwrap();
        assert_eq!(replay.record.replay.unwrap().status, ReplayStatus::Match);
        assert_eq!(replay.record.run_id, run.record.run_id);
        ids.push(run.record.run_id);
        let enabled = seedbox::engine_adapter::recorded_deferred_tx(&recorded.profile()).unwrap();
        f.backend.profile = off.clone().with_deferred_tx(!enabled).profile();
        for allow_engine_change in [false, true] {
            let error = f
                .orchestrator
                .replay_with_options(
                    &run.directory,
                    &f.backend,
                    ReplayOptions {
                        allow_engine_change,
                        ..Default::default()
                    },
                )
                .unwrap_err()
                .to_string();
            assert!(error.contains("--deferred-tx"));
        }
    }
    assert_ne!(ids[0], ids[1]);
}

#[test]
fn materialize_trusts_only_unchanged_objects_this_store_hashed() {
    let directory = tempfile::tempdir().unwrap();
    let store = ContentStore::new(directory.path().join("store"));
    let digest = store.put(b"retained guest bytes").unwrap();
    let object = directory.path().join("store/sha256").join(digest.hex());
    // Hashed by put, so this copy skips the hash and keeps the bytes.
    let copy = store
        .materialize(&digest, &directory.path().join("a"))
        .unwrap();
    assert_eq!(fs::read(copy).unwrap(), b"retained guest bytes");
    // A same-length rewrite changes the object's ctime, so it is hashed again.
    std::thread::sleep(std::time::Duration::from_millis(20));
    fs::write(&object, b"corrupted guest bytes").unwrap();
    fs::write(&object, b"retained guest byteX").unwrap();
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
            vec![&inputs.profile.engine, &inputs.kernel]
        }
        fn execute(&self, request: ExecutionRequest<'_>) -> Result<ExecutionReport> {
            let inputs = &self.0.preparer.inputs;
            assert_eq!(request.artifacts.keys().collect::<Vec<_>>(), {
                let mut keys = vec![&inputs.profile.engine, &inputs.kernel];
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
fn materialize_linked_shares_one_copy_and_replaces_a_changed_one() {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let directory = tempfile::tempdir().unwrap();
    let store = ContentStore::new(directory.path().join("store"));
    let digest = store.put(b"retained guest bytes").unwrap();
    let shared = directory.path().join("shared");
    let a = store
        .materialize_linked(&digest, &shared, &directory.path().join("a"))
        .unwrap();
    let b = store
        .materialize_linked(&digest, &shared, &directory.path().join("b"))
        .unwrap();
    let copy = shared.join(digest.hex());
    assert_eq!(
        fs::metadata(&a).unwrap().ino(),
        fs::metadata(&b).unwrap().ino()
    );
    assert_eq!(
        fs::metadata(&copy).unwrap().ino(),
        fs::metadata(&a).unwrap().ino()
    );
    assert_eq!(
        fs::metadata(&copy).unwrap().permissions().mode() & 0o777,
        0o500
    );
    // A changed shared copy is replaced, never linked again.
    fs::set_permissions(&copy, fs::Permissions::from_mode(0o700)).unwrap();
    fs::write(&copy, b"tampered guest bytes").unwrap();
    let c = store
        .materialize_linked(&digest, &shared, &directory.path().join("c"))
        .unwrap();
    assert_eq!(fs::read(&c).unwrap(), b"retained guest bytes");
    assert_ne!(
        fs::metadata(&c).unwrap().ino(),
        fs::metadata(&a).unwrap().ino()
    );
    // A changed store object is hashed again and fails.
    let object = directory.path().join("store/sha256").join(digest.hex());
    fs::write(&object, b"retained guest byteX").unwrap();
    assert!(store
        .materialize_linked(&digest, &shared, &directory.path().join("d"))
        .is_err());
}

#[test]
fn exploration_attempts_link_one_shared_copy_of_each_input() {
    use seedbox::exploration::ExploreOptions;
    use std::os::unix::fs::MetadataExt;
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
            &fixture.backend,
        )
        .unwrap();
    assert_eq!(batch.outcome, Outcome::Pass);
    let shared = batch.directory.join("inputs");
    let shared_count = fs::read_dir(&shared).unwrap().count();
    assert!(shared_count > 0);
    for attempt in &batch.attempts {
        let inputs = attempt.directory.as_ref().unwrap().join("inputs");
        let mut count = 0;
        for entry in fs::read_dir(inputs).unwrap() {
            let entry = entry.unwrap();
            let copy = shared.join(entry.file_name());
            assert_eq!(
                entry.metadata().unwrap().ino(),
                fs::metadata(copy).unwrap().ino()
            );
            count += 1;
        }
        assert_eq!(count, shared_count);
    }
}

#[test]
fn put_file_of_a_retained_object_reads_it_without_rewriting_it() {
    use std::os::unix::fs::MetadataExt;
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
    let first = seedbox::admission::file_digest(&source).unwrap();
    assert_eq!(seedbox::admission::file_digest(&source).unwrap(), first);
    let store = ContentStore::new(directory.path().join("store"));
    assert_eq!(store.put_file(&source).unwrap(), first);
    // Rewritten in place, and replaced by a new file under the same name.
    fs::write(&source, b"second engine bytes").unwrap();
    let second = seedbox::admission::file_digest(&source).unwrap();
    assert_eq!(second, ContentDigest::of(b"second engine bytes"));
    assert_eq!(store.put_file(&source).unwrap(), second);
    let replacement = directory.path().join("replacement");
    fs::write(&replacement, b"third").unwrap();
    fs::rename(&replacement, &source).unwrap();
    assert_eq!(
        seedbox::admission::file_digest(&source).unwrap(),
        ContentDigest::of(b"third")
    );
}
