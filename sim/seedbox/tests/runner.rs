use seedbox::{
    execution::*,
    fixture::{self, Fixture, Frozen},
    orchestration::*,
    runner::*,
    store::{ContentDigest, ContentStore},
    vmm::VmmExecutor,
    Error, Result,
};
use std::{
    collections::BTreeSet,
    path::PathBuf,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Mutex,
    },
    time::Duration,
};

struct Rig {
    dir: tempfile::TempDir,
    frozen: Frozen,
    store: ContentStore,
    calls: Mutex<Vec<(u64, String)>>,
    active: AtomicUsize,
    peak: AtomicUsize,
    failure: Option<&'static str>,
}
impl Rig {
    fn new(failure: Option<&'static str>) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path();
        std::fs::write(path.join("kernel"), b"kernel").unwrap();
        std::fs::write(path.join("archive"), b"archive").unwrap();
        std::fs::write(path.join("engine"), b"engine").unwrap();
        std::fs::write(path.join("world.json"), r#"{
            "kernel":"kernel","mem_mb":64,"duration_ns":1000000,"end_on_exit":"a",
            "machines":[{"name":"z","initrd":"archive","cmdline":"keep order"},{"name":"a","initrd":"archive"}],
            "scenario":[{"at_ns":2,"machine":"z","serial":"stop\\n"}],
            "kill_restart":{"members":["z"],"start_ns":1,"end_ns":100,"gap_ns":[1,2],"down_ns":[1,2]}
        }"#).unwrap();
        let store = ContentStore::new(path.join("store"));
        let fixture = Fixture::load(&path.join("world.json"), None, 1_000_000_000).unwrap();
        let executor = VmmExecutor::new(&path.join("engine"), &store, 2, 64, WitnessMode::Exits)
            .unwrap()
            .with_fixture();
        let frozen = fixture.prepare(executor.profile(), &store).unwrap();
        Self {
            dir,
            frozen,
            store,
            calls: Mutex::new(vec![]),
            active: AtomicUsize::new(0),
            peak: AtomicUsize::new(0),
            failure,
        }
    }
}
struct Backend<'a> {
    rig: &'a Rig,
    job: &'a Job,
}
impl ExecutionBackend for Backend<'_> {
    fn profile(&self) -> ExecutionProfile {
        self.rig.frozen.inputs.profile.clone()
    }
    fn execute(&self, req: ExecutionRequest<'_>) -> Result<ExecutionReport> {
        let world = fixture::world(&req)?;
        assert_eq!(world["machines"][0]["name"], "z");
        assert_eq!(world["machines"][0]["cmdline"], "keep order");
        assert_eq!(world["scenario"][0]["at_ns"], 2);
        assert_eq!(world["kill_restart"]["members"][0], "z");
        assert!(PathBuf::from(world["kernel"].as_str().unwrap()).exists());
        match self.rig.failure {
            Some("overshoot") if self.job.margin != "98304" => {
                return Err(Error::Overshoot("injected".into()))
            }
            Some("always overshoot") => return Err(Error::Overshoot("injected".into())),
            Some("clock bound") => return Err(Error::Execution("clock bound exceeded".into())),
            Some("busy") => return Err(Error::LeaseBusy(2)),
            _ => (),
        }
        Ok(ExecutionReport {
            reason: StopReason::Completed,
            workload_exit: Some(WorkloadExit {
                service: "a".into(),
                code: u8::from(self.rig.failure == Some("application")),
            }),
            witness: Some(ContentDigest::of(b"witness")),
            full_state: None,
            faults: vec![],
            diagnostics: None,
            detail: None,
        })
    }
}
impl JobRunner for Rig {
    fn run(&self, job: &Job, _: u32, cancel: &CancellationToken) -> Result<Run> {
        self.calls
            .lock()
            .unwrap()
            .push((job.seed, job.margin.clone()));
        let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
        self.peak.fetch_max(active, Ordering::SeqCst);
        std::thread::sleep(Duration::from_millis(2));
        let result = Orchestrator::new(self.store.clone(), self.dir.path().join("runs")).run(
            &self.frozen.simulation,
            RunOptions {
                seed: Some(job.seed),
                cancellation: cancel.clone(),
            },
            &self.frozen,
            &Backend { rig: self, job },
        );
        self.active.fetch_sub(1, Ordering::SeqCst);
        result
    }
}
#[test]
fn retained_fixture_survives_source_removal_and_replays_without_oci() {
    let rig = Rig::new(None);
    for name in ["kernel", "archive", "engine", "world.json"] {
        std::fs::remove_file(rig.dir.path().join(name)).unwrap();
    }
    assert!(rig.frozen.inputs.images.is_empty());
    let job = Job::new("fixture", 9);
    let run = rig.run(&job, 2, &CancellationToken::default()).unwrap();
    assert_eq!(run.record.outcome, Outcome::Pass);
    let replay = Orchestrator::new(rig.store.clone(), rig.dir.path().join("runs"))
        .replay(
            &run.directory,
            &Backend {
                rig: &rig,
                job: &job,
            },
            &CancellationToken::default(),
        )
        .unwrap();
    assert_eq!(replay.record.replay.unwrap().status, ReplayStatus::Match);
    assert_ne!(run.directory, replay.directory);
}
#[test]
fn scheduler_bounds_concurrency_preserves_order_and_keeps_duplicate_seed_attempts() {
    let rig = Rig::new(None);
    let jobs: Vec<_> = [9, 2, 2, 8, 1]
        .into_iter()
        .map(|s| Job::new("fixture", s))
        .collect();
    let runs = dispatch(
        &jobs,
        &[2, 3],
        &rig,
        &CancellationToken::default(),
        Duration::ZERO,
    );
    assert_eq!(
        runs.iter().map(|r| r.job.seed).collect::<Vec<_>>(),
        vec![9, 2, 2, 8, 1]
    );
    assert!(runs.iter().all(JobResult::completed));
    assert_eq!(
        runs.iter()
            .flat_map(|r| &r.attempts)
            .collect::<BTreeSet<_>>()
            .len(),
        5
    );
    assert_eq!(rig.peak.load(Ordering::SeqCst), 2);
}
#[test]
fn only_typed_overshoot_retries_once_with_fresh_evidence() {
    for (failure, expected, complete) in [
        ("overshoot", 2, true),
        ("always overshoot", 2, false),
        ("clock bound", 1, false),
        ("busy", 1, false),
    ] {
        let rig = Rig::new(Some(failure));
        let runs = dispatch(
            &[Job::new("fixture", 1)],
            &[2],
            &rig,
            &CancellationToken::default(),
            Duration::ZERO,
        );
        assert_eq!(runs[0].attempts.len(), expected, "{failure}");
        assert_eq!(runs[0].completed(), complete, "{failure}");
        if expected == 2 {
            assert_ne!(runs[0].attempts[0], runs[0].attempts[1]);
            assert_eq!(rig.calls.lock().unwrap()[1].1, "98304");
            assert_eq!(
                inspect(&runs[0].attempts[0]).unwrap().outcome,
                Outcome::Error
            );
        }
    }
}
#[test]
fn cancelled_jobs_never_enter_execution() {
    let rig = Rig::new(None);
    let cancel = CancellationToken::default();
    cancel.cancel();
    let runs = dispatch(
        &[Job::new("fixture", 1)],
        &[2],
        &rig,
        &cancel,
        Duration::ZERO,
    );
    assert!(!runs[0].completed());
    assert!(runs[0].attempts.is_empty());
    assert!(rig.calls.lock().unwrap().is_empty());
}
#[test]
fn campaign_honors_exact_limit_and_stops_on_failures_or_infrastructure() {
    for (failure, keep, expected) in [
        (None, false, 5),
        (Some("application"), false, 2),
        (Some("application"), true, 5),
        (Some("clock bound"), true, 2),
    ] {
        let rig = Rig::new(failure);
        let mut retained = vec![];
        let report = campaign(
            &Job::new("fixture", 10),
            &CampaignOptions {
                first_seed: 10,
                max_seeds: 5,
                time_budget: Duration::from_secs(10),
                continue_after_failure: keep,
                lease_wait: Duration::ZERO,
            },
            &[2, 3],
            &rig,
            &CancellationToken::default(),
            |r| {
                retained.extend(r.iter().map(|r| r.job.seed));
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(report.attempted, expected);
        assert_eq!(retained, (10..10 + expected).collect::<Vec<_>>());
        assert_eq!(
            report.unresolved,
            if failure == Some("clock bound") { 2 } else { 0 }
        );
    }
}
#[test]
fn cpu_topology_rejects_overlapping_siblings_before_dispatch() {
    assert_eq!(cpu_list("2,4-5\n").unwrap(), vec![2, 4, 5]);
    for bad in ["", "2,2", "2-1", "1024"] {
        assert!(cpu_list(bad).is_err());
    }
    assert!(validate_cpus(&[2, 18], |_| Ok(BTreeSet::from([2, 18]))).is_err());
    assert!(validate_cpus(&[2, 3], |c| Ok(BTreeSet::from([c, c + 16]))).is_ok());
}

#[test]
fn runner_panics_are_unresolved_and_do_not_drop_remaining_jobs() {
    struct Panic;
    impl JobRunner for Panic {
        fn run(&self, _: &Job, _: u32, _: &CancellationToken) -> Result<Run> {
            panic!("injected");
        }
    }
    let results = dispatch(
        &[Job::new("fixture", 1), Job::new("fixture", 2)],
        &[2],
        &Panic,
        &CancellationToken::default(),
        Duration::ZERO,
    );
    assert_eq!(results.len(), 2);
    assert!(results
        .iter()
        .all(|r| !r.completed() && r.error.as_ref().unwrap().contains("panicked")));
}
