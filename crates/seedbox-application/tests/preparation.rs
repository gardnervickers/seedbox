//! Native preparation retains immutable inputs and honors cancellation.
use seedbox_application::{
    config,
    execution::{CancellationToken, WitnessMode},
    oci::{ImageTransport, ImportedImage, RootFilesystem},
    preparation::{OciPreparer, PreparationContext},
    store::{ContentDigest, ContentStore},
    Error,
};
use std::{collections::BTreeMap, fs, path::Path};
struct FixturePreparer {
    inner: OciPreparer,
    runtime: seedbox_driver_execution::runtime_assets::Inputs,
}
impl std::ops::Deref for FixturePreparer {
    type Target = OciPreparer;
    fn deref(&self) -> &OciPreparer {
        &self.inner
    }
}
impl std::ops::DerefMut for FixturePreparer {
    fn deref_mut(&mut self) -> &mut OciPreparer {
        &mut self.inner
    }
}
impl FixturePreparer {
    fn prepare_with_transport(
        &self,
        simulation: &config::Simulation,
        store: &ContentStore,
        context: &PreparationContext,
        transport: &dyn ImageTransport,
    ) -> seedbox_application::Result<seedbox_application::execution::PreparedInputs> {
        let mut inputs = self.inner.prepare_driver_with_transport(
            simulation,
            store,
            context,
            transport,
            &self.runtime,
        )?;
        seedbox_application::preparation::retain_oci_prepared(
            &mut inputs,
            simulation,
            &self.runtime,
            store,
            context,
        )?;
        Ok(inputs)
    }
    fn prepare_cancellable(
        &self,
        simulation: &config::Simulation,
        store: &ContentStore,
        context: &PreparationContext,
    ) -> seedbox_application::Result<seedbox_application::execution::PreparedInputs> {
        self.prepare_with_transport(
            simulation,
            store,
            context,
            &seedbox_application::container_images::ContainerTransport {
                docker: self.docker.clone(),
                cancellation: context.cancellation.clone(),
                progress: |_| {},
                export_cache: None,
            },
        )
    }
    fn prepare(
        &self,
        simulation: &config::Simulation,
        store: &ContentStore,
    ) -> seedbox_application::Result<seedbox_application::execution::PreparedInputs> {
        let temporary = tempfile::tempdir()?;
        let context =
            PreparationContext::new(temporary.path().join("prepare"), Default::default())?;
        self.prepare_cancellable(simulation, store, &context)
    }
}
fn fixture(root: &Path) -> (FixturePreparer, config::Simulation, ContentStore) {
    use seedbox_application::execution::ExecutionBackend;
    use seedbox_driver_execution::driver_world::DriverExecutor;
    let (_, runtime, _) = sdk(root, seedbox_driver_execution::runtime_assets::Backend::Pv);
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
    let executor = DriverExecutor::for_preparation(
        &simulation,
        WitnessMode::Exits,
        Default::default(),
    )
    .unwrap();
    let inner = OciPreparer {
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
    (FixturePreparer { inner, runtime }, simulation, store)
}

#[test]
fn cancelled_preparation_never_imports_images() {
    let root = tempfile::tempdir().unwrap();
    let (preparer, simulation, store) = fixture(root.path());
    let token = CancellationToken::default();
    token.cancel();
    let context = PreparationContext::new(root.path().join("evidence"), token).unwrap();
    assert!(matches!(
        preparer.prepare_cancellable(&simulation, &store, &context),
        Err(Error::PreparationCancelled)
    ));
    assert!(!context.directory.join("setup.json").exists());
}
#[test]
fn invalid_machine_memory_fails_before_image_import() {
    let root = tempfile::tempdir().unwrap();
    let (preparer, mut simulation, store) = fixture(root.path());
    simulation.machine.memory_mb = 32;
    assert!(preparer
        .prepare(&simulation, &store)
        .unwrap_err()
        .to_string()
        .contains("memoryMiB must be 64..65536"));
}
#[test]
fn preparation_retains_kernel_before_transport_changes_source() {
    struct Transport<'a>(&'a Path);
    impl ImageTransport for Transport<'_> {
        fn import(
            &self,
            service: &config::Service,
            directory: &Path,
            store: &ContentStore,
            _: Option<(&PreparationContext, &str)>,
            _: seedbox_application::guest::Architecture,
        ) -> seedbox_application::Result<ImportedImage> {
            fs::create_dir_all(directory)?;
            let config = br#"{"os":"linux","architecture":"amd64","config":{"Cmd":["/app"]}}"#;
            let digest = ContentDigest::of(config);
            fs::write(directory.join(digest.hex()), config)?;
            fs::write(
                directory.join("manifest.json"),
                serde_json::to_vec(
                    &serde_json::json!({"schemaVersion":2,"config":{"mediaType":"application/vnd.oci.image.config.v1+json","digest":format!("sha256:{}",digest.hex()),"size":config.len()},"layers":[]}),
                )?,
            )?;
            let image = seedbox_application::oci::import_metadata(directory, service, store)?;
            let root = directory.join("root");
            fs::create_dir(&root)?;
            fs::write(root.join("app"), b"application")?;
            fs::write(self.0, b"changed")?;
            Ok(ImportedImage {
                image,
                rootfs: RootFilesystem::Directory(root),
                auxiliary: BTreeMap::new(),
                provenance: BTreeMap::new(),
            })
        }
    }
    let root = tempfile::tempdir().unwrap();
    let (preparer, simulation, store) = fixture(root.path());
    let context =
        PreparationContext::new(root.path().join("evidence"), Default::default()).unwrap();
    let original_kernel = fs::read(&preparer.kernel).unwrap();
    let inputs = preparer
        .prepare_with_transport(&simulation, &store, &context, &Transport(&preparer.kernel))
        .unwrap();
    assert!(inputs.auxiliary.contains_key("driver-world/manifest"));
    assert_eq!(fs::read(&preparer.kernel).unwrap(), b"changed");
    assert_eq!(
        store.read(&ContentDigest::of(&original_kernel)).unwrap(),
        original_kernel
    );
    let receipt: serde_json::Value =
        serde_json::from_slice(&fs::read(context.directory.join("setup.json")).unwrap()).unwrap();
    assert_eq!(
        receipt["kernel"],
        serde_json::to_value(ContentDigest::of(&original_kernel)).unwrap()
    );
}

#[test]
fn preparation_derives_the_epoch_from_image_build_times_unless_resolved_configuration_sets_it() {
    // Each image config records its own build time; the latest one wins.
    struct Transport;
    impl seedbox_application::oci::ImageTransport for Transport {
        fn import(
            &self,
            service: &seedbox_application::config::Service,
            directory: &Path,
            store: &ContentStore,
            _context: Option<(&PreparationContext, &str)>,
            _architecture: seedbox_application::guest::Architecture,
        ) -> seedbox_application::Result<seedbox_application::oci::ImportedImage> {
            fs::create_dir_all(directory)?;
            fs::File::open(directory)?
                .set_times(fs::FileTimes::new().set_modified(std::time::UNIX_EPOCH))?;
            Ok(seedbox_application::oci::ImportedImage {
                image: seedbox_application::execution::PreparedImage {
                    manifest: store.put(b"manifest")?,
                    config: store.put(
                        format!(
                            r#"{{"os":"linux","architecture":"amd64","config":{{"Cmd":["/app"]}},"created":"2026-01-01T00:00:00Z","history":[{{"created":"{}"}}]}}"#,
                            match &service.source {
                                config::ImageSource::Registry { reference } if reference == "example/late" => "2026-10-09T04:05:33.5Z",
                                _ => "2026-10-01T00:00:00Z",
                            }
                        )
                        .as_bytes(),
                    )?,
                    layers: vec![],
                    launch: seedbox_application::image::Launch {
                        argv: vec!["/app".into()],
                        environment: BTreeMap::new(),
                        working_dir: "/".into(),
                        user: "".into(),
                        volumes: vec![],
                        disk_volumes: vec![],
                    },
                },
                rootfs: seedbox_application::oci::RootFilesystem::Directory(directory.into()),
                auxiliary: BTreeMap::new(),
                provenance: BTreeMap::new(),
            })
        }
    }
    let root = tempfile::tempdir().unwrap();
    let (preparer, mut simulation, store) = fixture(root.path());
    let mut late = simulation.services["oracle"].clone();
    late.source = config::ImageSource::Registry {
        reference: "example/late".into(),
    };
    simulation.services.insert("other".into(), late);
    let context =
        PreparationContext::new(root.path().join("evidence"), CancellationToken::default())
            .unwrap();
    let prepared = preparer
        .prepare_with_transport(&simulation, &store, &context, &Transport)
        .unwrap();
    assert_eq!(prepared.wall_clock_epoch, Some(1_791_518_733));
    simulation.simulator.progress_budget = std::num::NonZeroU32::new(1).unwrap().into();
    let budget_context = PreparationContext::new(
        root.path().join("changed-budget"),
        CancellationToken::default(),
    )
    .unwrap();
    let changed_budget = preparer
        .prepare_with_transport(&simulation, &store, &budget_context, &Transport)
        .unwrap();
    assert_eq!(changed_budget.wall_clock_epoch, prepared.wall_clock_epoch);
    assert_ne!(changed_budget.guests, prepared.guests);
    simulation.simulator = Default::default();
    let json = serde_json::to_vec(&prepared).unwrap();
    let retained: seedbox_application::execution::PreparedInputs =
        serde_json::from_slice(&json).unwrap();
    assert_eq!(retained, prepared);
    for (requested, budget) in [None, Some(86400), Some(1_800_000_000), Some(u32::MAX)]
        .into_iter()
        .zip([10_000, 1, 17, u32::MAX])
    {
        simulation.simulator.progress_budget = std::num::NonZeroU32::new(budget).unwrap().into();
        simulation.wall_clock_epoch = requested;
        let label = requested.map_or("derived".into(), |epoch| epoch.to_string());
        let context = PreparationContext::new(
            root.path().join(format!("epoch-{label}")),
            Default::default(),
        )
        .unwrap();
        let first = preparer
            .prepare_with_transport(&simulation, &store, &context, &Transport)
            .unwrap();
        let epoch = requested.unwrap_or(1_791_518_733);
        assert_eq!(first.wall_clock_epoch, Some(epoch));
        assert_eq!(requested.is_none(), first.guests == prepared.guests);
        let second_context = PreparationContext::new(
            root.path().join(format!("epoch-{label}-second")),
            Default::default(),
        )
        .unwrap();
        let second = preparer
            .prepare_with_transport(&simulation, &store, &second_context, &Transport)
            .unwrap();
        assert_eq!(first, second);
        for digest in first.guests.values() {
            let archive = store.read(digest).unwrap();
            let clock = archive
                .windows(8)
                .position(|bytes| bytes == b"SBXCLK1\0")
                .unwrap()
                + 8;
            assert_eq!(
                u32::from_le_bytes(archive[clock..clock + 4].try_into().unwrap()),
                epoch
            );
            let start = archive
                .windows(8)
                .position(|bytes| bytes == b"SBXAPP2\0")
                .unwrap()
                + 8;
            let mut cursor = start;
            let read = |cursor: &mut usize| {
                let count =
                    u32::from_le_bytes(archive[*cursor..*cursor + 4].try_into().unwrap()) as usize;
                *cursor += 4;
                (0..count)
                    .map(|_| {
                        let len =
                            u32::from_le_bytes(archive[*cursor..*cursor + 4].try_into().unwrap())
                                as usize;
                        *cursor += 4;
                        let value = archive[*cursor..*cursor + len].to_vec();
                        *cursor += len;
                        value
                    })
                    .collect::<Vec<_>>()
            };
            let _argv = read(&mut cursor);
            let prefix = read(&mut cursor);
            let position = prefix
                .iter()
                .position(|value| value == b"--blocks-per-progress-call")
                .unwrap();
            assert_eq!(prefix[position + 1], budget.to_string().as_bytes());
        }
    }
}

// Real OCI preparation composition with a cold image transport, not native or
// container-engine acceptance. The native CLI proof uses the ordinary engine.
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
#[test]
fn ordinary_oci_output_retains_a_complete_driver_factory_without_source_paths() {
    cold_driver_factory(seedbox_driver_execution::runtime_assets::Backend::Pv);
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
#[test]
fn ordinary_tcg_oci_output_needs_no_pv_progress_and_retains_firmware() {
    cold_driver_factory(seedbox_driver_execution::runtime_assets::Backend::Tcg);
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
fn cold_driver_factory(backend: seedbox_driver_execution::runtime_assets::Backend) {
    use seedbox_application::execution::ExecutionBackend;
    use seedbox_driver_execution::driver_world::{self, DriverExecutor};
    use seedbox_driver_execution::runtime_assets;
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
    let (mut preparer, mut simulation, store) = fixture(root.path());
    let cpu_backend = match backend {
        runtime_assets::Backend::Pv => seedbox_spec::backend::CpuBackend::Pv,
        runtime_assets::Backend::Tcg => seedbox_spec::backend::CpuBackend::Tcg,
    };
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
    let executor = DriverExecutor::for_preparation(
        &simulation,
        WitnessMode::Exits,
        cpu_backend,
    )
    .unwrap();
    preparer.profile = executor.profile();
    let context =
        PreparationContext::new(root.path().join("evidence"), Default::default()).unwrap();
    let mut inputs = preparer
        .prepare_driver_with_transport(&simulation, &store, &context, &Transport, &selected)
        .unwrap();
    if backend == runtime_assets::Backend::Tcg {
        assert!(
            !preparer.progress.exists(),
            "TCG package deliberately lacks progress.tar"
        );
        assert!(!inputs.auxiliary.contains_key("guest-progress"));
        assert!(inputs.auxiliary.contains_key("guest-firmware"));
    }
    let original_boots = inputs.guests.clone();
    seedbox_application::preparation::retain_oci_prepared(
        &mut inputs,
        &simulation,
        &selected,
        &store,
        &context,
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
            ("app".into(), simulation.machine.memory_mb),
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
            assert_eq!(backend, runtime_assets::Backend::Pv);
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
            assert_eq!(backend, runtime_assets::Backend::Tcg);
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

fn sdk(
    root: &Path,
    backend: seedbox_driver_execution::runtime_assets::Backend,
) -> (
    seedbox_driver_execution::runtime_assets::Runtime,
    seedbox_driver_execution::runtime_assets::Inputs,
    std::path::PathBuf,
) {
    use seedbox_driver_execution::runtime_assets;
    let package = root.join("package.tar.gz");
    let mut tar = tar::Builder::new(flate2::write::GzEncoder::new(
        fs::File::create(&package).unwrap(),
        flate2::Compression::fast(),
    ));
    let mut append = |name: &str, bytes: &[u8]| {
        let mut header = tar::Header::new_gnu();
        header.set_uid(0);
        header.set_gid(0);
        header.set_mtime(0);
        header.set_mode(if name.contains("/bin/") || name.contains("/libexec/") {
            0o755
        } else {
            0o644
        });
        header.set_size(bytes.len() as u64);
        header.set_cksum();
        tar.append_data(&mut header, name, bytes).unwrap();
    };
    let system = format!(
        "{}-{}",
        std::env::consts::ARCH,
        if cfg!(target_os = "macos") {
            "darwin"
        } else {
            "linux"
        }
    );
    append("host-system", system.as_bytes());
    let mut kernel = seedbox_machine::test_support::assets::bzimage_header();
    kernel[0x22c..0x230].copy_from_slice(&0x7fffffffu32.to_le_bytes());
    let mut arm_kernel = vec![0; 65536];
    arm_kernel[8..16].copy_from_slice(&0x80000_u64.to_le_bytes());
    arm_kernel[16..24].copy_from_slice(&65536_u64.to_le_bytes());
    arm_kernel[56..60].copy_from_slice(b"ARM\x64");
    let arm_elf = seedbox_machine::test_support::assets::image(183);
    append("pv/aarch64/kernel", &arm_kernel);
    append("pv/aarch64/init", &arm_elf);
    append("pv/aarch64/mke2fs", &arm_elf);
    append("pv/aarch64/command-line", b"rdinit=/init");
    append("pv/x86_64/kernel", &kernel);
    let elf = seedbox_machine::test_support::assets::image(62);
    append("pv/x86_64/init", &elf);
    append("pv/x86_64/mke2fs", &elf);
    append("pv/x86_64/command-line", b"rdinit=/init");
    append("tcg/x86_64/kernel", &kernel);
    append("tcg/x86_64/init", &elf);
    append("tcg/x86_64/mke2fs", &elf);
    append("tcg/x86_64/command-line", b"rdinit=/init");
    append("tcg/x86_64/firmware", &vec![3; 64 * 1024]);
    // Host QEMU members live under the system recorded in host-system.
    let host = |name: &str| format!("host/{system}/{name}");
    append(&host("bin/qemu-system-x86_64"), b"#!/bin/sh\nexit 0\n");
    append(&host("libexec/qemu-system-x86_64"), &elf);
    append(&host("lib/loader"), b"cold library");
    append(&host("share/qemu/qboot.rom"), b"cold firmware");
    let mut progress = tar::Builder::new(Vec::new());
    for name in [
        "bin64/drrun",
        "libseedbox_progress.so",
        "lib64/release/libdynamorio.so",
        "lib64/release/libdrpreload.so",
        "ext/lib64/release/libdrmgr.so",
        "ext/lib64/release/libdrreg.so",
        "lib/ld-linux-x86-64.so.2",
        "lib/libc.so.6",
        "lib/libm.so.6",
        "lib/libdl.so.2",
        "lib/libpthread.so.0",
        "lib/librt.so.1",
        "DynamoRIO-LICENSE.txt",
        "DynamoRIO-ACKNOWLEDGEMENTS.txt",
        "glibc-COPYING.LIB",
    ] {
        let bytes = if name.ends_with(".txt") || name.ends_with(".LIB") {
            b"cold license".as_slice()
        } else {
            elf.as_slice()
        };
        let mut header = tar::Header::new_gnu();
        header.set_uid(0);
        header.set_gid(0);
        header.set_mtime(0);
        header.set_mode(0o755);
        header.set_size(bytes.len() as u64);
        header.set_cksum();
        progress
            .append_data(&mut header, format!("seedbox/progress/{name}"), bytes)
            .unwrap();
    }
    let progress = progress.into_inner().unwrap();
    append("pv/x86_64/progress.tar", &progress);
    let mut arm_progress = Vec::new();
    let mut builder = tar::Builder::new(&mut arm_progress);
    for entry in tar::Archive::new(progress.as_slice()).entries().unwrap() {
        let mut entry = entry.unwrap();
        let mut name = entry.path().unwrap().into_owned();
        // The aarch64 runtime carries its own ELF loader name.
        if name.ends_with("lib/ld-linux-x86-64.so.2") {
            name = "seedbox/progress/lib/ld-linux-aarch64.so.1".into();
        }
        let bytes = if name.to_string_lossy().ends_with(".txt")
            || name.to_string_lossy().ends_with(".LIB")
        {
            b"cold license".as_slice()
        } else {
            arm_elf.as_slice()
        };
        let mut header = tar::Header::new_gnu();
        header.set_uid(0);
        header.set_gid(0);
        header.set_mtime(0);
        header.set_mode(0o755);
        header.set_size(bytes.len() as u64);
        header.set_cksum();
        builder.append_data(&mut header, name, bytes).unwrap();
        std::io::copy(&mut entry, &mut std::io::sink()).unwrap();
    }
    builder.finish().unwrap();
    drop(builder);
    append("pv/aarch64/progress.tar", &arm_progress);
    tar.into_inner().unwrap().finish().unwrap();
    let runtime = runtime_assets::Runtime::from_archive(&package, root).unwrap();
    let selected = runtime
        .select(
            backend,
            if backend == runtime_assets::Backend::Pv && cfg!(target_arch = "aarch64") {
                seedbox_spec::definition::Architecture::Aarch64
            } else {
                seedbox_spec::definition::Architecture::X86_64
            },
        )
        .unwrap();
    (runtime, selected, package)
}
