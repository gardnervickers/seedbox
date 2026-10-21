//! Real native SDK composition; preparation policy cases live beside the component.
#![cfg(all(target_os = "linux", target_arch = "x86_64"))]

// Shared synthetic package data only, not another crate's implementation.
// #[path] lets the preparation unit tests and this integration test share it
// without making it a public module of the library.
#[path = "../src/preparation/test_sdk.rs"]
mod test_sdk;
use seedbox_application::{
    config,
    execution::WitnessMode,
    oci::{ImageTransport, ImportedImage, RootFilesystem},
    preparation::{OciPreparer, PreparationContext},
    store::{ContentDigest, ContentStore},
};
use std::{collections::BTreeMap, fs, path::Path};
use test_sdk::sdk;
fn fixture(
    root: &Path,
) -> (
    OciPreparer,
    config::Simulation,
    ContentStore,
    seedbox_driver_execution::runtime_assets::Inputs,
) {
    use seedbox_application::execution::ExecutionBackend;
    use seedbox_driver_execution::driver_world::DriverExecutor;
    let (_, runtime, _) = sdk(root, seedbox_spec::backend::CpuBackend::Pv);
    let kernel = root.join("kernel");
    fs::copy(runtime.guest_assets.join("kernel"), &kernel).unwrap();
    let engine = std::env::current_exe().unwrap();
    let store = ContentStore::new(root.join("store"));
    let simulation = config::parse_for_architecture(
        "name: preparation-test\nmachines: {}\noracle: {image: example/app}\n",
        &root.join("seedbox.yaml"),
        &BTreeMap::new(),
        if cfg!(target_arch = "aarch64") {
            seedbox_application::guest::Architecture::Arm64
        } else {
            seedbox_application::guest::Architecture::Amd64
        },
    )
    .unwrap();
    let executor =
        DriverExecutor::for_preparation(&simulation, WitnessMode::Exits, Default::default())
            .unwrap();
    let preparer = OciPreparer {
        kernel,
        engine,
        init: runtime.guest_assets.join("init"),
        progress: runtime.guest_assets.join("progress.tar"),
        mke2fs: None,
        profile: executor.profile(),
        docker: bollard::Docker::connect_with_http(
            "http://127.0.0.1:9",
            10,
            bollard::API_DEFAULT_VERSION,
        )
        .unwrap(),
    };
    (preparer, simulation, store, runtime)
}

// Real OCI preparation composition with a cold image transport, not native or
// container-engine acceptance. The native CLI proof uses the ordinary engine.
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
#[test]
fn ordinary_oci_output_retains_a_complete_driver_factory_without_source_paths() {
    cold_driver_factory(seedbox_spec::backend::CpuBackend::Pv);
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
#[test]
fn ordinary_tcg_oci_output_needs_no_pv_progress_and_retains_firmware() {
    cold_driver_factory(seedbox_spec::backend::CpuBackend::Tcg);
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
fn cold_driver_factory(backend: seedbox_spec::backend::CpuBackend) {
    use seedbox_application::execution::ExecutionBackend;
    use seedbox_driver_execution::driver_world::{self, DriverExecutor};
    use seedbox_engine_protocol::driver_world as wire;
    struct Transport;
    impl ImageTransport for Transport {
        fn import(
            &self,
            service: &config::Service,
            directory: &Path,
            store: &ContentStore,
            _: Option<(&PreparationContext, &str)>,
            _: seedbox_application::guest::Architecture,
        ) -> seedbox_application::Result<ImportedImage> {
            fs::create_dir_all(directory)?;
            let config = br#"{"os":"linux","architecture":"amd64","config":{"Cmd":["/app"],"Volumes":{"/scratch":{}}}}"#;
            let digest = ContentDigest::of(config);
            fs::write(directory.join(digest.hex()), config)?;
            fs::write(
                directory.join("manifest.json"),
                serde_json::to_vec(&serde_json::json!({
                    "schemaVersion":2,"config":{"mediaType":"application/vnd.oci.image.config.v1+json","digest":format!("sha256:{}",digest.hex()),"size":config.len()},"layers":[]
                }))?,
            )?;
            let image = seedbox_application::oci::import_metadata(directory, service, store)?;
            let root = directory.join("root");
            fs::create_dir(&root)?;
            fs::write(root.join("app"), b"cold application bytes; never executed")?;
            // Match real ContainerTransport's prefixed archive, not just its
            // directory-shaped test double: the SDK supplies its own rootfs/.
            let archive = directory.join("rootfs.tar");
            let mut tar = tar::Builder::new(fs::File::create(&archive)?);
            tar.follow_symlinks(false);
            tar.append_dir_all("rootfs", &root)?;
            let mut header = tar::Header::new_gnu();
            header.set_mode(0o644);
            header.set_uid(0);
            header.set_gid(0);
            header.set_mtime(0);
            header.set_size(0);
            header.set_entry_type(tar::EntryType::Link);
            tar.append_link(&mut header, "rootfs/app-hard", "rootfs/app")?;
            header.set_entry_type(tar::EntryType::Symlink);
            tar.append_link(&mut header, "rootfs/app-link", "/app")?;
            tar.finish()?;
            Ok(ImportedImage {
                image,
                rootfs: RootFilesystem::Archive(archive),
                auxiliary: BTreeMap::new(),
                provenance: BTreeMap::new(),
            })
        }
    }
    let root = tempfile::tempdir().unwrap();
    let (mut preparer, mut simulation, store, _fixture_runtime) = fixture(root.path());
    let cpu_backend = backend;
    simulation
        .services
        .insert("side".into(), simulation.services["oracle"].clone());
    // A per-machine RAM override reaches the retained Driver machine.
    simulation.services.get_mut("side").unwrap().memory_mb = Some(512);
    let config = root.path().join("config");
    fs::write(&config, b"kept config").unwrap();
    simulation.configs.insert("settings".into(), config);
    let service = simulation.services.get_mut("oracle").unwrap();
    service.configs.push(seedbox_spec::ConfigMount {
        source: "settings".into(),
        target: "/etc/settings".into(),
    });
    service.volumes.push(seedbox_spec::VolumeMount {
        source: "data".into(),
        target: "/data".into(),
    });
    preparer.mke2fs = Some(preparer.init.clone());
    let (runtime, selected, package) = sdk(root.path(), backend);
    preparer.kernel = selected.guest_assets.join("kernel");
    preparer.init = selected.guest_assets.join("init");
    preparer.progress = selected.guest_assets.join("progress.tar");
    preparer.mke2fs = Some(selected.guest_assets.join("mke2fs"));
    preparer.engine = std::env::current_exe().unwrap();
    simulation.control = Some(seedbox_application::config::Control {
        service: "oracle".into(),
    });
    let executor =
        DriverExecutor::for_preparation(&simulation, WitnessMode::Exits, cpu_backend).unwrap();
    preparer.profile = executor.profile();
    let context =
        PreparationContext::new(root.path().join("evidence"), Default::default()).unwrap();
    let mut inputs = preparer
        .prepare_driver_with_transport(&simulation, &store, &context, &Transport, &selected)
        .unwrap();
    if backend == seedbox_spec::backend::CpuBackend::Tcg {
        assert!(
            !preparer.progress.exists(),
            "TCG package deliberately lacks progress.tar"
        );
        assert!(!inputs.auxiliary.contains_key("guest-progress"));
        assert!(inputs.auxiliary.contains_key("guest-firmware"));
    }
    let original_boots = inputs.guests.clone();
    seedbox_driver_execution::driver_world::preparation::retain_oci_prepared(
        &mut inputs,
        &simulation,
        &selected,
        &store,
        &|| context.check(),
    )
    .unwrap();
    for runtime_asset in [
        "guest-init",
        "guest-progress",
        "guest-firmware",
        "guest-mke2fs",
    ] {
        assert!(!inputs.auxiliary.contains_key(runtime_asset));
    }
    assert_eq!(
        inputs.guests, original_boots,
        "actual OCI boot bytes must not be replaced"
    );
    let prepared: wire::Prepared = serde_json::from_slice(
        &store
            .read(&inputs.auxiliary["driver-world/manifest"])
            .unwrap(),
    )
    .unwrap();
    let view_digest = ContentDigest::try_from(prepared.oracle.filesystem.sha256.clone()).unwrap();
    let view = store.read(&view_digest).unwrap();
    let names = tar::Archive::new(&view[..])
        .entries()
        .unwrap()
        .map(|entry| entry.unwrap().path().unwrap().into_owned())
        .collect::<Vec<_>>();
    assert!(names.iter().any(|path| path == Path::new("app")));
    assert!(!names.iter().any(|path| path.starts_with("seedbox")));
    assert!(!names.iter().any(|path| path.starts_with("rootfs")));
    let links = tar::Archive::new(&view[..])
        .entries()
        .unwrap()
        .filter_map(|entry| {
            let entry = entry.unwrap();
            entry
                .link_name()
                .unwrap()
                .map(|link| (entry.path().unwrap().into_owned(), link.into_owned()))
        })
        .collect::<BTreeMap<_, _>>();
    assert_eq!(links[Path::new("app-hard")], Path::new("app"));
    assert_eq!(links[Path::new("app-link")], Path::new("/app"));
    assert_eq!(prepared.oracle.name, "oracle");
    assert_eq!(prepared.machines[0].name, "side");
    assert_eq!(
        inputs.machine_memory_mb,
        BTreeMap::from([
            ("oracle".into(), simulation.machine.memory_mb),
            ("side".into(), 512)
        ])
    );
    assert_eq!(prepared.machines[0].memory_bytes, 512 << 20);
    assert_eq!(
        prepared.oracle.memory_bytes,
        u64::from(simulation.machine.memory_mb) << 20
    );
    assert_eq!(
        prepared.oracle.process.argv,
        inputs.images["oracle"].launch.argv
    );
    assert_eq!(
        prepared
            .oracle
            .interface
            .as_ref()
            .unwrap()
            .address
            .to_string(),
        "10.0.0.1"
    );
    assert_eq!(
        prepared.machines[0]
            .interface
            .as_ref()
            .unwrap()
            .address
            .to_string(),
        "10.0.0.2"
    );
    assert!(matches!(
        prepared.oracle.disk,
        Some(wire::Disk::Zero { bytes: 1073741824 })
    ));
    assert!(prepared.machines[0].disk.is_none());
    let (boot_archives, environments, kernel) = match &prepared.inventory {
        wire::Inventory::Pv {
            boot_archives,
            environments,
            kernel,
            ..
        } => {
            assert_eq!(backend, seedbox_spec::backend::CpuBackend::Pv);
            (boot_archives, environments, kernel)
        }
        wire::Inventory::Tcg {
            boot_archives,
            environments,
            kernel,
            firmware,
            executable,
            ..
        } => {
            assert_eq!(backend, seedbox_spec::backend::CpuBackend::Tcg);
            assert_eq!(
                firmware.sha256,
                ContentDigest::of(&fs::read(selected.guest_assets.join("firmware")).unwrap()).hex()
            );
            assert_eq!(
                executable.sha256,
                ContentDigest::of(&fs::read(selected.qemu.as_ref().unwrap()).unwrap()).hex()
            );
            (boot_archives, environments, kernel)
        }
    };
    assert_eq!(
        kernel.sha256,
        ContentDigest::of(&fs::read(selected.guest_assets.join("kernel")).unwrap()).hex()
    );
    assert_eq!(
        boot_archives["oracle"].sha256,
        inputs.guests["oracle"].hex()
    );
    assert_eq!(environments["oracle"].named_volumes[0].name, "data");
    assert_eq!(environments["oracle"].anonymous_volumes, vec!["/scratch"]);
    assert_eq!(
        environments["oracle"].configs["/etc/settings"].sha256,
        ContentDigest::of(b"kept config").hex()
    );
    assert!(environments["oracle"].hosts.contains("10.0.0.2 side"));
    assert!(environments["oracle"].control_socket);
    assert!(!environments["side"].control_socket);
    assert_eq!(prepared.oracle.name, "oracle");
    assert!(prepared
        .inventory
        .artifacts()
        .iter()
        .all(|asset| asset.artifact.starts_with("asset:")));
    let digest = driver_world::runtime_digest(&inputs, &store).unwrap();
    drop(selected);
    drop(runtime);
    fs::remove_file(package).unwrap();
    store.verify(&digest).unwrap();
    for digest in inputs.digests() {
        store.verify(digest).unwrap();
    }
}
