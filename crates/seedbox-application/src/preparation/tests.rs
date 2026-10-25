//! Preparation policy and production archive wiring with synthetic guest assets.
//! No hypervisor, kernel boot or application execution occurs.
use super::*;
use crate as seedbox_application;
use crate::{
    config,
    execution::{CancellationToken, ExecutionProfile, SemanticParameters, WitnessMode},
    oci::{ImageTransport, ImportedImage, RootFilesystem},
    store::ContentDigest,
};
use seedbox_driver_execution::test_support::sdk;
use std::path::Path;

mod cache;

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
        let mut inputs = self.inner.prepare_with_factory(
            simulation,
            store,
            context,
            transport,
            &self.runtime,
            (
                || Ok(()),
                |epoch, blocks| TestBoot::new(&self.runtime, epoch, blocks),
            ),
        )?;
        seedbox_driver_execution::driver_world::preparation::retain_oci_prepared(
            &mut inputs,
            simulation,
            &self.runtime,
            store,
            &|| context.check(),
        )?;
        Ok(inputs)
    }
}

fn fixture(root: &Path) -> (FixturePreparer, config::Simulation, ContentStore) {
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
    let preparer = OciPreparer {
        kernel,
        engine,
        init: runtime.guest_assets.join("init"),
        progress: runtime.guest_assets.join("progress.tar"),
        mke2fs: None,
        profile: ExecutionProfile {
            backend: "driver-pv".into(),
            witness_mode: WitnessMode::Exits,
            semantic_parameters: SemanticParameters {
                engine: Some(DriverExecutor::descriptor(
                    if cfg!(target_arch = "aarch64") {
                        "aarch64"
                    } else {
                        "x86_64"
                    },
                )),
                ..Default::default()
            },
        },
        docker: bollard::Docker::connect_with_http(
            "http://127.0.0.1:9",
            10,
            bollard::API_DEFAULT_VERSION,
        )
        .unwrap(),
    };
    (
        FixturePreparer {
            inner: preparer,
            runtime,
        },
        simulation,
        store,
    )
}

// Any import here is an ordering violation, not a network failure.
struct NoImport;
impl ImageTransport for NoImport {
    fn import(
        &self,
        _: &config::Service,
        _: &Path,
        _: &ContentStore,
        _: Option<(&PreparationContext, &str)>,
        _: crate::guest::Architecture,
    ) -> Result<ImportedImage> {
        panic!("preparation must refuse before image import")
    }
}

#[test]
fn cancelled_preparation_never_imports_images() {
    let root = tempfile::tempdir().unwrap();
    let (preparer, simulation, store) = fixture(root.path());
    let token = CancellationToken::default();
    token.cancel();
    let context = PreparationContext::new(root.path().join("evidence"), token).unwrap();
    let result = preparer.inner.prepare_with_factory(
        &simulation,
        &store,
        &context,
        &NoImport,
        &preparer.runtime,
        (
            || panic!("cancellation must precede runtime validation"),
            |_, _| -> Result<TestBoot> { panic!("cancelled preparation must not build a factory") },
        ),
    );
    assert!(matches!(result, Err(Error::PreparationCancelled)));
    assert!(!context.directory.join("setup.json").exists());
}

#[test]
fn invalid_machine_memory_fails_before_image_import() {
    let root = tempfile::tempdir().unwrap();
    let (mut preparer, mut simulation, store) = fixture(root.path());
    simulation.machine.memory_mb = 32;
    for (guest, platform, maximum) in [
        ("x86_64", "linux/amd64", 65536),
        ("aarch64", "linux/arm64", 3072),
    ] {
        preparer
            .profile
            .semantic_parameters
            .engine
            .as_mut()
            .unwrap()
            .guest_arch = guest.into();
        simulation.services.get_mut("oracle").unwrap().platform = platform.into();
        let context = PreparationContext::new(root.path().join(guest), Default::default()).unwrap();
        let error = preparer
            .prepare_with_transport(&simulation, &store, &context, &NoImport)
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            format!("machine oracle: memoryMiB must be 64..{maximum}")
        );
    }
}

#[test]
fn runtime_validation_failure_precedes_image_import_and_factory_construction() {
    let root = tempfile::tempdir().unwrap();
    let (preparer, simulation, store) = fixture(root.path());
    let context =
        PreparationContext::new(root.path().join("evidence"), Default::default()).unwrap();
    let error = preparer
        .inner
        .prepare_with_factory(
            &simulation,
            &store,
            &context,
            &NoImport,
            &preparer.runtime,
            (
                || Err(Error::Setup("fixture runtime validation failed".into())),
                |_, _| -> Result<TestBoot> { panic!("invalid runtime must not build a factory") },
            ),
        )
        .unwrap_err();
    assert_eq!(error.to_string(), "fixture runtime validation failed");
    assert!(!context.directory.join("setup.json").exists());
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
            architecture: seedbox_application::guest::Architecture,
        ) -> seedbox_application::Result<ImportedImage> {
            fs::create_dir_all(directory)?;
            let config = serde_json::to_vec(&serde_json::json!({
                "os": "linux", "architecture": architecture.oci_name(),
                "config": {"Cmd": ["/app"]},
            }))?;
            let digest = ContentDigest::of(&config);
            fs::write(directory.join(digest.hex()), &config)?;
            fs::write(
                directory.join("manifest.json"),
                serde_json::to_vec(
                    &serde_json::json!({"schemaVersion":2,"config":{"mediaType":"application/vnd.oci.image.config.v1+json","digest":format!("sha256:{}",digest.hex()),"size":config.len()},"layers":[]}),
                )?,
            )?;
            let image = seedbox_application::oci::import_metadata_for_arch(
                directory,
                service,
                store,
                architecture,
            )?;
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
            architecture: seedbox_application::guest::Architecture,
        ) -> seedbox_application::Result<seedbox_application::oci::ImportedImage> {
            fs::create_dir_all(directory)?;
            fs::File::open(directory)?
                .set_times(fs::FileTimes::new().set_modified(std::time::UNIX_EPOCH))?;
            Ok(seedbox_application::oci::ImportedImage {
                image: seedbox_application::execution::PreparedImage {
                    manifest: store.put(b"manifest")?,
                    config: store.put(
                        format!(
                            r#"{{"os":"linux","architecture":"{}","config":{{"Cmd":["/app"]}},"created":"2026-01-01T00:00:00Z","history":[{{"created":"{}"}}]}}"#,
                            architecture.oci_name(),
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
            assert_boot_epoch_and_budget(&archive, epoch, budget);
        }
    }
}

fn assert_boot_epoch_and_budget(archive: &[u8], epoch: u32, budget: u32) {
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
        let count = u32::from_le_bytes(archive[*cursor..*cursor + 4].try_into().unwrap()) as usize;
        *cursor += 4;
        (0..count)
            .map(|_| {
                let len =
                    u32::from_le_bytes(archive[*cursor..*cursor + 4].try_into().unwrap()) as usize;
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

// Exercise the production constructor and archive assembly, independently of
// TestBoot's policy seam. Loading these synthetic assets does not start a worker
// or require KVM/HVF. Linux ARM remains unsupported by OciBootFactory.
#[cfg(any(
    all(target_os = "linux", target_arch = "x86_64"),
    all(target_os = "macos", target_arch = "aarch64")
))]
#[test]
fn production_oci_boot_factory_embeds_epoch_and_progress_budget() {
    let root = tempfile::tempdir().unwrap();
    let (preparer, simulation, store) = fixture(root.path());
    let launch = crate::image::Launch {
        argv: vec!["/app".into()],
        environment: BTreeMap::new(),
        working_dir: "/".into(),
        user: "".into(),
        volumes: vec![],
        disk_volumes: vec![],
    };
    let filesystem = tar::Builder::new(Vec::new()).into_inner().unwrap();
    let configs = BTreeMap::new();
    for (epoch, budget) in [(86_400, 1), (1_800_000_000, 17), (u32::MAX, u32::MAX)] {
        let factory = OciBootFactory::new(
            &preparer.engine,
            &preparer.runtime,
            epoch,
            std::num::NonZeroU32::new(budget).unwrap().into(),
        )
        .unwrap();
        let boot = factory
            .boot(
                OciBoot {
                    name: "oracle",
                    filesystem: filesystem.clone(),
                    launch: &launch,
                    simulation: &simulation,
                    configs: &configs,
                    hosts: "127.0.0.1 localhost\n",
                },
                &store,
            )
            .unwrap();
        assert_eq!(store.read(&boot.filesystem).unwrap(), filesystem);
        assert_boot_epoch_and_budget(&boot.bytes, epoch, budget);
    }
}

// An explicit test dependency, not a native admission or boot correctness oracle.
// Epoch/budget come through the same private preparation path as production.
struct TestBoot {
    bootstrap: seedbox_machine::application::GuestBootstrap,
    blocks: seedbox_spec::progress::ProgressBudget,
}
impl TestBoot {
    fn new(
        runtime: &seedbox_driver_execution::runtime_assets::Inputs,
        epoch: u32,
        blocks: seedbox_spec::progress::ProgressBudget,
    ) -> Result<Self> {
        Ok(Self {
            bootstrap: seedbox_machine::application::GuestBootstrap {
                kernel: seedbox_machine::application::Kernel {
                    image: fs::read(runtime.guest_assets.join("kernel"))?,
                    command_line: "rdinit=/init".into(),
                },
                init: fs::read(runtime.guest_assets.join("init"))?,
                base_initramfs: vec![],
                epoch_seconds: epoch,
            },
            blocks,
        })
    }
}
impl BootFactory for TestBoot {
    fn boot(&self, request: OciBoot<'_>, store: &ContentStore) -> Result<BootArchive> {
        use seedbox_machine::{application, definition};
        let filesystem = store.put(&request.filesystem)?;
        let application = application::DerivedApplication {
            application: definition::PreparedApplication {
                filesystem: definition::FilesystemArchive::new(request.filesystem)
                    .map_err(|error| Error::Integrity(error.to_string()))?,
                launch: definition::Launch {
                    argv: request.launch.argv.clone(),
                    environment: request.launch.environment.clone(),
                    working_directory: request.launch.working_dir.clone(),
                    user: request.launch.user.clone(),
                },
            },
            execution_prefix: std::iter::once("/fixture/progress".into())
                .chain(self.blocks.translator_arguments())
                .collect(),
        };
        let mut environment =
            application::GuestEnvironment::new(request.name, request.hosts, false);
        environment.configs = request.configs.clone();
        let bytes =
            application::boot_derived_configured(&application, &self.bootstrap, &environment, None)
                .map_err(|error| Error::Integrity(error.to_string()))?;
        Ok(BootArchive { bytes, filesystem })
    }
}
