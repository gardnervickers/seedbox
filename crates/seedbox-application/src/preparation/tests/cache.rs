use super::*;
use std::cell::Cell;

fn launch() -> crate::image::Launch {
    crate::image::Launch {
        argv: vec!["/app".into()],
        environment: BTreeMap::new(),
        working_dir: "/".into(),
        user: "".into(),
        volumes: vec![],
        disk_volumes: vec![],
    }
}

fn filesystem(contents: &[u8]) -> Vec<u8> {
    let mut archive = tar::Builder::new(Vec::new());
    let mut header = tar::Header::new_gnu();
    header.set_uid(0);
    header.set_gid(0);
    header.set_mtime(0);
    header.set_mode(0o755);
    header.set_size(contents.len() as u64);
    archive.append_data(&mut header, "app", contents).unwrap();
    archive.into_inner().unwrap()
}

#[test]
fn prepared_boot_cache_invalidates_every_machine_input() {
    let root = tempfile::tempdir().unwrap();
    let (preparer, simulation, store) = fixture(root.path());
    let engine = store.put(b"engine").unwrap();
    let epoch = 86_400;
    let blocks = simulation.simulator.progress_budget;
    let cache = PreparedBootCache::new(&engine, &preparer.runtime, epoch, blocks, &store).unwrap();
    let boot = TestBoot::new(&preparer.runtime, epoch, blocks).unwrap();
    let launch = launch();
    let configs = BTreeMap::from([("/config".into(), b"one".to_vec())]);
    let fs = filesystem(b"application");
    let request = || OciBoot {
        name: "oracle",
        filesystem: fs.clone(),
        launch: &launch,
        simulation: &simulation,
        configs: &configs,
        hosts: "127.0.0.1 localhost\n10.0.0.1 oracle\n",
    };
    let renders = Cell::new(0);
    let render = |request: OciBoot<'_>, store: &ContentStore| {
        renders.set(renders.get() + 1);
        boot.boot(request, store)
    };
    let first = cache.prepare(request(), &store, render).unwrap();
    assert_eq!(first, cache.prepare(request(), &store, render).unwrap());
    assert_eq!(renders.get(), 1);
    let miss = |cache: &PreparedBootCache, request: OciBoot<'_>| {
        let before = renders.get();
        cache.prepare(request, &store, render).unwrap();
        assert_eq!(renders.get(), before + 1);
    };
    for (epoch, blocks, engine) in [
        (epoch + 1, blocks, engine.clone()),
        (
            epoch,
            std::num::NonZeroU32::new(7).unwrap().into(),
            engine.clone(),
        ),
        (epoch, blocks, store.put(b"changed engine").unwrap()),
    ] {
        let changed =
            PreparedBootCache::new(&engine, &preparer.runtime, epoch, blocks, &store).unwrap();
        miss(&changed, request());
    }
    // Equal lengths intentionally rule out size-only identities.
    miss(
        &cache,
        OciBoot {
            filesystem: filesystem(b"APPLICATION"),
            ..request()
        },
    );
    let changed_configs = BTreeMap::from([("/config".into(), b"two".to_vec())]);
    miss(
        &cache,
        OciBoot {
            configs: &changed_configs,
            ..request()
        },
    );
    let moved_config = BTreeMap::from([("/other".into(), b"one".to_vec())]);
    miss(
        &cache,
        OciBoot {
            configs: &moved_config,
            ..request()
        },
    );
    miss(
        &cache,
        OciBoot {
            hosts: "changed hosts",
            ..request()
        },
    );
    for field in [
        "argv",
        "environment",
        "working_dir",
        "user",
        "volumes",
        "disk_volumes",
    ] {
        let mut changed = launch.clone();
        match field {
            "argv" => changed.argv.push("argument".into()),
            "environment" => {
                changed.environment.insert("KEY".into(), "value".into());
            }
            "working_dir" => changed.working_dir = "/tmp".into(),
            "user" => changed.user = "1000".into(),
            "volumes" => changed.volumes.push("/data".into()),
            "disk_volumes" => changed.disk_volumes.push(config::VolumeMount {
                source: "data".into(),
                target: "/data".into(),
                size_bytes: None,
            }),
            _ => unreachable!(),
        }
        miss(
            &cache,
            OciBoot {
                launch: &changed,
                ..request()
            },
        );
    }
    let mut changed = simulation.clone();
    changed.machine.memory_mb += 1;
    miss(
        &cache,
        OciBoot {
            simulation: &changed,
            ..request()
        },
    );
    changed = simulation.clone();
    changed.services.get_mut("oracle").unwrap().memory_mb = Some(256);
    miss(
        &cache,
        OciBoot {
            simulation: &changed,
            ..request()
        },
    );
    changed = simulation.clone();
    changed.control = if simulation
        .control
        .as_ref()
        .is_some_and(|c| c.service == "oracle")
    {
        None
    } else {
        Some(config::Control {
            service: "oracle".into(),
        })
    };
    miss(
        &cache,
        OciBoot {
            simulation: &changed,
            ..request()
        },
    );
    changed = simulation.clone();
    changed
        .services
        .insert("before".into(), simulation.services["oracle"].clone());
    miss(
        &cache,
        OciBoot {
            simulation: &changed,
            ..request()
        },
    );
    changed = simulation.clone();
    let service = changed.services.remove("oracle").unwrap();
    changed.services.insert("renamed".into(), service);
    miss(
        &cache,
        OciBoot {
            name: "renamed",
            simulation: &changed,
            ..request()
        },
    );

    for path in ["kernel", "init", "command-line", "progress.tar", "mke2fs"] {
        let path = preparer.runtime.guest_assets.join(path);
        let original = fs::read(&path).unwrap();
        let mut changed = original.clone();
        changed[0] ^= 1;
        fs::write(&path, changed).unwrap();
        let cache =
            PreparedBootCache::new(&engine, &preparer.runtime, epoch, blocks, &store).unwrap();
        miss(&cache, request());
        fs::write(path, original).unwrap();
    }
    let package = preparer.runtime.archive();
    let original = fs::read(&package).unwrap();
    fs::write(&package, b"changed package").unwrap();
    let cache = PreparedBootCache::new(&engine, &preparer.runtime, epoch, blocks, &store).unwrap();
    miss(&cache, request());
    fs::write(package, original).unwrap();
}

#[test]
fn prepared_boot_cache_requires_verified_complete_content() {
    let root = tempfile::tempdir().unwrap();
    let (preparer, simulation, store) = fixture(root.path());
    let cache = PreparedBootCache::new(
        &store.put(b"engine").unwrap(),
        &preparer.runtime,
        86_400,
        simulation.simulator.progress_budget,
        &store,
    )
    .unwrap();
    let boot = TestBoot::new(
        &preparer.runtime,
        86_400,
        simulation.simulator.progress_budget,
    )
    .unwrap();
    let launch = launch();
    let configs = BTreeMap::new();
    let request = || OciBoot {
        name: "oracle",
        filesystem: filesystem(b"application"),
        launch: &launch,
        simulation: &simulation,
        configs: &configs,
        hosts: "localhost",
    };
    let renders = Cell::new(0);
    let render = |request: OciBoot<'_>, store: &ContentStore| {
        renders.set(renders.get() + 1);
        boot.boot(request, store)
    };
    let expected = cache.prepare(request(), &store, render).unwrap();
    for digest in [&expected.0, &expected.1] {
        fs::remove_file(root.path().join("store/sha256").join(digest.hex())).unwrap();
        let before = renders.get();
        assert_eq!(cache.prepare(request(), &store, render).unwrap(), expected);
        assert_eq!(renders.get(), before + 1);
    }
    let records = root.path().join("store/records");
    let record = fs::read_dir(&records)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let invalid = store.put(b"invalid record json").unwrap();
    for contents in ["not a digest", invalid.hex()] {
        fs::write(&record, contents).unwrap();
        let before = renders.get();
        assert_eq!(cache.prepare(request(), &store, render).unwrap(), expected);
        assert_eq!(renders.get(), before + 1);
    }
    // Integrity errors from preexisting corrupt content must remain errors.
    fs::write(
        root.path().join("store/sha256").join(expected.0.hex()),
        b"corrupt",
    )
    .unwrap();
    let error = cache.prepare(request(), &store, render).unwrap_err();
    assert!(error.to_string().contains("hash mismatch"), "{error}");
}

struct Transport;
impl ImageTransport for Transport {
    fn import(
        &self,
        _: &config::Service,
        directory: &Path,
        store: &ContentStore,
        _: Option<(&PreparationContext, &str)>,
        architecture: crate::guest::Architecture,
    ) -> Result<ImportedImage> {
        fs::create_dir_all(directory)?;
        let config = store.put(&serde_json::to_vec(&serde_json::json!({
            "os": "linux", "architecture": architecture.oci_name(), "config": {"Cmd": ["/app"]},
        }))?)?;
        let archive = directory.join("rootfs.tar");
        let mut tar = tar::Builder::new(fs::File::create(&archive)?);
        let mut header = tar::Header::new_gnu();
        header.set_uid(0);
        header.set_gid(0);
        header.set_mtime(0);
        header.set_mode(0o755);
        header.set_size(11);
        tar.append_data(&mut header, "rootfs/app", b"application".as_slice())?;
        tar.finish()?;
        Ok(ImportedImage {
            image: crate::execution::PreparedImage {
                manifest: store.put(b"manifest")?,
                config,
                layers: vec![store.put_file(&archive)?],
                launch: launch(),
            },
            rootfs: RootFilesystem::Archive(archive),
            auxiliary: BTreeMap::new(),
            provenance: BTreeMap::new(),
        })
    }
}

#[test]
fn prepared_boot_cache_matches_fresh_retention_and_cross_project_relative_files() {
    let root = tempfile::tempdir().unwrap();
    let (preparer, _, store) = fixture(root.path());
    let renders = Cell::new(0);
    struct CountBoot<'a>(TestBoot, &'a Cell<usize>);
    impl BootFactory for CountBoot<'_> {
        fn boot(&self, request: OciBoot<'_>, store: &ContentStore) -> Result<BootArchive> {
            self.1.set(self.1.get() + 1);
            self.0.boot(request, store)
        }
    }
    let prepare = |simulation: &config::Simulation, store: &ContentStore| {
        let evidence = tempfile::tempdir_in(root.path()).unwrap();
        let context =
            PreparationContext::new(evidence.path().join("evidence"), Default::default()).unwrap();
        let mut inputs = preparer
            .inner
            .prepare_with_factory(
                simulation,
                store,
                &context,
                &Transport,
                &preparer.runtime,
                (
                    || Ok(()),
                    |epoch, blocks| {
                        Ok(CountBoot(
                            TestBoot::new(&preparer.runtime, epoch, blocks)?,
                            &renders,
                        ))
                    },
                ),
            )
            .unwrap();
        seedbox_driver_execution::driver_world::preparation::retain_oci_prepared(
            &mut inputs,
            simulation,
            &preparer.runtime,
            store,
            &|| context.check(),
        )
        .unwrap();
        inputs
    };
    let project = |name: &str, bytes: &[u8]| {
        let directory = root.path().join(name);
        fs::create_dir(&directory).unwrap();
        fs::write(directory.join("config.txt"), bytes).unwrap();
        config::parse_for_architecture(
            "name: cache\nmachines: {}\noracle:\n  image: example/app\n  files: [{source: ./config.txt, target: /config}]\n",
            &directory.join("seedbox.yaml"), &BTreeMap::new(),
            crate::guest::Architecture::from_profile(&preparer.profile).unwrap(),
        ).unwrap()
    };
    let a = project("a", b"one");
    let b = project("b", b"two");
    let same = project("same", b"one");
    let first = prepare(&a, &store);
    let hit = prepare(&same, &store);
    assert_eq!(
        renders.get(),
        1,
        "equal relative file contents share boot inputs"
    );
    assert_eq!(first, hit);
    let different = prepare(&b, &store);
    assert_eq!(renders.get(), 2);
    assert_ne!(different.guests, first.guests);
    let path = root.path().join("a/config.txt");
    let modified = fs::metadata(&path).unwrap().modified().unwrap();
    fs::write(&path, b"two").unwrap();
    fs::File::open(&path)
        .unwrap()
        .set_times(fs::FileTimes::new().set_modified(modified))
        .unwrap();
    assert_eq!(different, prepare(&a, &store));
    assert_eq!(
        renders.get(),
        2,
        "changed bytes reuse the other project's matching content"
    );
    fs::write(path, b"one").unwrap();
    let fresh_store = ContentStore::new(root.path().join("fresh-store"));
    assert_eq!(first, prepare(&a, &fresh_store));
    assert_eq!(renders.get(), 3);
    for digest in first.digests() {
        store.verify(digest).unwrap();
        fresh_store.verify(digest).unwrap();
    }
    // Replay/exports retain all bytes independently of project files and records.
    for name in ["a", "b", "same"] {
        fs::remove_dir_all(root.path().join(name)).unwrap();
    }
    fs::remove_dir_all(root.path().join("store/records")).unwrap();
    for digest in first.digests() {
        store.verify(digest).unwrap();
    }
    assert!(
        seedbox_driver_execution::driver_world::retained::runtime_digest(&first, &store).is_ok()
    );
}

#[cfg(any(
    all(target_os = "linux", target_arch = "x86_64"),
    all(target_os = "macos", target_arch = "aarch64")
))]
#[test]
fn prepared_boot_cache_matches_production_pv_and_tcg_archives() {
    use seedbox_spec::backend::CpuBackend;
    for backend in [CpuBackend::Pv, CpuBackend::Tcg] {
        let root = tempfile::tempdir().unwrap();
        let (preparer, simulation, store) = fixture(root.path());
        let tcg_directory = tempfile::tempdir_in(root.path()).unwrap();
        let (_, tcg, _) = sdk(tcg_directory.path(), CpuBackend::Tcg);
        let runtime = if backend == CpuBackend::Pv {
            &preparer.runtime
        } else {
            &tcg
        };
        let epoch = 1_800_000_000;
        let blocks = std::num::NonZeroU32::new(17).unwrap().into();
        let factory = OciBootFactory::new(&preparer.engine, runtime, epoch, blocks).unwrap();
        let engine = store.put(b"engine identity").unwrap();
        let cache = PreparedBootCache::new(&engine, runtime, epoch, blocks, &store).unwrap();
        let mut launch = launch();
        launch.volumes.push("/anonymous".into());
        launch.disk_volumes.push(config::VolumeMount {
            source: "disk".into(),
            target: "/data".into(),
            size_bytes: None,
        });
        let configs = BTreeMap::from([("/config".into(), b"configuration".to_vec())]);
        let request = || OciBoot {
            name: "oracle",
            filesystem: filesystem(b"application"),
            launch: &launch,
            simulation: &simulation,
            configs: &configs,
            hosts: "127.0.0.1 localhost\n",
        };
        let expected = factory.boot(request(), &store).unwrap();
        let renders = Cell::new(0);
        let render = |request: OciBoot<'_>, store: &ContentStore| {
            renders.set(renders.get() + 1);
            factory.boot(request, store)
        };
        let first = cache.prepare(request(), &store, render).unwrap();
        // A new store handle exercises verification without the process-local
        // verification memo from publishing the objects.
        let reopened = ContentStore::new(root.path().join("store"));
        let hit = cache.prepare(request(), &reopened, render).unwrap();
        assert_eq!(renders.get(), 1);
        assert_eq!(first, hit);
        assert_eq!(store.read(&hit.0).unwrap(), expected.bytes);
        assert_eq!(hit.1, expected.filesystem);
        if backend == CpuBackend::Tcg {
            // TCG has different selected guest/host inputs from PV.
            for path in [
                runtime.guest_assets.join("firmware"),
                runtime.qemu.clone().unwrap(),
            ] {
                let original = fs::read(&path).unwrap();
                let mut changed = original.clone();
                changed[0] ^= 1;
                fs::write(&path, changed).unwrap();
                let cache =
                    PreparedBootCache::new(&engine, runtime, epoch, blocks, &store).unwrap();
                let before = renders.get();
                cache.prepare(request(), &store, render).unwrap();
                assert_eq!(renders.get(), before + 1);
                fs::write(path, original).unwrap();
            }
        }
    }
}

#[test]
fn prepared_boot_cache_hits_preserve_archive_memory_errors() {
    let root = tempfile::tempdir().unwrap();
    let (preparer, mut simulation, store) = fixture(root.path());
    simulation.machine.memory_mb = 64;
    let mut errors = vec![];
    for _ in 0..2 {
        let evidence = tempfile::tempdir_in(root.path()).unwrap();
        let context =
            PreparationContext::new(evidence.path().join("evidence"), Default::default()).unwrap();
        errors.push(
            preparer
                .prepare_with_transport(&simulation, &store, &context, &Transport)
                .unwrap_err()
                .to_string(),
        );
    }
    assert_eq!(errors[0], errors[1]);
    assert!(errors[0].contains("archive boot floor"), "{}", errors[0]);
}
