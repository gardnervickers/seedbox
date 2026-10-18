//! Streaming whole-world capture, using the existing non-Clone mock and codec.
use super::*;
use std::sync::Mutex;

fn encode_saved(saved: Saved, store: &mut Store) -> SnapshotReference {
    let mut e = Encoder { output: Vec::new() };
    saved.0.put(&mut e).unwrap();
    let bytes = e.output;
    let digest = Sha256::digest(&bytes).into();
    let reference = SnapshotReference::new(digest, bytes.len() as u64);
    store.insert(digest, bytes);
    reference
}

/// Compare the original route, identity mapping and immediate codec mapping.
/// Restore only after destroying the source and all original native snapshots.
fn exercise_streaming(mut d: Driver<Mock>) {
    let before = status(&mut d);
    let expected_artifact = artifact(d.checkpoint().unwrap());
    let identity = d.checkpoint_map(Ok::<_, Io>).unwrap();
    assert_eq!(artifact(identity), expected_artifact);
    let drops = Arc::new(AtomicUsize::new(0));
    let count = d.execution.machines.len();
    for entry in &mut d.execution.machines {
        entry.machine.as_mut().unwrap().saved_drops = Some(drops.clone());
    }
    let mut store = Store::new();
    let refs = d
        .checkpoint_map(|saved| Ok::<_, Io>(encode_saved(saved, &mut store)))
        .unwrap();
    assert_eq!(drops.load(Ordering::SeqCst), count);
    let mut bytes = Vec::new();
    refs.write_to(&mut bytes).unwrap();
    drop(refs);
    assert_eq!((bytes.clone(), store.clone()), expected_artifact);
    let expected = continue_from(&mut d);
    drop(d);
    let reusable = decode(&bytes, &store);
    let branches: Vec<_> = [1, 4]
        .into_iter()
        .map(|workers| restore(&reusable, workers))
        .collect();
    drop(reusable);
    for mut b in branches {
        assert_eq!(status(&mut b), before);
        assert_eq!(continue_from(&mut b), expected);
    }
}

#[test]
fn streamed_nonclone_snapshots_keep_bytes_rng_publications_and_future_closure() {
    for drain in [false, true] {
        let mut d = build(
            vec![
                spec(0, true, vec![(0, Response::Ready)]),
                spec(1, true, vec![(0, Response::Ready)]),
                spec(2, true, vec![(0, Response::Ready)]),
            ],
            (3..5)
                .map(|i| spec(i, true, vec![(0, Response::Ready)]))
                .collect(),
            true,
        );
        for (time, name) in [(25, "fixture-3"), (26, "fixture-4")] {
            d.schedule(at(time), key(name), Action::Spawn { name: name.into() })
                .unwrap();
        }
        // Arbitrary fill widths [1,7,31,2,33] cross machine RNG caches;
        // three initial plus two future admissions cross the root RNG cache.
        for end in [2, 5, 9, 10, 13] {
            d.step(at(end)).unwrap();
        }
        assert!(states(&d)[0].entropy.len() > 32);
        if drain {
            d.drain_events();
        }
        d.stage(
            at(13),
            EventKind::MachineAdded {
                machine: MachineId(0),
                cause: Cause::Host,
            },
        );
        exercise_streaming(d);
    }
}

#[test]
fn streamed_capture_preserves_held_fault_and_pending_quantum_without_resuming() {
    let mut d = build(
        vec![spec(0, true, vec![]), spec(1, true, vec![])],
        vec![],
        true,
    );
    d.schedule(
        at(5),
        key("throttle"),
        Action::Throttle {
            target: Target::Machine(MachineId(1)),
            config: ExecutionConfig::new(2000).unwrap(),
        },
    )
    .unwrap();
    d.with_barrier(|b| b.pause_before_next_fault(PlaybackRate::Unlimited))
        .unwrap();
    while !matches!(d.step(at(10)).unwrap(), StepOutcome::BeforeFault { .. }) {}
    let before = status(&mut d);
    let mut store = Store::new();
    let c = d
        .checkpoint_map(|saved| Ok::<_, Io>(encode_saved(saved, &mut store)))
        .unwrap();
    let mut bytes = Vec::new();
    c.write_to(&mut bytes).unwrap();
    assert_eq!(
        (bytes.clone(), store.clone()),
        artifact(d.checkpoint().unwrap())
    );
    assert_eq!(status(&mut d), before);
    assert_eq!(
        d.execution.machines[1]
            .lifecycle
            .execution()
            .factor_permille(),
        1000
    );
    reference_observer_session(&mut d);
    d.with_barrier(|b| b.set_rate(PlaybackRate::Unlimited))
        .unwrap();
    let expected = advance(&mut d, 30);
    let expected_states = states(&d);
    drop(d);
    for workers in [1, 4] {
        let decoded = decode(&bytes, &store);
        let mut b = restore(&decoded, workers);
        assert_eq!(status(&mut b), before);
        assert!(matches!(
            b.step(at(100)).unwrap(),
            StepOutcome::BeforeFault { .. }
        ));
        b.with_barrier(|b| b.set_rate(PlaybackRate::Unlimited))
            .unwrap();
        assert_eq!(advance(&mut b, 30), expected);
        assert_eq!(states(&b), expected_states);
        assert_eq!(
            b.execution.machines[1]
                .lifecycle
                .execution()
                .factor_permille(),
            2000
        );
    }
    let mut d = build(vec![spec(0, true, vec![])], vec![], true);
    d.step(at(3)).unwrap();
    d.with_barrier(|b| {
        b.set_rate(PlaybackRate::Paused)?;
        b.step_quantum()
    })
    .unwrap();
    exercise_streaming(d);
}

#[derive(Default)]
struct Audit {
    calls: Mutex<Vec<String>>,
    live_at_capture: Mutex<Vec<usize>>,
    live: AtomicUsize,
    peak: AtomicUsize,
    native_drops: AtomicUsize,
    mapped_drops: AtomicUsize,
    runs: AtomicUsize,
    factories: AtomicUsize,
    contexts: AtomicUsize,
}
struct Tracked {
    inner: Mock,
    audit: Arc<Audit>,
}
// Neither snapshot nor mapped output implements Clone.
struct TrackedSnapshot {
    native: Saved,
    audit: Arc<Audit>,
}
impl Drop for TrackedSnapshot {
    fn drop(&mut self) {
        self.audit.live.fetch_sub(1, Ordering::SeqCst);
        self.audit.native_drops.fetch_add(1, Ordering::SeqCst);
    }
}
struct Mapped(Arc<Audit>);
impl Drop for Mapped {
    fn drop(&mut self) {
        self.0.mapped_drops.fetch_add(1, Ordering::SeqCst);
    }
}
impl Machine for Tracked {
    type Context = Context;
    type Error = Io;
    type Snapshot = TrackedSnapshot;
    fn run(&mut self, c: &mut RunContext<'_, Context>, upto: Timestamp) -> Result<StopReason, Io> {
        self.audit.runs.fetch_add(1, Ordering::SeqCst);
        self.inner.run(c, upto)
    }
    fn snapshot(&mut self) -> Result<TrackedSnapshot, Io> {
        self.audit
            .calls
            .lock()
            .unwrap()
            .push(format!("capture:{}", self.inner.state.name));
        self.audit
            .live_at_capture
            .lock()
            .unwrap()
            .push(self.audit.live.load(Ordering::SeqCst));
        let native = self.inner.snapshot()?;
        let live = self.audit.live.fetch_add(1, Ordering::SeqCst) + 1;
        self.audit.peak.fetch_max(live, Ordering::SeqCst);
        Ok(TrackedSnapshot {
            native,
            audit: self.audit.clone(),
        })
    }
}
fn tracked(count: usize) -> (Driver<Tracked>, Arc<Audit>) {
    let audit = Arc::new(Audit::default());
    let factories = audit.clone();
    let contexts = audit.clone();
    let driver = Driver::new_seeded(
        at(0),
        Duration::from_nanos(10),
        1,
        51,
        move || {
            contexts.contexts.fetch_add(1, Ordering::SeqCst);
            Ok(Context(None))
        },
        fixture::definition(0),
        (1..count).map(fixture::definition).collect(),
        vec![],
        NetworkConfig::default(),
        move |definition| {
            factories.factories.fetch_add(1, Ordering::SeqCst);
            let index = definition
                .name
                .strip_prefix("fixture-")
                .unwrap()
                .parse()
                .unwrap();
            Ok(Tracked {
                inner: Mock::new(spec(index, true, vec![])),
                audit: factories.clone(),
            })
        },
    )
    .unwrap();
    (driver, audit)
}
fn map_tracked(snapshot: TrackedSnapshot) -> Mapped {
    snapshot
        .audit
        .calls
        .lock()
        .unwrap()
        .push(format!("map:{}", snapshot.native.0.name));
    Mapped(snapshot.audit.clone())
}
fn invocation_prefix(successes: usize, final_capture: bool) -> Vec<String> {
    let mut calls = Vec::new();
    for i in 0..successes {
        calls.push(format!("capture:fixture-{i}"));
        calls.push(format!("map:fixture-{i}"));
    }
    if final_capture {
        calls.push(format!("capture:fixture-{successes}"));
    }
    calls
}

#[test]
fn immediate_mapper_can_release_native_snapshot_before_next_capture_peak_one() {
    let (mut d, audit) = tracked(5);
    let c = d.checkpoint_map(|s| Ok::<_, Io>(map_tracked(s))).unwrap();
    assert_eq!(*audit.calls.lock().unwrap(), invocation_prefix(5, false));
    assert_eq!(*audit.live_at_capture.lock().unwrap(), vec![0; 5]);
    assert_eq!(audit.peak.load(Ordering::SeqCst), 1);
    assert_eq!(audit.live.load(Ordering::SeqCst), 0);
    assert_eq!(audit.native_drops.load(Ordering::SeqCst), 5);
    assert_eq!(audit.mapped_drops.load(Ordering::SeqCst), 0);
    assert_eq!(audit.runs.load(Ordering::SeqCst), 0);
    assert_eq!(audit.factories.load(Ordering::SeqCst), 5);
    assert_eq!(audit.contexts.load(Ordering::SeqCst), 1);
    drop(c);
    assert_eq!(audit.mapped_drops.load(Ordering::SeqCst), 5);
    let (mut d, audit) = tracked(5);
    let c = d.checkpoint().unwrap();
    assert_eq!(*audit.live_at_capture.lock().unwrap(), vec![0, 1, 2, 3, 4]);
    assert_eq!(audit.peak.load(Ordering::SeqCst), 5);
    assert_eq!(audit.live.load(Ordering::SeqCst), 5);
    drop(c);
    assert_eq!(audit.live.load(Ordering::SeqCst), 0);
    assert_eq!(audit.native_drops.load(Ordering::SeqCst), 5);
}

/// Inspect cloned RNGs only, never drawing from the source to observe it.
fn cursors(d: &Driver<Tracked>) -> Vec<Vec<u8>> {
    let SimulationRng::Seeded(root) = &d.execution.rng else {
        panic!("unowned RNG")
    };
    std::iter::once(root)
        .chain(d.execution.machines.iter().map(|m| m.rng.as_ref().unwrap()))
        .map(|rng| {
            let mut copy = rng.clone();
            let mut bytes = vec![0; 137];
            copy.fill_bytes(&mut bytes);
            bytes
        })
        .collect()
}
fn tracked_states(d: &Driver<Tracked>) -> Vec<State> {
    d.execution
        .machines
        .iter()
        .map(|e| e.machine.as_ref().unwrap().inner.state.clone())
        .collect()
}
#[derive(Debug)]
struct WriterFailure(u32);
impl fmt::Display for WriterFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "writer failure {}", self.0)
    }
}
impl std::error::Error for WriterFailure {}

#[test]
fn nth_capture_or_mapper_failure_drops_partial_results_without_source_changes_or_retry() {
    for capture_failure in [true, false] {
        let (mut d, audit) = tracked(5);
        let (mut reference, _) = tracked(5);
        for end in [2, 5, 9, 10, 13] {
            d.step(at(end)).unwrap();
            reference.step(at(end)).unwrap();
        }
        let states = tracked_states(&d);
        let position = d.position();
        let rng = cursors(&d);
        let runs = audit.runs.load(Ordering::SeqCst);
        if capture_failure {
            d.execution.machines[2]
                .machine
                .as_mut()
                .unwrap()
                .inner
                .state
                .fail_capture = true;
        }
        // This external side effect deliberately survives failure: Driver owns no
        // publication/rollback policy and must not retry the writer.
        let mut external_artifacts = Vec::new();
        let error = match d.checkpoint_map(|s| {
            let index = s
                .native
                .0
                .name
                .strip_prefix("fixture-")
                .unwrap()
                .parse::<usize>()
                .unwrap();
            external_artifacts.push(index);
            if index == 2 {
                s.audit
                    .calls
                    .lock()
                    .unwrap()
                    .push(format!("map:{}", s.native.0.name));
                Err(Io::new(io::ErrorKind::PermissionDenied, WriterFailure(73)))
            } else {
                Ok(map_tracked(s))
            }
        }) {
            Ok(_) => panic!("expected failure"),
            Err(error) => error,
        };
        if capture_failure {
            assert!(matches!(
                error,
                CheckpointMapError::Capture(Error::Machine {
                    id: MachineId(2),
                    ..
                })
            ));
            assert_eq!(*audit.calls.lock().unwrap(), invocation_prefix(2, true));
            assert_eq!(external_artifacts, vec![0, 1]);
        } else {
            let CheckpointMapError::Map(error) = error else {
                panic!("mapper error erased")
            };
            assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
            assert_eq!(
                error
                    .get_ref()
                    .unwrap()
                    .downcast_ref::<WriterFailure>()
                    .unwrap()
                    .0,
                73
            );
            assert_eq!(*audit.calls.lock().unwrap(), invocation_prefix(3, false));
            assert_eq!(external_artifacts, vec![0, 1, 2]);
        }
        assert_eq!(
            audit.native_drops.load(Ordering::SeqCst),
            if capture_failure { 2 } else { 3 }
        );
        assert_eq!(audit.mapped_drops.load(Ordering::SeqCst), 2);
        assert_eq!(audit.live.load(Ordering::SeqCst), 0);
        assert_eq!(audit.peak.load(Ordering::SeqCst), 1);
        assert_eq!(audit.runs.load(Ordering::SeqCst), runs);
        assert_eq!(audit.factories.load(Ordering::SeqCst), 5);
        assert_eq!(audit.contexts.load(Ordering::SeqCst), 1);
        assert_eq!(d.position(), position);
        assert_eq!(cursors(&d), rng);
        assert!(!d.execution.failed);
        d.execution.machines[2]
            .machine
            .as_mut()
            .unwrap()
            .inner
            .state
            .fail_capture = false;
        assert_eq!(tracked_states(&d), states);
        for end in [17, 20, 31, 40] {
            assert_eq!(d.step(at(end)).unwrap(), reference.step(at(end)).unwrap());
            assert_eq!(d.drain_events(), reference.drain_events());
            assert_eq!(tracked_states(&d), tracked_states(&reference));
            assert_eq!(cursors(&d), cursors(&reference));
        }
    }
}

#[test]
fn every_boundary_validation_precedes_first_capture_and_mapping() {
    for invalid in 0..6 {
        let (mut d, audit) = tracked(3);
        match invalid {
            0 => {
                d.execution.machines[0]
                    .machine
                    .as_mut()
                    .unwrap()
                    .inner
                    .state
                    .fail_run = true;
                assert!(d.step(at(1)).is_err());
            }
            1 => d.execution.rng = SimulationRng::External(Box::new(SeededRng::new(51))),
            2 => d.execution.machines[2].rng = None,
            3 => d.execution.quantum_ns = 0,
            4 => d.execution.machines[2].reached = at(1),
            5 => d.execution.oracle = MachineId(99),
            _ => unreachable!(),
        }
        let runs = audit.runs.load(Ordering::SeqCst);
        let result = d.checkpoint_map::<(), Io>(|_| panic!("mapper before all validation"));
        match invalid {
            0 => assert!(matches!(
                result,
                Err(CheckpointMapError::Capture(Error::Failed))
            )),
            1 => assert!(matches!(
                result,
                Err(CheckpointMapError::Capture(Error::CheckpointUnsupported))
            )),
            _ => assert!(matches!(
                result,
                Err(CheckpointMapError::Capture(Error::InvalidCheckpoint(_)))
            )),
        }
        assert!(audit.calls.lock().unwrap().is_empty());
        assert_eq!(audit.runs.load(Ordering::SeqCst), runs);
        assert_eq!(audit.factories.load(Ordering::SeqCst), 3);
        assert_eq!(audit.contexts.load(Ordering::SeqCst), 1);
    }
}
