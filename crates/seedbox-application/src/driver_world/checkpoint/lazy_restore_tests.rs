//! Non-Clone decoded-native-body lifetimes through the same production open
//! descriptor path. Target machines accumulate; decoded snapshots must not.
use super::super::{open, Opened};
use super::*;

enum LazySaved {
    Stored(Opened),
    Native(Saved),
}
struct Restoring {
    machine: Mock,
    drops: Arc<AtomicUsize>,
}
impl Drop for Restoring {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::SeqCst);
    }
}
impl Machine for Restoring {
    type Context = ();
    type Error = String;
    type Snapshot = LazySaved;
    fn run(
        &mut self,
        c: &mut RunContext<'_, ()>,
        upto: Timestamp,
    ) -> std::result::Result<StopReason, String> {
        self.machine.run(c, upto)
    }
    fn snapshot(&mut self) -> std::result::Result<LazySaved, String> {
        self.machine.snapshot().map(LazySaved::Native)
    }
}
struct Decoded {
    saved: Saved,
    live: Arc<AtomicUsize>,
}
impl Drop for Decoded {
    fn drop(&mut self) {
        self.live.fetch_sub(1, Ordering::SeqCst);
    }
}
fn restore_one(
    saved: &LazySaved,
    live: &Arc<AtomicUsize>,
    peak: &Arc<AtomicUsize>,
    drops: &Arc<AtomicUsize>,
    fail: bool,
) -> std::result::Result<Restoring, String> {
    let decoded = match saved {
        LazySaved::Stored(file) => decode_body(&mut file.reader()).map_err(|e| e.to_string())?,
        LazySaved::Native(saved) => Saved {
            at: saved.at,
            random: saved.random,
            first: saved.first,
            live: Arc::new(AtomicUsize::new(1)),
        },
    };
    let count = live.fetch_add(1, Ordering::SeqCst) + 1;
    peak.fetch_max(count, Ordering::SeqCst);
    let decoded = Decoded {
        saved: decoded,
        live: live.clone(),
    };
    if fail {
        return Err("injected later native restore failure".into());
    }
    let machine = Mock {
        at: decoded.saved.at,
        random: decoded.saved.random,
        first: decoded.saved.first,
        live_snapshots: Arc::default(),
        peak_snapshots: Arc::default(),
        fail_capture: false,
    };
    // The non-Clone owned decoded RAM surrogate drops here, before the next
    // callback. Only the target machine's independent execution state survives.
    drop(decoded);
    Ok(Restoring {
        machine,
        drops: drops.clone(),
    })
}
#[test]
fn captured_names_require_unique_present_names_even_if_service_names_match() {
    use super::super::validate_machine_names;
    let valid = vec!["oracle".into(), "target".into()];
    validate_machine_names([Some("oracle"), Some("target")].into_iter(), &valid).unwrap();
    assert!(
        validate_machine_names([Some("oracle"), None].into_iter(), &valid)
            .unwrap_err()
            .to_string()
            .contains("missing service machine name")
    );
    let duplicates = vec!["oracle".into(), "oracle".into()];
    assert!(
        validate_machine_names([Some("oracle"), Some("oracle")].into_iter(), &duplicates)
            .unwrap_err()
            .to_string()
            .contains("duplicate service machine names")
    );
    assert!(validate_machine_names([Some("target"), Some("oracle")].into_iter(), &valid).is_err());
}
#[test]
fn service_name_substitution_is_refused_before_any_native_mapping() {
    let root = tempfile::tempdir().unwrap();
    let inventory = fixture_inventory(&root.path().join("authority"));
    let mut source = driver(1, 2, Arc::default(), Arc::default(), None);
    let mut state = state(2);
    let path = capture(
        &mut source,
        Capture {
            inventory: &inventory,
            state: &mut state,
            prepared: false,
        },
        root.path(),
        "name-binding",
        publish,
        encode,
    )
    .unwrap();
    let mut envelope = read(&path).unwrap();
    let bodies: Vec<_> = envelope
        .machines
        .iter()
        .map(|artifact| fs::read(&artifact.artifact).unwrap())
        .collect();
    envelope.state.world.machine_names[1] = "substituted".into();
    envelope.state.world.readiness.initial = vec!["oracle".into(), "substituted".into()];
    envelope.validate().unwrap();
    let callbacks = AtomicUsize::new(0);
    let error = open(&envelope, limits())
        .and_then(|checkpoint| {
            checkpoint.try_map_snapshots(|_| {
                callbacks.fetch_add(1, Ordering::SeqCst);
                Ok::<_, Error>(())
            })
        })
        .err()
        .unwrap();
    assert!(error.to_string().contains("captured Driver control ledger"));
    assert_eq!(callbacks.load(Ordering::SeqCst), 0);
    for (artifact, before) in envelope.machines.iter().zip(bodies) {
        assert_eq!(fs::read(&artifact.artifact).unwrap(), before);
    }
}
#[test]
fn simultaneous_restores_share_verified_descriptors_not_reader_positions() {
    let root = tempfile::tempdir().unwrap();
    let inventory = fixture_inventory(&root.path().join("authority"));
    let mut source = driver(4, 3, Arc::default(), Arc::default(), None);
    source.step(Timestamp::from_nanos(10)).unwrap();
    let mut state = state(3);
    let path = capture(
        &mut source,
        Capture {
            inventory: &inventory,
            state: &mut state,
            prepared: false,
        },
        root.path(),
        "concurrent",
        publish,
        encode,
    )
    .unwrap();
    source.step(Timestamp::from_nanos(20)).unwrap();
    let expected = session_events(source.drain_events());
    drop(source);
    let envelope = read(&path).unwrap();
    let checkpoint = open(&envelope, limits())
        .unwrap()
        .try_map_snapshots(|file| Ok::<_, Error>(LazySaved::Stored(file)))
        .unwrap();
    for artifact in &envelope.machines {
        fs::remove_file(&artifact.artifact).unwrap();
    }
    std::thread::scope(|scope| {
        let mut handles = vec![];
        for workers in [1, 4, 1, 4] {
            let checkpoint = &checkpoint;
            let expected = &expected;
            handles.push(scope.spawn(move || {
                let live = Arc::new(AtomicUsize::new(0));
                let peak = Arc::new(AtomicUsize::new(0));
                let drops = Arc::new(AtomicUsize::new(0));
                let mut branch = Driver::<Restoring>::from_checkpoint(
                    checkpoint,
                    workers,
                    || Ok(()),
                    |saved| {
                        let LazySaved::Stored(file) = saved else {
                            panic!("retained descriptor required")
                        };
                        let mut first = file.reader();
                        let mut second = file.reader();
                        let mut a = [0; 8];
                        let mut b = [0; 8];
                        first.read_exact(&mut a).unwrap();
                        second.read_exact(&mut b).unwrap();
                        assert_eq!(a, b);
                        let mut tail_a = vec![];
                        let mut tail_b = vec![];
                        first.read_to_end(&mut tail_a).unwrap();
                        second.read_to_end(&mut tail_b).unwrap();
                        assert_eq!(tail_a.len(), 17);
                        assert_eq!(tail_a, tail_b);
                        restore_one(saved, &live, &peak, &drops, false)
                    },
                    |_| panic!("existing machines must not call future factory"),
                )
                .unwrap();
                assert_eq!(live.load(Ordering::SeqCst), 0);
                assert_eq!(peak.load(Ordering::SeqCst), 1);
                branch.step(Timestamp::from_nanos(20)).unwrap();
                assert_eq!(&session_events(branch.drain_events()), expected);
                drop(branch);
                assert_eq!(drops.load(Ordering::SeqCst), 3);
            }));
        }
        for handle in handles {
            handle.join().unwrap();
        }
    });
}

#[test]
fn descriptor_backed_restore_has_one_decoded_body_at_a_time_and_cleans_partial_targets() {
    let root = tempfile::tempdir().unwrap();
    let inventory = fixture_inventory(&root.path().join("authority"));
    let mut source = driver(4, 3, Arc::default(), Arc::default(), None);
    source.step(Timestamp::from_nanos(10)).unwrap();
    let mut state = state(3);
    let manifest = capture(
        &mut source,
        Capture {
            inventory: &inventory,
            state: &mut state,
            prepared: false,
        },
        root.path(),
        "saved",
        publish,
        encode,
    )
    .unwrap();
    source.step(Timestamp::from_nanos(20)).unwrap();
    let expected = session_events(source.drain_events());
    drop(source);
    let envelope = read(&manifest).unwrap();
    let checkpoint = open(&envelope, limits())
        .unwrap()
        .try_map_snapshots(|file| Ok::<_, Error>(LazySaved::Stored(file)))
        .unwrap();
    for artifact in &envelope.machines {
        fs::remove_file(&artifact.artifact).unwrap();
    }
    let live = Arc::new(AtomicUsize::new(0));
    let peak = Arc::new(AtomicUsize::new(0));
    let drops = Arc::new(AtomicUsize::new(0));
    let mut calls = 0;
    let failed = Driver::<Restoring>::from_checkpoint(
        &checkpoint,
        4,
        || Ok(()),
        |saved| {
            calls += 1;
            restore_one(saved, &live, &peak, &drops, calls == 2)
        },
        |_| panic!("existing machines must not call future factory"),
    );
    assert!(failed
        .err()
        .unwrap()
        .to_string()
        .contains("injected later native restore failure"));
    assert_eq!(calls, 2);
    assert_eq!(live.load(Ordering::SeqCst), 0);
    assert_eq!(peak.load(Ordering::SeqCst), 1);
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    // The same checkpoint remains reusable after failure and native source/file
    // destruction. Independent pread cursors do not need seek-reset or retries.
    for workers in [1, 4] {
        let before = drops.load(Ordering::SeqCst);
        let mut branch = Driver::<Restoring>::from_checkpoint(
            &checkpoint,
            workers,
            || Ok(()),
            |saved| restore_one(saved, &live, &peak, &drops, false),
            |_| panic!("existing machines must not call future factory"),
        )
        .unwrap();
        assert_eq!(live.load(Ordering::SeqCst), 0);
        assert_eq!(peak.load(Ordering::SeqCst), 1);
        assert!(branch.drain_events().is_empty());
        branch.step(Timestamp::from_nanos(20)).unwrap();
        assert_eq!(session_events(branch.drain_events()), expected);
        drop(branch);
        assert_eq!(drops.load(Ordering::SeqCst) - before, 3);
    }
}
