use super::super::CaptureFailure;
use super::*;
#[test]
fn cheap_output_and_identity_refusals_precede_drain_and_native_capture() {
    let root = tempfile::tempdir().unwrap();
    let inventory = fixture_inventory(&root.path().join("authority"));
    for existing in [false, true] {
        let output = root
            .path()
            .join(if existing { "existing" } else { "missing" });
        if existing {
            fs::create_dir(&output).unwrap();
            fs::write(
                output.join("checkpoint-saved.json"),
                b"unowned existing manifest",
            )
            .unwrap();
        }
        let peak = Arc::new(AtomicUsize::new(0));
        let mut driver = driver(1, 1, Arc::default(), peak.clone(), None);
        let mut state = state(1);
        let error = capture(
            &mut driver,
            Capture {
                inventory: &inventory,
                state: &mut state,
                prepared: false,
            },
            &output,
            "saved",
            |_, _| panic!("refusal must not publish"),
            |_, _| panic!("refusal must not snapshot"),
        )
        .unwrap_err();
        assert!(matches!(error, CaptureFailure::Refused(_)));
        assert_eq!(peak.load(Ordering::SeqCst), 0);
        assert!(!driver.drain_observations().events.is_empty());
        if existing {
            assert_eq!(
                fs::read(output.join("checkpoint-saved.json")).unwrap(),
                b"unowned existing manifest"
            );
        }
    }
}
#[test]
fn missing_factory_authority_is_a_benign_pre_drain_refusal() {
    let root = tempfile::tempdir().unwrap();
    let inventory = fixture_inventory(&root.path().join("authority"));
    let output = root.path().join("output");
    fs::create_dir(&output).unwrap();
    fs::remove_file(&inventory.artifacts()[0].artifact).unwrap();
    let mut driver = driver(1, 1, Arc::default(), Arc::default(), None);
    let mut state = state(1);
    let error = capture(
        &mut driver,
        Capture {
            inventory: &inventory,
            state: &mut state,
            prepared: false,
        },
        &output,
        "saved",
        |_, _| panic!("must not publish"),
        |_, _| panic!("must not capture"),
    )
    .unwrap_err();
    assert!(matches!(error, CaptureFailure::Refused(_)));
    assert!(!driver.drain_observations().events.is_empty());
    assert_eq!(fs::read_dir(output).unwrap().count(), 0);
}
#[test]
fn publication_after_drain_failure_is_distinctly_terminal_not_an_ordinary_refusal() {
    let root = tempfile::tempdir().unwrap();
    let inventory = fixture_inventory(&root.path().join("authority"));
    let output = root.path().join("output");
    fs::create_dir(&output).unwrap();
    let mut driver = driver(1, 2, Arc::default(), Arc::default(), None);
    let mut state = state(2);
    let error = capture(
        &mut driver,
        Capture {
            inventory: &inventory,
            state: &mut state,
            prepared: false,
        },
        &output,
        "saved",
        |_, _| Err(integrity("injected publication failure")),
        |_, _| panic!("must not capture"),
    )
    .unwrap_err();
    assert!(matches!(error, CaptureFailure::Publication(_)));
    assert!(error.to_string().contains("stop execution"));
    assert!(driver.drain_observations().events.is_empty());
    assert_eq!(fs::read_dir(output).unwrap().count(), 0);
}
#[test]
fn abandoned_bodies_never_publish_or_block_identity_and_are_never_clobbered() {
    let root = tempfile::tempdir().unwrap();
    let inventory = fixture_inventory(&root.path().join("authority"));
    let output = root.path().join("output");
    fs::create_dir(&output).unwrap();
    let abandoned = output.join("checkpoint-saved-bodies");
    fs::create_dir(&abandoned).unwrap();
    fs::write(abandoned.join("sentinel"), b"abandoned owned-session data").unwrap();
    let mut driver = driver(1, 1, Arc::default(), Arc::default(), None);
    let mut state = state(1);
    let path = capture(
        &mut driver,
        Capture {
            inventory: &inventory,
            state: &mut state,
            prepared: false,
        },
        &output,
        "saved",
        publish,
        encode,
    )
    .unwrap();
    let envelope = read(&path).unwrap();
    assert_eq!(
        fs::read(abandoned.join("sentinel")).unwrap(),
        b"abandoned owned-session data"
    );
    assert_ne!(
        Path::new(&envelope.driver.artifact).parent().unwrap(),
        abandoned
    );
    assert!(
        path.is_file(),
        "only the manifest acknowledges a published capture"
    );
}
#[test]
fn actual_driver_before_fault_hold_is_captured_without_applying_effect() {
    use seedbox_driver::driver::{Action, DueOutcome, InputKey, PlaybackRate, Target};
    let root = tempfile::tempdir().unwrap();
    let inventory = fixture_inventory(&root.path().join("authority"));
    let peak = Arc::new(AtomicUsize::new(0));
    let mut driver = driver(1, 2, Arc::default(), peak.clone(), None);
    let mut state = state(2);
    driver
        .with_barrier(|barrier| {
            barrier.set_rate(PlaybackRate::Paused)?;
            barrier.schedule(
                Timestamp::from_nanos(0),
                InputKey::new("held").unwrap(),
                Action::Crash(Target::Name("m1".into())),
            )?;
            Ok(())
        })
        .unwrap();
    assert!(matches!(
        driver
            .with_barrier(|barrier| barrier.apply_due_now())
            .unwrap(),
        DueOutcome::BeforeFault { .. }
    ));
    let before = driver
        .with_barrier(|barrier| Ok(barrier.playback()))
        .unwrap();
    let mut effects = vec![];
    let path = capture(
        &mut driver,
        Capture {
            inventory: &inventory,
            state: &mut state,
            prepared: false,
        },
        root.path(),
        "held",
        |state, batch| {
            for event in &batch.events {
                if let seedbox_driver::driver::EventKind::FaultSettled { .. } = &event.kind {
                    effects.push(event.clone());
                }
            }
            publish(state, batch)
        },
        encode,
    )
    .unwrap();
    assert!(effects.is_empty(), "capture must not publish an effect");
    assert_eq!(peak.load(Ordering::SeqCst), 1);
    let captured = resolve(&read(&path).unwrap(), limits(), decode).unwrap();
    let mut restored = restored(&captured, 4);
    assert_eq!(
        restored
            .with_barrier(|barrier| Ok(barrier.playback()))
            .unwrap(),
        before
    );
    assert!(
        restored.drain_observations().events.is_empty(),
        "capture must not replay observations"
    );
    assert_eq!(
        driver
            .with_barrier(|barrier| Ok(barrier.playback()))
            .unwrap(),
        before
    );
}
