use super::*;
#[path = "codec_v3_tests.rs"]
mod codec_v3_tests;
#[path = "controller_tests.rs"]
mod controller_tests;
#[path = "lazy_restore_tests.rs"]
mod lazy_restore_tests;
#[path = "metadata_roundtrip_tests.rs"]
mod metadata_roundtrip_tests;
#[path = "publication_tests.rs"]
mod publication_tests;
use seedbox_driver::driver::NetworkConfig;
use seedbox_machine::{
    definition::{FilesystemArchive, Launch, MachineDefinition, PreparedApplication, Resources},
    machine::{RunContext, StopReason, Timestamp},
    pv::{Output, Response, Stream},
};
use std::{
    collections::BTreeMap,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};

struct Mock {
    at: u64,
    random: [u8; 16],
    first: bool,
    live_snapshots: Arc<AtomicUsize>,
    peak_snapshots: Arc<AtomicUsize>,
    fail_capture: bool,
}
struct Saved {
    at: u64,
    random: [u8; 16],
    first: bool,
    live: Arc<AtomicUsize>,
}
impl Drop for Saved {
    fn drop(&mut self) {
        self.live.fetch_sub(1, Ordering::SeqCst);
    }
}
impl Machine for Mock {
    type Context = ();
    type Error = String;
    type Snapshot = Saved;
    fn run(
        &mut self,
        c: &mut RunContext<'_, ()>,
        upto: Timestamp,
    ) -> std::result::Result<StopReason, String> {
        while c.inbox.take().is_some() {}
        if self.first {
            c.outbox
                .emit(Output {
                    at: Timestamp::from_nanos(c.clock.now()),
                    payload: Response::Ready,
                })
                .unwrap();
            self.first = false;
        }
        if upto.as_nanos() > c.clock.now() {
            c.rng.fill_bytes(&mut self.random);
            c.outbox
                .emit(Output {
                    at: upto,
                    payload: Response::ApplicationOutput {
                        stream: Stream::Stdout,
                        bytes: self.random.to_vec(),
                    },
                })
                .unwrap();
        }
        self.at = upto.as_nanos();
        Ok(StopReason::BoundaryReached)
    }
    fn snapshot(&mut self) -> std::result::Result<Saved, String> {
        if self.fail_capture {
            return Err("native capture failed".into());
        }
        let count = self.live_snapshots.fetch_add(1, Ordering::SeqCst) + 1;
        self.peak_snapshots.fetch_max(count, Ordering::SeqCst);
        Ok(Saved {
            at: self.at,
            random: self.random,
            first: self.first,
            live: self.live_snapshots.clone(),
        })
    }
}
fn definition(name: &str) -> MachineDefinition {
    MachineDefinition {
        name: name.into(),
        resources: Resources {
            memory_bytes: 128 << 20,
        },
        application: PreparedApplication {
            filesystem: FilesystemArchive::new(tar::Builder::new(Vec::new()).into_inner().unwrap())
                .unwrap(),
            launch: Launch {
                argv: vec!["/application".into()],
                environment: BTreeMap::new(),
                working_directory: "/".into(),
                user: "0:0".into(),
            },
        },
        disk: None,
        network: None,
    }
}
fn driver(
    workers: usize,
    count: usize,
    live: Arc<AtomicUsize>,
    peak: Arc<AtomicUsize>,
    failure: Option<usize>,
) -> Driver<Mock> {
    let mut index = 0;
    Driver::new_seeded(
        Timestamp::from_nanos(0),
        Duration::from_nanos(10),
        workers,
        9338,
        || Ok(()),
        definition("oracle"),
        (1..count).map(|i| definition(&format!("m{i}"))).collect(),
        vec![],
        NetworkConfig::default(),
        move |_| {
            let fail_capture = failure == Some(index);
            index += 1;
            Ok(Mock {
                at: 0,
                random: [0; 16],
                first: true,
                live_snapshots: live.clone(),
                peak_snapshots: peak.clone(),
                fail_capture,
            })
        },
    )
    .unwrap()
}
fn fixture_inventory(root: &Path) -> Inventory {
    fs::create_dir_all(root).unwrap();
    let make = |name: &str| {
        let path = root.join(name);
        fs::write(&path, format!("authority {name}")).unwrap();
        describe(&path, &path).unwrap()
    };
    Inventory::Pv {
        policy: None,
        executable: make("executable"),
        kernel: make("kernel"),
        init: make("init"),
        command_line: make("command-line"),
        progress: make("progress"),
        runtime: make("runtime"),
        formatter: None,
        environments: Default::default(),
        boot_archives: Default::default(),
        blocks_per_progress_call: std::num::NonZeroU32::new(32).unwrap(),
        epoch_seconds: 1700000123,
    }
}
fn state(count: usize) -> State {
    let mut initial = vec!["oracle".to_owned()];
    initial.extend((1..count).map(|i| format!("m{i}")));
    initial.sort();
    State {
        world: wire::World {
            seed: 9338,
            machine_names: std::iter::once("oracle".to_owned())
                .chain((1..count).map(|i| format!("m{i}")))
                .collect(),
            input_program: Default::default(),
            intervention_next: 0,
            readiness: wire::Readiness {
                initial,
                seen: vec![],
                latched: false,
            },
            publication: Default::default(),
            position: Default::default(),
        },
    }
}
// Compare exact future facts/positions while each observer session owns its
// independent dense sequence. Never drop or filter an event or its payload.
fn session_events(
    mut events: Vec<seedbox_driver::driver::Event>,
) -> Vec<seedbox_driver::driver::Event> {
    let first = events.first().map_or(0, |event| event.sequence);
    for (offset, event) in events.iter_mut().enumerate() {
        assert_eq!(event.sequence, first + offset as u64);
        event.sequence = offset as u64;
    }
    events
}
fn publish(state: &mut State, batch: seedbox_driver::driver::FlushResults) -> Result<()> {
    for event in batch.events {
        assert_eq!(
            event.sequence,
            state.world.publication.session.next_driver_sequence
        );
        state.world.publication.session.next_driver_sequence += 1;
    }
    Ok(())
}

fn encode(snapshot: Saved, writer: &mut BufWriter<File>) -> Result<()> {
    writer.write_all(&snapshot.at.to_le_bytes())?;
    writer.write_all(&snapshot.random)?;
    writer.write_all(&[snapshot.first as u8])?;
    Ok(())
}
fn decode(_: &Inventory, reader: &mut BufReader<File>) -> Result<Saved> {
    decode_body(reader)
}
fn decode_body(reader: &mut impl Read) -> Result<Saved> {
    let mut at = [0; 8];
    reader.read_exact(&mut at)?;
    let mut random = [0; 16];
    reader.read_exact(&mut random)?;
    let mut first = [0];
    reader.read_exact(&mut first)?;
    let mut extra = [0];
    if reader.read(&mut extra)? != 0 {
        return Err(integrity("mock trailing bytes"));
    }
    Ok(Saved {
        at: u64::from_le_bytes(at),
        random,
        first: first[0] != 0,
        live: Arc::new(AtomicUsize::new(1)),
    })
}
fn limits() -> seedbox_driver::driver::DecodeLimits {
    seedbox_driver::driver::DecodeLimits {
        encoded_bytes: 16 << 20,
        allocation_bytes: 128 << 20,
        collection_items: 100_000,
        total_items: 500_000,
        string_bytes: 4096,
        blob_bytes: 16 << 20,
        depth: 128,
        archive_entries: 100_000,
        archive_path_bytes: 1 << 20,
        archive_expanded_bytes: 16 << 20,
    }
}
fn restored(checkpoint: &Checkpoint<Saved>, workers: usize) -> Driver<Mock> {
    Driver::<Mock>::from_checkpoint(
        checkpoint,
        workers,
        || Ok(()),
        |saved| {
            Ok(Mock {
                at: saved.at,
                random: saved.random,
                first: saved.first,
                live_snapshots: Arc::new(AtomicUsize::new(0)),
                peak_snapshots: Arc::new(AtomicUsize::new(0)),
                fail_capture: false,
            })
        },
        |_| Err("restoration must not call future factory for existing machines".into()),
    )
    .unwrap()
}

#[test]
fn streamed_world_capture_survives_source_drop_and_branches_with_one_or_four_workers() {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("source");
    fs::create_dir(&source).unwrap();
    let output = root.path().join("retained");
    fs::create_dir(&output).unwrap();
    let inventory = fixture_inventory(&source);
    let live = Arc::new(AtomicUsize::new(0));
    let peak = Arc::new(AtomicUsize::new(0));
    let mut source_driver = driver(4, 4, live.clone(), peak.clone(), None);
    source_driver.step(Timestamp::from_nanos(10)).unwrap();
    let mut state = state(4);
    let identities = source_driver
        .with_barrier(|barrier| Ok(barrier.machines().map(|(id, _)| id).collect::<Vec<_>>()))
        .unwrap();
    let names = ["oracle", "m1", "m2", "m3"];
    let manifest = capture(
        &mut source_driver,
        Capture {
            inventory: &inventory,
            state: &mut state,
            prepared: false,
        },
        &output,
        "saved",
        |state, batch| {
            for change in &batch.changes {
                if let seedbox_driver::driver::CommittedChange::Ready { machine, .. } =
                    &change.change
                {
                    let index = identities.iter().position(|id| id == machine).unwrap();
                    state.world.readiness.observe(names[index]);
                }
            }
            publish(state, batch)
        },
        encode,
    )
    .unwrap();
    assert_eq!(peak.load(Ordering::SeqCst), 1);
    assert_eq!(live.load(Ordering::SeqCst), 0);
    assert!(state.world.readiness.latched);
    assert!(
        source_driver.drain_events().is_empty(),
        "capture must drain publications once"
    );
    source_driver.step(Timestamp::from_nanos(20)).unwrap();
    let expected = session_events(source_driver.drain_events());
    drop(source_driver);
    fs::remove_dir_all(source).unwrap();
    let envelope = read(&manifest).unwrap();
    assert_eq!(
        serde_json::to_value(&envelope.state).unwrap(),
        serde_json::to_value(&state).unwrap()
    );
    assert_eq!(envelope.state.world.publication.session, Default::default());
    for workers in [1, 4] {
        let checkpoint = resolve(&envelope, limits(), decode).unwrap();
        let mut branch = restored(&checkpoint, workers);
        assert!(
            branch.drain_events().is_empty(),
            "restoration must not reemit published prefix"
        );
        branch.step(Timestamp::from_nanos(20)).unwrap();
        assert_eq!(session_events(branch.drain_events()), expected);
    }
}

#[test]
fn mapper_failure_drops_current_snapshot_and_publishes_nothing_without_retry() {
    let root = tempfile::tempdir().unwrap();
    let inventory = fixture_inventory(&root.path().join("authority"));
    let output = root.path().join("output");
    fs::create_dir(&output).unwrap();
    let live = Arc::new(AtomicUsize::new(0));
    let peak = Arc::new(AtomicUsize::new(0));
    let mut driver = driver(1, 3, live.clone(), peak.clone(), None);
    let mut state = state(3);
    let mut calls = 0;
    let result = capture(
        &mut driver,
        Capture {
            inventory: &inventory,
            state: &mut state,
            prepared: false,
        },
        &output,
        "fail",
        publish,
        |snapshot, writer| {
            calls += 1;
            if calls == 2 {
                writer.write_all(b"partial")?;
                return Err(integrity("injected write failure"));
            }
            encode(snapshot, writer)
        },
    );
    assert!(result
        .unwrap_err()
        .to_string()
        .contains("injected write failure"));
    assert_eq!(calls, 2);
    assert_eq!(live.load(Ordering::SeqCst), 0);
    assert_eq!(fs::read_dir(&output).unwrap().count(), 0);
    assert!(
        driver.step(Timestamp::from_nanos(10)).is_ok(),
        "mapper I/O failure does not poison valid source"
    );
}

#[test]
fn later_native_capture_failure_never_acknowledges_partial_checkpoint() {
    let root = tempfile::tempdir().unwrap();
    let inventory = fixture_inventory(&root.path().join("authority"));
    let output = root.path().join("output");
    fs::create_dir(&output).unwrap();
    let live = Arc::new(AtomicUsize::new(0));
    let peak = Arc::new(AtomicUsize::new(0));
    let mut driver = driver(1, 3, live.clone(), peak, Some(1));
    let mut state = state(3);
    let error = capture(
        &mut driver,
        Capture {
            inventory: &inventory,
            state: &mut state,
            prepared: false,
        },
        &output,
        "fail",
        publish,
        encode,
    )
    .unwrap_err();
    assert!(error.to_string().contains("native capture failed"));
    assert_eq!(live.load(Ordering::SeqCst), 0);
    assert_eq!(fs::read_dir(output).unwrap().count(), 0);
}

#[test]
fn prepared_capture_refusal_does_not_drain_or_call_native_snapshot() {
    let root = tempfile::tempdir().unwrap();
    let inventory = fixture_inventory(&root.path().join("authority"));
    let live = Arc::new(AtomicUsize::new(0));
    let peak = Arc::new(AtomicUsize::new(0));
    let mut driver = driver(1, 1, live, peak.clone(), None);
    let mut state = state(1);
    assert!(capture(
        &mut driver,
        Capture {
            inventory: &inventory,
            state: &mut state,
            prepared: true,
        },
        root.path(),
        "saved",
        |_, _| panic!("must not publish"),
        |_, _| panic!("must not snapshot")
    )
    .is_err());
    assert_eq!(peak.load(Ordering::SeqCst), 0);
    assert!(!driver.drain_events().is_empty());
}

#[test]
fn integrity_and_length_are_verified_before_decoding_any_native_body() {
    let root = tempfile::tempdir().unwrap();
    let inventory = fixture_inventory(&root.path().join("authority"));
    let mut driver = driver(1, 1, Arc::default(), Arc::default(), None);
    let mut state = state(1);
    let path = capture(
        &mut driver,
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
    let envelope = read(&path).unwrap();
    let artifact = &envelope.machines[0];
    let original = fs::read(&artifact.artifact).unwrap();
    let mut changed = original.clone();
    changed[0] ^= 1;
    fs::write(&artifact.artifact, &changed).unwrap();
    assert!(read(&path).unwrap_err().to_string().contains("SHA-256"));
    assert!(resolve::<Saved>(&envelope, limits(), |_, _| panic!("must verify first")).is_err());
    fs::write(&artifact.artifact, &original[..original.len() - 1]).unwrap();
    assert!(read(&path)
        .unwrap_err()
        .to_string()
        .contains("encoded length"));
}

#[test]
fn body_manifest_mismatch_and_unused_entries_are_not_silently_accepted() {
    let root = tempfile::tempdir().unwrap();
    let inventory = fixture_inventory(&root.path().join("authority"));
    let mut driver = driver(1, 2, Arc::default(), Arc::default(), None);
    let mut state = state(2);
    let path = capture(
        &mut driver,
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
    let mut envelope = read(&path).unwrap();
    envelope.machines[0].sha256 = "00".repeat(32);
    assert!(resolve::<Saved>(&envelope, limits(), |_, _| panic!(
        "must check reference first"
    ))
    .is_err());
    let mut envelope = read(&path).unwrap();
    envelope.machines.push(envelope.machines[0].clone());
    assert!(resolve(&envelope, limits(), decode)
        .err()
        .unwrap()
        .to_string()
        .contains("unused"));
}

#[test]
fn durable_capture_exports_without_source_paths() {
    let root = tempfile::tempdir().unwrap();
    let inventory = fixture_inventory(&root.path().join("authority"));
    let mut driver = driver(1, 1, Arc::default(), Arc::default(), None);
    let mut state = state(1);
    let output = root.path().join("source-run");
    fs::create_dir(&output).unwrap();
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
    let store = crate::store::ContentStore::new(root.path().join("store"));
    let start = crate::definition::capture_start(&path, &output, &store).unwrap();
    let seedbox_spec::definition::Start::Checkpoint { manifest, assets } = start else {
        panic!("expected checkpoint")
    };
    assert_eq!(
        assets.len(),
        8,
        "six factory artifacts (including the runtime closure) + native + Driver metadata"
    );
    assert_eq!(
        manifest.pointer("/state/world/input_program/next").unwrap(),
        0
    );
    fs::remove_dir_all(output).unwrap();
    assert!(!assets.is_empty());
    // Each binary was captured in the store; no source-run path is needed to
    // retain the exported definition. Full definition materialization is owned
    // by existing definition tests, and this test verifies our walker shape.
    for asset in assets.values() {
        let digest = crate::store::ContentDigest::try_from(
            asset.0.strip_prefix("sha256:").unwrap().to_owned(),
        )
        .unwrap();
        store.verify(&digest).unwrap();
    }
}

#[test]
fn failed_manifest_publication_preserves_unowned_file_and_removes_unpublished_bodies() {
    let root = tempfile::tempdir().unwrap();
    let inventory = fixture_inventory(&root.path().join("authority"));
    let output = root.path().join("output");
    fs::create_dir(&output).unwrap();
    let mut driver = driver(1, 1, Arc::default(), Arc::default(), None);
    let mut state = state(1);
    let competing = output.join("checkpoint-race.v1.json");
    let result = capture(
        &mut driver,
        Capture {
            inventory: &inventory,
            state: &mut state,
            prepared: false,
        },
        &output,
        "race.v1",
        publish,
        |snapshot, writer| {
            fs::write(&competing, b"unowned manifest")?;
            encode(snapshot, writer)
        },
    );
    assert!(result.is_err());
    assert_eq!(fs::read(competing).unwrap(), b"unowned manifest");
    assert_eq!(fs::read_dir(output).unwrap().count(), 1);
}

#[test]
fn publish_failure_is_terminal_for_capture_without_native_calls_or_republication() {
    let root = tempfile::tempdir().unwrap();
    let inventory = fixture_inventory(&root.path().join("authority"));
    let output = root.path().join("output");
    fs::create_dir(&output).unwrap();
    let peak = Arc::new(AtomicUsize::new(0));
    let mut driver = driver(1, 2, Arc::default(), peak.clone(), None);
    let mut state = state(2);
    let mut publications = 0;
    let result = capture(
        &mut driver,
        Capture {
            inventory: &inventory,
            state: &mut state,
            prepared: false,
        },
        &output,
        "fail",
        |_, _| {
            publications += 1;
            Err(integrity("observer failure"))
        },
        |_, _| panic!("must not capture after publication failure"),
    );
    assert!(result.unwrap_err().to_string().contains("observer failure"));
    assert_eq!(publications, 1);
    assert_eq!(peak.load(Ordering::SeqCst), 0);
    assert_eq!(fs::read_dir(output).unwrap().count(), 0);
    assert!(
        driver.drain_events().is_empty(),
        "the worker owner must terminate, never reemit or retry the drained prefix"
    );
}

#[test]
fn corrupt_later_body_and_unused_manifest_entries_fail_before_any_backend_decoder() {
    let root = tempfile::tempdir().unwrap();
    let inventory = fixture_inventory(&root.path().join("authority"));
    let mut driver = driver(1, 2, Arc::default(), Arc::default(), None);
    let mut state = state(2);
    let path = capture(
        &mut driver,
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
    let mut envelope = read(&path).unwrap();
    envelope.machines.push(envelope.machines[0].clone());
    assert!(resolve::<Saved>(&envelope, limits(), |_, _| panic!(
        "all references must precede decode"
    ))
    .is_err());
    let envelope = read(&path).unwrap();
    let later = &envelope.machines[1];
    let mut bytes = fs::read(&later.artifact).unwrap();
    bytes[0] ^= 1;
    fs::write(&later.artifact, bytes).unwrap();
    assert!(resolve::<Saved>(&envelope, limits(), |_, _| panic!(
        "all integrity checks must precede decode"
    ))
    .is_err());
}
