//! Native public-service acceptance, not a fixture executor or protocol model.
//! Uses the normal release CLI/runtime package and existing finite CPU fixture.

#[cfg(feature = "kvm-tests")]
#[path = "native_controller.rs"]
mod controller;
#[cfg(feature = "kvm-tests")]
pub use controller::{pv_controller_checkpoint, pv_controller_repeat};
#[cfg(feature = "kvm-tests")]
#[path = "native_export.rs"]
mod export;
#[cfg(feature = "kvm-tests")]
pub use export::pv_large_export_under_load;
#[path = "native_held_checkpoint.rs"]
mod held_checkpoint;
use futures_util::StreamExt;
pub use held_checkpoint::native_held_checkpoint;
use seedbox_api::{
    ComparisonStatus, ExecutionTermination, Phase, SimulationSpec, SubmitRequest, TestVerdict,
};
pub use seedbox_application::driver_world::DriverBackend;
use seedbox_application::{
    driver_world::{self, DriverExecutor},
    execution::{
        Backend, CancellationToken, ExecutionBackend, ExecutionProfile, PreparedImage,
        PreparedInputs, SemanticParameters, WitnessMode,
    },
    image::Launch,
    process_lifetime::{reclaim_barrier, EXECUTION_LEASE_FILE, SESSION_LEASE_FILE},
    runtime_assets::{self, Runtime},
    store::{ContentDigest, ContentStore},
};
use seedbox_client::Client;
use seedbox_engine_protocol::driver_world as wire;
use seedbox_machine::{
    application::GuestEnvironment,
    definition::{
        FilesystemArchive, Launch as GuestLaunch, MachineDefinition, PreparedApplication, Resources,
    },
};
use seedbox_server::{
    catalogue::Catalogue,
    execution::{BackendProvider, Backends, ExecutionService},
    manager::Manager,
};
use std::{
    collections::BTreeMap,
    fs,
    num::{NonZeroU32, NonZeroUsize},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

/// Simulated time by which an execution must finish, or, when it is cancelled,
/// show its application's `START=` output. This is the failure condition: a
/// guest that keeps running without getting there fails at the same simulated
/// point on any host. Measured with the cpu-loop fixture: TCG finishes 200k
/// and 400k iterations, and prints `START=`, by 1.962 s; PV finishes 200k by
/// 21 ms and the checkpoint scenario's 2M by 133 ms; the controller fixture's
/// one 20 ms sleep is far shorter. The bounds are about 20 times the largest.
pub fn simulated_bound(backend: DriverBackend) -> u64 {
    match backend {
        DriverBackend::Pv => 3_000_000_000,
        DriverBackend::Tcg => 40_000_000_000,
    }
}

/// Host-time hang guard for one test case, not its failure condition: it
/// catches only an execution that stops making simulated progress, which
/// [`simulated_bound`] cannot see. Unloaded, three TCG cases take about 185 s
/// and three PV cases 291 s (serial); the checkpoint case's own phases take
/// about 150 s. 20 minutes per case leaves room for a heavily shared host.
pub const HANG_GUARD: Duration = Duration::from_secs(1200);

/// Fail, after stopping the execution, once it has run past its simulated
/// bound without reaching `goal`.
async fn check_bound(
    client: &Client,
    id: &seedbox_api::SimulationId,
    backend: DriverBackend,
    simulated_ns: u64,
    goal: &str,
) {
    let bound = simulated_bound(backend);
    if simulated_ns > bound {
        let _ = client.stop(id).await;
        panic!(
            "{} execution {id} reached simulated {simulated_ns} ns, past its {bound} ns bound, \
             before {goal}",
            label(backend)
        );
    }
}

// A failed assertion requests service stop but never deletes inputs beneath
// an execution whose ownership barriers have not yet been established.
struct TestDirectory(Option<tempfile::TempDir>);
impl TestDirectory {
    fn path(&self) -> &Path {
        self.0.as_ref().unwrap().path()
    }
    fn close(mut self) {
        self.0.take().unwrap().close().unwrap();
    }
}
impl Drop for TestDirectory {
    fn drop(&mut self) {
        if let Some(directory) = self.0.take() {
            eprintln!(
                "native service failure retained inputs at {}",
                directory.keep().display()
            );
        }
    }
}
struct CancelServer(CancellationToken, Arc<Manager>);
impl Drop for CancelServer {
    fn drop(&mut self) {
        // Runtime teardown may abort serve before it polls global shutdown.
        // Cancel blocking work now; normal receipt/reap owns native cleanup.
        let result = self.1.cancel_active();
        self.0.cancel();
        if let Err(error) = result {
            eprintln!("native fixture cancellation failed; inputs retained: {error}");
        }
    }
}

struct NativeProvider {
    backend: DriverBackend,
    session: PathBuf,
    runtimes: Arc<Mutex<Vec<PathBuf>>>,
    workers: Arc<AtomicUsize>,
    observed: Arc<AtomicBool>,
}
/// Test-only executor that launches its inner Driver worker without the
/// server-owned observation socket, as an unobserved execution would.
struct Unobserved(Box<dyn ExecutionBackend>);
impl ExecutionBackend for Unobserved {
    fn preflight_workers(&self, workers: u32) -> seedbox_application::Result<()> {
        self.0.preflight_workers(workers)
    }
    fn preflight(&self, output: Option<&Path>) -> seedbox_application::Result<()> {
        self.0.preflight(output)
    }
    fn validate_retained(&self, inputs: &PreparedInputs) -> seedbox_application::Result<()> {
        self.0.validate_retained(inputs)
    }
    fn runtime_inputs<'a>(&self, inputs: &'a PreparedInputs) -> Vec<&'a ContentDigest> {
        self.0.runtime_inputs(inputs)
    }
    fn profile(&self) -> ExecutionProfile {
        self.0.profile()
    }
    fn performance_parameters(&self) -> BTreeMap<String, String> {
        self.0.performance_parameters()
    }
    fn execute(
        &self,
        mut request: seedbox_application::execution::ExecutionRequest<'_>,
    ) -> seedbox_application::Result<seedbox_application::execution::ExecutionReport> {
        assert!(
            request.observations.is_some(),
            "service supplies observations"
        );
        request.observations = None;
        self.0.execute(request)
    }
}
impl BackendProvider for NativeProvider {
    fn project(
        &self,
        _: &seedbox_application::config::Simulation,
        _: &ContentStore,
        _: &CancellationToken,
    ) -> seedbox_application::Result<Backends> {
        panic!("resolved native submissions must not prepare OCI images")
    }
    fn retained(
        &self,
        inputs: &PreparedInputs,
        store: &ContentStore,
    ) -> seedbox_application::Result<Box<dyn ExecutionBackend>> {
        let digest = driver_world::runtime_digest(inputs, store)?;
        let runtime = Runtime::from_store(store, &digest, &self.session)?;
        let selected = runtime.select(
            runtime_backend(self.backend),
            seedbox_spec::definition::Architecture::X86_64,
        )?;
        self.runtimes
            .lock()
            .unwrap()
            .push(selected.archive().parent().unwrap().to_owned());
        let executor: Box<dyn ExecutionBackend> = Box::new(
            DriverExecutor::new(
                inputs.profile.clone(),
                self.backend,
                self.workers.load(Ordering::SeqCst),
            )?
            .with_runtime(selected)?
            .with_launch_context(vec![], Some(self.session.join(SESSION_LEASE_FILE))),
        );
        Ok(if self.observed.load(Ordering::SeqCst) {
            executor
        } else {
            Box::new(Unobserved(executor))
        })
    }
}

fn release_executable(checkout: &Path) -> PathBuf {
    let target = std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| checkout.join("target"));
    let target = if target.is_absolute() {
        target
    } else {
        checkout.join(target)
    };
    target.join("release/seedbox")
}

fn artifact(path: &Path) -> wire::Artifact {
    let bytes = fs::read(path).unwrap();
    wire::Artifact {
        sha256: ContentDigest::of(&bytes).hex().into(),
        length: bytes.len() as u64,
        artifact: path.to_str().unwrap().into(),
    }
}
fn directories(root: &Path) -> Vec<PathBuf> {
    let mut found = vec![];
    let mut pending = vec![root.to_owned()];
    while let Some(at) = pending.pop() {
        let entries = match fs::read_dir(at) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => panic!("inspect owned directories: {error}"),
        };
        for entry in entries {
            let entry = entry.unwrap();
            if entry.file_type().unwrap().is_dir() {
                let path = entry.path();
                pending.push(path.clone());
                found.push(path);
            }
        }
    }
    found
}
/// A one-file root filesystem holding `binary` as `/application`.
fn application_tar(path: &Path, binary: &[u8]) -> PathBuf {
    let mut tar = tar::Builder::new(Vec::new());
    let mut header = tar::Header::new_gnu();
    header.set_mode(0o755);
    header.set_uid(0);
    header.set_gid(0);
    header.set_mtime(0);
    header.set_size(binary.len() as u64);
    header.set_cksum();
    tar.append_data(&mut header, "application", binary).unwrap();
    fs::write(path, tar.into_inner().unwrap()).unwrap();
    path.to_owned()
}
fn submission(
    backend: DriverBackend,
    root: &Path,
    store: &ContentStore,
    iterations: u64,
    seed: u64,
) -> SubmitRequest {
    let checkout = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap();
    let executable = release_executable(&checkout);
    let archive = checkout.join("build/assets/driver-runtime.tar.gz");
    assert!(
        executable.is_file() && archive.is_file(),
        "run cargo xtask build-assets and cargo build --release first"
    );
    submission_with_machines(backend, root, store, iterations, seed, vec![])
}
fn submission_with_machines(
    backend: DriverBackend,
    root: &Path,
    store: &ContentStore,
    iterations: u64,
    seed: u64,
    extra_machines: Vec<&'static str>,
) -> SubmitRequest {
    // Both backends run the exact same static x86 cpu-loop ELF. The input is
    // backend-neutral: it selects no backend and building it needs no KVM.
    let binary = fs::read(
        std::env::var_os("SEEDBOX_CPU_LOOP_FIXTURE")
            .expect("SEEDBOX_CPU_LOOP_FIXTURE: machinePvTestFixtures/bin/cpu-loop"),
    )
    .unwrap();
    let filesystem = application_tar(&root.join("application.tar"), &binary);
    let launch = GuestLaunch {
        argv: vec!["/application".into(), iterations.to_string()],
        environment: BTreeMap::new(),
        working_directory: "/".into(),
        user: "0:0".into(),
    };
    fixture_submission(
        backend,
        root,
        store,
        seed,
        NativeFixture {
            name: "oracle",
            filesystem,
            launch,
            control: false,
            request_id: format!("cpu-{iterations}"),
            extra_machines,
        },
    )
}
struct NativeFixture {
    name: &'static str,
    filesystem: PathBuf,
    launch: GuestLaunch,
    control: bool,
    request_id: String,
    extra_machines: Vec<&'static str>,
}
fn fixture_submission(
    backend: DriverBackend,
    root: &Path,
    store: &ContentStore,
    seed: u64,
    fixture: NativeFixture,
) -> SubmitRequest {
    let checkout = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap();
    let executable = release_executable(&checkout);
    let archive = checkout.join("build/assets/driver-runtime.tar.gz");
    assert!(
        executable.is_file() && archive.is_file(),
        "build normal release assets first"
    );
    let oracle = fixture.name;
    let filesystem = fixture.filesystem;
    let launch = fixture.launch;
    let mut definition = MachineDefinition {
        name: oracle.into(),
        resources: Resources { memory_bytes: 0 },
        application: PreparedApplication {
            filesystem: FilesystemArchive::new(fs::read(&filesystem).unwrap()).unwrap(),
            launch: launch.clone(),
        },
        disk: None,
        network: None,
    };
    let runtime = Runtime::from_archive(&archive, root).unwrap();
    let selected = runtime
        .select(
            runtime_backend(backend),
            seedbox_spec::definition::Architecture::X86_64,
        )
        .unwrap();
    eprintln!(
        "native backend={}; fixture={:?}; executable={}; runtime_archive={}; selected_guest_assets={}; retained_qemu={:?}",
        label(backend),
        std::env::var_os("SEEDBOX_CPU_LOOP_FIXTURE"),
        executable.display(),
        archive.display(),
        selected.guest_assets.display(),
        selected.qemu,
    );
    let environment = GuestEnvironment::new(oracle, "127.0.0.1 localhost\n", fixture.control);
    let boot = match backend {
        DriverBackend::Pv => seedbox_machine_pv::machine::pv::Factory::new(
            &executable,
            &selected.guest_assets,
            946_684_800,
        )
        .unwrap()
        .with_blocks_per_progress_call(NonZeroU32::new(32).unwrap())
        .boot_archive_configured(&definition, &environment)
        .unwrap(),
        DriverBackend::Tcg => seedbox_machine_tcg::machine::tcg::Factory::new(
            selected
                .qemu
                .as_ref()
                .expect("TCG runtime must include its QEMU"),
            &selected.guest_assets,
            946_684_800,
        )
        .unwrap()
        .boot_archive_configured(&definition, &environment)
        .unwrap(),
    };
    let floor = u64::try_from(boot.len())
        .unwrap()
        .checked_mul(3)
        .unwrap()
        .checked_add(128 << 20)
        .unwrap();
    let floor = if fixture.extra_machines.is_empty() {
        floor
    } else {
        floor.max(192 << 20)
    };
    let memory_mb = u32::try_from(floor.div_ceil(1 << 20)).unwrap();
    definition.resources.memory_bytes = u64::from(memory_mb) << 20;
    eprintln!(
        "native service cpu-loop: {} boot bytes, {} MiB RAM",
        boot.len(),
        memory_mb
    );
    let assets = &selected.guest_assets;
    let filesystem_digest = store.put_file(&filesystem).unwrap();
    let mut semantic = SemanticParameters::new(memory_mb);
    semantic.engine = Some(DriverExecutor::descriptor(backend, "x86_64"));
    let mut inputs = PreparedInputs {
        wall_clock_epoch: Some(946_684_800),
        kernel: store.put_file(&assets.join("kernel")).unwrap(),
        images: BTreeMap::from([(
            oracle.into(),
            PreparedImage {
                manifest: filesystem_digest.clone(),
                config: filesystem_digest.clone(),
                layers: vec![],
                launch: Launch {
                    argv: launch.argv.clone(),
                    environment: launch.environment.clone(),
                    working_dir: launch.working_directory.clone(),
                    user: launch.user.clone(),
                    volumes: vec![],
                    disk_volumes: vec![],
                },
            },
        )]),
        guests: BTreeMap::from([(oracle.into(), filesystem_digest)]),
        placement: BTreeMap::from([(oracle.into(), oracle.into())]),
        profile: ExecutionProfile {
            backend: Backend::Other(format!("driver-{}", label(backend))),
            engine: store.put_file(&executable).unwrap(),
            witness_mode: WitnessMode::Exits,
            semantic_parameters: semantic,
        },
        auxiliary: BTreeMap::new(),
        provenance: BTreeMap::new(),
    };
    let environments = BTreeMap::from([(
        oracle.into(),
        wire::Environment {
            hostname: environment.hostname.clone(),
            hosts: environment.hosts.clone(),
            anonymous_volumes: vec![],
            named_volumes: vec![],
            configs: BTreeMap::new(),
            limits: wire::GuestLimits {
                max_map_count: environment.limits.max_map_count,
                nofile: environment.limits.nofile,
            },
            control_socket: fixture.control,
        },
    )]);
    let inventory = match backend {
        DriverBackend::Pv => wire::Inventory::Pv {
            policy: None,
            kernel: artifact(&assets.join("kernel")),
            init: artifact(&assets.join("init")),
            command_line: artifact(&assets.join("command-line")),
            runtime: artifact(&archive),
            formatter: Some(artifact(&assets.join("mke2fs"))),
            environments,
            boot_archives: BTreeMap::new(),
            epoch_seconds: 946_684_800,
            executable: artifact(&executable),
            progress: artifact(&assets.join("progress.tar")),
            blocks_per_progress_call: NonZeroU32::new(32).unwrap(),
        },
        DriverBackend::Tcg => wire::Inventory::Tcg {
            policy: None,
            kernel: artifact(&assets.join("kernel")),
            init: artifact(&assets.join("init")),
            command_line: artifact(&assets.join("command-line")),
            runtime: artifact(&archive),
            formatter: Some(artifact(&assets.join("mke2fs"))),
            environments,
            boot_archives: BTreeMap::new(),
            epoch_seconds: 946_684_800,
            executable: artifact(selected.qemu.as_ref().expect("retained TCG QEMU")),
            firmware: artifact(&assets.join("firmware")),
        },
    };
    let mut prepared = wire::Prepared {
        inventory,
        oracle: wire::Machine {
            name: oracle.into(),
            memory_bytes: definition.resources.memory_bytes,
            filesystem: artifact(&filesystem),
            process: wire::Process {
                argv: launch.argv,
                environment: launch.environment,
                working_directory: launch.working_directory,
                user: launch.user,
            },
            disk: None,
            interface: None,
        },
        machines: vec![],
        spawnable: vec![],
        quantum_ns: 1_000_000,
        network: wire::Network::default(),
    };
    for &name in &fixture.extra_machines {
        let mut machine = prepared.oracle.clone();
        machine.name = name.into();
        prepared.machines.push(machine);
        let environments = match &mut prepared.inventory {
            wire::Inventory::Pv { environments, .. }
            | wire::Inventory::Tcg { environments, .. } => environments,
        };
        let mut environment = environments[oracle].clone();
        environment.hostname = name.into();
        environment.control_socket = false;
        environments.insert(name.into(), environment);
        inputs
            .images
            .insert(name.into(), inputs.images[oracle].clone());
        inputs
            .guests
            .insert(name.into(), inputs.guests[oracle].clone());
        inputs.placement.insert(name.into(), name.into());
    }
    driver_world::retain_prepared(&mut inputs, &prepared, store).unwrap();
    let image = if fixture.control {
        "fixture/prepared-fixture"
    } else {
        "fixture/cpu-loop"
    };
    let services: String = std::iter::once(oracle)
        .chain(fixture.extra_machines.iter().copied())
        .map(|name| format!("  {name}:\n    image: {image}\n"))
        .collect();
    let mut configuration = seedbox_application::config::parse(
        &format!(
            "name: native-{}-service\nservices:\n{services}",
            label(backend)
        ),
        Path::new("/fixture/compose.yaml"),
        &Default::default(),
    )
    .unwrap();
    configuration.check.service = oracle.into();
    configuration.machine.memory_mb = memory_mb;
    configuration.control = fixture.control.then(|| seedbox_spec::Control {
        service: oracle.into(),
    });
    let definition =
        seedbox_application::definition::resolve(&configuration, &inputs, seed, vec![]).unwrap();
    assert_eq!(definition.simulator.oracle_control, fixture.control);
    let mut request = SubmitRequest {
        metadata: Default::default(),
        options: Default::default(),
        request_id: fixture.request_id,
        spec: SimulationSpec::resolved(definition),
    };
    request.metadata.name = format!("native-{}-service", label(backend));
    request
}

// A fresh public-service session per scenario; the Driver manifest is hand-built
// for this native slice, not proof of the production authoring/provider path.
struct NativeSession {
    backend: DriverBackend,
    cancel: CancelServer,
    server: Option<tokio::task::JoinHandle<Result<(), seedbox_server::execution::Error>>>,
    root: TestDirectory,
    session: TestDirectory,
    store: ContentStore,
    execution_root: PathBuf,
    runtime_paths: Arc<Mutex<Vec<PathBuf>>>,
    workers: Arc<AtomicUsize>,
    observed: Arc<AtomicBool>,
    client: Client,
    submitted: usize,
}
impl NativeSession {
    fn new(backend: DriverBackend) -> Self {
        let root = TestDirectory(Some(tempfile::tempdir().unwrap()));
        let store = ContentStore::new(root.path().join("store"));
        let session = TestDirectory(Some(
            tempfile::Builder::new()
                .prefix("session-")
                .tempdir_in(root.path())
                .unwrap(),
        ));
        fs::write(session.path().join(SESSION_LEASE_FILE), []).unwrap();
        let runtime_paths = Arc::new(Mutex::new(Vec::new()));
        let workers = Arc::new(AtomicUsize::new(1));
        let observed = Arc::new(AtomicBool::new(true));
        let manager = Arc::new(Manager::new(Catalogue::in_memory()));
        let execution_root = session.path().join("execution");
        let service = Arc::new(
            ExecutionService::new(
                manager.clone(),
                execution_root.clone(),
                store.clone(),
                Arc::new(NativeProvider {
                    backend,
                    session: session.path().to_owned(),
                    runtimes: runtime_paths.clone(),
                    workers: workers.clone(),
                    observed: observed.clone(),
                }),
            )
            .unwrap(),
        );
        let cancellation = CancellationToken::default();
        let socket = root.path().join("api.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let server = tokio::spawn(seedbox_server::serve(
            listener,
            manager.clone(),
            service,
            NonZeroUsize::new(1).unwrap(),
            cancellation.clone(),
        ));
        Self {
            backend,
            cancel: CancelServer(cancellation, manager),
            server: Some(server),
            root,
            session,
            store,
            execution_root,
            runtime_paths,
            workers,
            observed,
            client: Client::connect_to(&socket).unwrap(),
            submitted: 0,
        }
    }
    async fn submit(
        &mut self,
        mut request: SubmitRequest,
        workers: usize,
    ) -> seedbox_api::SimulationId {
        self.workers.store(workers, Ordering::SeqCst);
        request.request_id = format!("native-run-{}", self.submitted);
        request.metadata.name = format!("native-pv-service-{}", self.submitted);
        let accepted = self.client.submit(&request).await.unwrap();
        self.submitted += 1;
        accepted.metadata.id
    }
    async fn worker(&self, id: &seedbox_api::SimulationId) -> PathBuf {
        loop {
            if let Some(path) = directories(self.session.path()).into_iter().find(|path| {
                path.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with(scratch_prefix(self.backend))
            }) {
                return path;
            }
            let state = self.client.describe(id).await.unwrap();
            assert_ne!(
                state.status.phase,
                Phase::Completed,
                "native worker never started: {:?}",
                state.status
            );
            check_bound(
                &self.client,
                id,
                self.backend,
                state.status.simulated_time_ns,
                "its worker started",
            )
            .await;
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
    async fn guest_start(&self, id: &seedbox_api::SimulationId) {
        let mut logs = self.client.logs_with(id, "oracle", true).await.unwrap();
        let mut progress = self.client.watch(id).await.unwrap();
        let mut bytes = vec![];
        // Wait for the output while watching simulated progress, so a guest
        // that never prints fails at its bound instead of waiting forever.
        loop {
            tokio::select! {
                delivery = logs.next() => {
                    let Some(delivery) = delivery else { break };
                    if let seedbox_api::LogDelivery::Entry {
                        entry: seedbox_api::LogEntry::Bytes { bytes: data, .. },
                        ..
                    } = delivery.unwrap()
                    {
                        bytes.extend(data);
                    }
                    if bytes.windows(6).any(|part| part == b"START=") {
                        return;
                    }
                }
                state = progress.next() => {
                    let state = state.expect("watch closed before START=").unwrap();
                    check_bound(
                        &self.client,
                        id,
                        self.backend,
                        state.status.simulated_time_ns,
                        "its application printed START=",
                    )
                    .await;
                }
            }
        }
        panic!("operation must occur after real guest application output");
    }
    async fn output(&self, id: &seedbox_api::SimulationId) -> Vec<u8> {
        let mut logs = self.client.logs_with(id, "oracle", false).await.unwrap();
        let mut bytes = Vec::new();
        while let Some(delivery) = logs.next().await {
            if let seedbox_api::LogDelivery::Entry {
                entry: seedbox_api::LogEntry::Bytes { bytes: data, .. },
                ..
            } = delivery.unwrap()
            {
                bytes.extend(data);
            }
        }
        bytes
    }
    /// The retained application stdout of `machine`, which must be complete:
    /// any omission or non-stdout record fails rather than being skipped. A
    /// stopped worker never sends its clean close, so a `stopped` history ends
    /// in exactly one terminal source-unavailable marker.
    async fn stdout(&self, id: &seedbox_api::SimulationId, machine: &str, stopped: bool) -> String {
        self.stdout_timed(id, machine, stopped).await.0
    }
    /// As `stdout`, with the end offset and simulated time of each record.
    async fn stdout_timed(
        &self,
        id: &seedbox_api::SimulationId,
        machine: &str,
        stopped: bool,
    ) -> (String, Vec<(usize, u64)>) {
        let mut logs = self.client.logs_with(id, machine, false).await.unwrap();
        let mut bytes = Vec::new();
        let mut boot = None;
        let mut ended = false;
        let mut times = Vec::new();
        while let Some(delivery) = logs.next().await {
            let delivery = delivery.unwrap();
            assert!(
                !ended || matches!(delivery, seedbox_api::LogDelivery::Closed),
                "{machine} log continues after its source ended: {delivery:?}"
            );
            match delivery {
                seedbox_api::LogDelivery::Entry {
                    entry:
                        seedbox_api::LogEntry::Bytes {
                            machine: from,
                            incarnation,
                            stream,
                            bytes: data,
                            simulated_time_ns,
                        },
                    ..
                } => {
                    assert_eq!(from, machine);
                    assert_eq!(
                        *boot.get_or_insert(incarnation),
                        incarnation,
                        "{machine} rebooted"
                    );
                    assert_eq!(
                        stream,
                        seedbox_api::LogStream::Stdout,
                        "{machine}: {data:?}"
                    );
                    bytes.extend(data);
                    times.push((bytes.len(), simulated_time_ns));
                }
                seedbox_api::LogDelivery::Entry {
                    entry: seedbox_api::LogEntry::SourceUnavailable,
                    ..
                } if stopped => ended = true,
                seedbox_api::LogDelivery::Closed => {}
                other => panic!("{machine} log history is incomplete: {other:?}"),
            }
        }
        assert_eq!(ended, stopped, "{machine} source end");
        (String::from_utf8(bytes).unwrap(), times)
    }
    fn launch_request(&self, id: &seedbox_api::SimulationId) -> wire::Request {
        let requests = directories(&self.execution_root.join("runs").join(id.as_str()))
            .into_iter()
            .map(|path| path.join("worker-request.json"))
            .filter(|path| path.is_file())
            .collect::<Vec<_>>();
        assert_eq!(requests.len(), 1);
        let request: wire::Request =
            serde_json::from_reader(fs::File::open(&requests[0]).unwrap()).unwrap();
        request
    }
    async fn complete(
        &self,
        id: &seedbox_api::SimulationId,
        stopped: bool,
        workers: usize,
        seed: u64,
    ) -> Option<String> {
        let mut watch = self.client.watch(id).await.unwrap();
        let completed = loop {
            let state = watch
                .next()
                .await
                .expect("watch closed before completion")
                .unwrap();
            if state.status.phase == Phase::Completed {
                // A completion observed without any earlier over-bound state
                // still fails if it finished past the bound.
                check_bound(
                    &self.client,
                    id,
                    self.backend,
                    state.status.simulated_time_ns,
                    "it completed",
                )
                .await;
                break state;
            }
            check_bound(
                &self.client,
                id,
                self.backend,
                state.status.simulated_time_ns,
                "it completed",
            )
            .await;
        };
        let witness = if stopped {
            assert_eq!(
                completed.status.termination,
                Some(ExecutionTermination::Stopped)
            );
            None
        } else {
            assert_eq!(
                completed.status.verdict(),
                TestVerdict::Passed,
                "{:?}",
                completed.status
            );
            assert_eq!(
                completed.status.termination,
                Some(ExecutionTermination::OracleExited)
            );
            let reports = directories(&self.execution_root.join("runs").join(id.as_str()))
                .into_iter()
                .filter_map(|path| seedbox_application::orchestration::inspect(&path).ok())
                .collect::<Vec<_>>();
            assert_eq!(reports.len(), 1);
            let witness = reports[0]
                .report
                .as_ref()
                .unwrap()
                .witness
                .as_ref()
                .expect("native getter must supply final witness")
                .hex();
            assert_eq!(witness.len(), 64);
            assert!(!witness.is_empty());
            eprintln!("native witness: id={id} workers={workers} witness={witness}");
            Some(witness.to_owned())
        };
        // Verify the actual launched Driver request, not just the test setting.
        let request = self.launch_request(id);
        assert_eq!(request.machine_workers, workers);
        assert_eq!(request.seed, seed);
        assert_eq!(self.runtime_paths.lock().unwrap().len(), self.submitted);
        let _barriers = self.assert_cleanup();
        witness
    }
    fn assert_cleanup(&self) -> Vec<fs::File> {
        let session = self.session.path();
        let mut barriers =
            vec![reclaim_barrier(&session.join(SESSION_LEASE_FILE), Duration::ZERO).unwrap()];
        let leases = directories(session)
            .into_iter()
            .map(|path| path.join(EXECUTION_LEASE_FILE))
            .filter(|path| path.exists())
            .collect::<Vec<_>>();
        barriers.extend(
            leases
                .iter()
                .map(|path| reclaim_barrier(path, Duration::ZERO).unwrap()),
        );
        let remaining = directories(session)
            .into_iter()
            .filter(|path| {
                let name = path.file_name().unwrap().to_string_lossy();
                name.starts_with("native-scratch")
                    || name.starts_with("seedbox-pv-")
                    || name.starts_with("seedbox-tcg-")
                    || name.starts_with("driver-runtime-")
            })
            .collect::<Vec<_>>();
        assert!(
            remaining.is_empty(),
            "native directories survive before close: {remaining:?}"
        );
        assert!(self
            .runtime_paths
            .lock()
            .unwrap()
            .iter()
            .all(|path| !path.exists()));
        eprintln!("pre-close native scratch/runtime absence verified with session and execution lease barriers");
        barriers
    }
    async fn close(mut self) {
        self.cancel.0.cancel();
        self.server.take().unwrap().await.unwrap().unwrap();
        let _barriers = self.assert_cleanup();
        let session_path = self.session.path().to_owned();
        self.session.close();
        assert!(!session_path.exists());
        self.root.close();
    }
    async fn control(
        &self,
        id: &seedbox_api::SimulationId,
        name: &str,
        command: seedbox_api::ControlCommand,
    ) -> seedbox_api::ControlReceipt {
        use seedbox_api::{ControlRequest, ControlState};
        let diagnostic = matches!(name, "verify-paused-restore" | "continue");
        let began = Instant::now();
        if diagnostic {
            eprintln!("restore control phase: id={id} name={name} elapsed_ms=0 request_send command={command:?}");
        }
        self.client
            .request_control(
                id,
                &ControlRequest {
                    request_id: name.into(),
                    command,
                },
            )
            .await
            .unwrap();
        if diagnostic {
            eprintln!(
                "restore control phase: id={id} name={name} elapsed_ms={} request_accepted",
                began.elapsed().as_millis()
            );
        }
        let mut last_diagnostic = Instant::now();
        loop {
            let receipt = self.client.control_receipt(id, name).await.unwrap();
            if diagnostic
                && (receipt.state != ControlState::Accepted
                    || last_diagnostic.elapsed() >= Duration::from_secs(2))
            {
                eprintln!(
                    "restore control phase: id={id} name={name} elapsed_ms={} receipt={receipt:?}",
                    began.elapsed().as_millis()
                );
                last_diagnostic = Instant::now();
            }
            if receipt.state != ControlState::Accepted {
                assert!(
                    matches!(
                        receipt.state,
                        ControlState::Acknowledged { error: None, .. }
                    ),
                    "{receipt:?}"
                );
                return receipt;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }
}

#[derive(Clone, Copy)]
struct NativeRun {
    iterations: u64,
    seed: u64,
    workers: usize,
}
impl NativeRun {
    fn cpu(iterations: u64) -> Self {
        Self {
            iterations,
            seed: 123,
            workers: 1,
        }
    }
}
async fn native_runs(
    backend: DriverBackend,
    runs: &[NativeRun],
    cancelled: bool,
    expected_match: Option<bool>,
) {
    let mut session = NativeSession::new(backend);
    let mut ids = Vec::new();
    let mut witnesses = Vec::new();
    let mut original: Option<SubmitRequest> = None;
    for (index, run) in runs.iter().enumerate() {
        let request = if index == 1 && expected_match == Some(true) {
            original.as_ref().unwrap().clone()
        } else {
            submission(
                backend,
                session.root.path(),
                &session.store,
                run.iterations,
                run.seed,
            )
        };
        if index == 0 {
            original = Some(request.clone());
        }
        let id = session.submit(request, run.workers).await;
        assert!(
            !ids.contains(&id),
            "fresh submission reused an old execution"
        );
        let worker = session.worker(&id).await;
        if cancelled {
            session.guest_start(&id).await;
            assert_ne!(
                session.client.describe(&id).await.unwrap().status.phase,
                Phase::Completed
            );
            session.client.stop(&id).await.unwrap();
        }
        let witness = session
            .complete(&id, cancelled, run.workers, run.seed)
            .await;
        assert!(!worker.exists());
        if let Some(witness) = witness {
            eprintln!(
                "native run: id={id} seed={} workers={} iterations={} witness={witness}",
                run.seed, run.workers, run.iterations
            );
            if index == 1 {
                let comparison = session.client.compare(&ids[0], &id).await.unwrap();
                let raw_equal = witnesses[0] == witness;
                if let Some(expected) = expected_match {
                    assert_eq!(raw_equal, expected, "raw native witness comparison");
                    assert_eq!(comparison.witness_matches, Some(expected));
                    if expected {
                        assert_eq!(comparison.status, ComparisonStatus::Match);
                    } else {
                        assert_ne!(comparison.status, ComparisonStatus::Match);
                    }
                }
                eprintln!(
                    "native comparison: raw_equal={raw_equal} status={:?} witness_matches={:?}",
                    comparison.status, comparison.witness_matches
                );
            }
            witnesses.push(witness);
        }
        ids.push(id);
    }
    session.close().await;
}
pub async fn native_session(
    backend: DriverBackend,
    iterations: &[u64],
    cancelled: bool,
    expected_match: Option<bool>,
) {
    let runs = iterations
        .iter()
        .map(|n| NativeRun::cpu(*n))
        .collect::<Vec<_>>();
    native_runs(backend, &runs, cancelled, expected_match).await;
}

pub async fn pv_worker_counts() {
    // test-time: allow(hang-guard) -- Outer HANG_GUARD retains the documented 1200s loaded-host safety budget; check_bound independently limits simulated progress and exact evidence decides success.
    tokio::time::timeout(
        HANG_GUARD,
        native_runs(
            DriverBackend::Pv,
            &[
                NativeRun::cpu(200_000),
                NativeRun {
                    workers: 4,
                    ..NativeRun::cpu(200_000)
                },
            ],
            false,
            Some(true),
        ),
    )
    .await
    .expect("worker-count pair hit the host hang guard without finishing");
}
pub async fn pv_seed_report() {
    // CPU-only input need not consume seed-dependent randomness. Report raw
    // equality/inequality honestly; the changed-input W1 test requires inequality.
    // test-time: allow(hang-guard) -- Outer HANG_GUARD retains the documented 1200s loaded-host safety budget; check_bound independently limits simulated progress and exact evidence decides success.
    tokio::time::timeout(
        HANG_GUARD,
        native_runs(
            DriverBackend::Pv,
            &[
                NativeRun::cpu(200_000),
                NativeRun {
                    seed: 124,
                    ..NativeRun::cpu(200_000)
                },
            ],
            false,
            None,
        ),
    )
    .await
    .expect("seed-report pair hit the host hang guard without finishing");
}
pub async fn pv_checkpoint_continuation() {
    use seedbox_api::{CheckpointState, ControlCommand, ControlState, InputSource, PlaybackRate};
    let began = Instant::now();
    let phase = |name: &str| {
        eprintln!(
            "checkpoint phase: elapsed_ms={} unix_ms={} {name}",
            began.elapsed().as_millis(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_millis()
        )
    };
    phase("case_begin");
    // test-time: allow(hang-guard) -- Outer HANG_GUARD retains the documented 1200s loaded-host safety budget; check_bound independently limits simulated progress and exact evidence decides success.
    tokio::time::timeout(HANG_GUARD, async {
        let mut session = NativeSession::new(DriverBackend::Pv);
        // More than one virtual window of CPU work, still a finite stock fixture.
        let request = submission(DriverBackend::Pv, session.root.path(), &session.store, 2_000_000, 123);
        phase("baseline_submit_begin");
        let baseline = session.submit(request.clone(), 1).await;
        phase("baseline_submitted");
        session.worker(&baseline).await;
        let uninterrupted = session.complete(&baseline, false, 1, 123).await.unwrap();
        phase("baseline_completed");
        let final_ns = session.client.describe(&baseline).await.unwrap().status.simulated_time_ns;
        let target_ns = final_ns.div_ceil(2);
        assert!(target_ns > 1_000_000 && target_ns < final_ns,
            "CPU fixture must span multiple committed windows");
        let mut capture_request = request;
        capture_request.options.start_paused = true;
        let source = session.submit(capture_request, 1).await;
        session.worker(&source).await;
        session.control(&source, "verify-paused-source", ControlCommand::SetRate { rate: PlaybackRate::Paused }).await;
        let mut step = 0u64;
        let source_output = loop {
            let before = session.client.playback(&source).await.unwrap().state.position.completed_rounds;
            session.control(&source, &format!("midloop-step-{step}"), ControlCommand::Step).await;
            step += 1;
            let status = loop {
                let status = session.client.playback(&source).await.unwrap().state;
                assert!(!status.terminal, "fixture ended before mid-workload capture");
                check_bound(
                    &session.client,
                    &source,
                    DriverBackend::Pv,
                    status.position.simulated_time_ns,
                    "its mid-workload capture point",
                )
                .await;
                if !status.stepping && status.position.completed_rounds > before { break status; }
                tokio::time::sleep(Duration::from_millis(5)).await;
            };
            let bytes = session.output(&source).await;
            let started = bytes.windows(6).any(|part| part == b"START=");
            assert!(!bytes.windows(4).any(|part| part == b"END="), "source finished before capture");
            if started && status.position.simulated_time_ns >= target_ns {
                assert!(status.application_ready);
                assert!(status.position.simulated_time_ns < final_ns);
                break bytes;
            }
        };
        assert_eq!(source_output.windows(6).filter(|part| *part == b"START=").count(), 1);
        let captured = session.control(&source, "saved", ControlCommand::Checkpoint).await;
        assert!(matches!(&captured.state, ControlState::Acknowledged { checkpoint: Some(c), error: None, .. }
            if c.state == CheckpointState::Captured && c.position.completed_rounds > 0));
        eprintln!("public checkpoint receipt: {captured:?}");
        let (captured_rounds, captured_ns) = match &captured.state {
            ControlState::Acknowledged { checkpoint: Some(checkpoint), .. } =>
                (checkpoint.position.completed_rounds, checkpoint.position.simulated_time_ns),
            _ => unreachable!("captured receipt asserted above"),
        };
        assert!(captured_rounds > 0 && captured_ns > 0);
        assert!(captured_ns >= target_ns && captured_ns < final_ns);
        eprintln!("C2 mid-workload capture: final_ns={final_ns} target_ns={target_ns} captured_ns={captured_ns} captured_rounds={captured_rounds} source_START_count=1 source_END_count=0");
        phase("export_begin");
        let exported = session.client.checkpoint_definition(&source, "saved").await.unwrap();
        phase("export_done");
        let InputSource::Resolved { definition } = &exported.spec.inputs else { panic!("resolved export required") };
        let seedbox_spec::definition::Start::Checkpoint { assets, .. } = &definition.start else { panic!("checkpoint export required") };
        assert!(!assets.is_empty());
        phase("source_stop_begin");
        session.client.stop(&source).await.unwrap();
        session.complete(&source, true, 1, 123).await;
        phase("source_stopped_reaped");
        // Import must use the exported content closure, not the source paths.
        phase("source_delete_begin");
        session.client.delete_checkpoint(&source, "saved").await.unwrap();
        session.client.delete(&source).await.unwrap();
        assert!(!session.execution_root.join("runs").join(source.as_str()).exists());
        phase("source_deleted_import_begin");
        let import_began = Instant::now();
        session.workers.store(4, Ordering::SeqCst);
        let restored = session.client.submit_definition(&serde_json::to_value(exported).unwrap(), true).await.unwrap().metadata.id;
        session.submitted += 1;
        assert_ne!(restored, source);
        phase("import_submit_returned");
        session.worker(&restored).await;
        phase("import_scratch_seen_not_yet_paused_ack");
        // C1: equal final evidence alone cannot distinguish restore from a
        // silent fresh start. Check the real launch and a fresh paused status.
        let launch = session.launch_request(&restored);
        let resume = launch.resume_checkpoint.as_ref().expect("restore launch must name its checkpoint");
        assert!(resume.is_file());
        phase("paused_restore_control_begin");
        session.control(&restored, "verify-paused-restore", ControlCommand::SetRate { rate: PlaybackRate::Paused }).await;
        phase("paused_restore_control_ack");
        eprintln!("import to paused ack: elapsed_ms={}", import_began.elapsed().as_millis());
        phase("paused_restore_status_query_begin");
        let paused = session.client.playback(&restored).await.unwrap().state;
        phase("paused_restore_status_query_returned");
        eprintln!("restore position BEFORE continue: {paused:?}");
        assert_eq!(paused.rate, PlaybackRate::Paused);
        assert!(!paused.stepping && !paused.terminal);
        assert!(paused.position.completed_rounds > 0 && paused.position.simulated_time_ns > 0);
        assert_eq!(paused.position.completed_rounds, captured_rounds);
        assert_eq!(paused.position.simulated_time_ns, captured_ns);
        eprintln!("C1 restore verified BEFORE continue: resume={} captured_rounds={captured_rounds} restored_rounds={} captured_ns={captured_ns} restored_ns={}", resume.display(), paused.position.completed_rounds, paused.position.simulated_time_ns);
        phase("continue_begin");
        session.control(&restored, "continue", ControlCommand::SetRate { rate: PlaybackRate::Unlimited }).await;
        phase("continue_ack");
        phase("post_continue_status_query_begin");
        let after_continue = session.client.playback(&restored).await.unwrap().state;
        eprintln!("restore position AFTER continue: {after_continue:?}");
        phase("post_continue_status_query_returned");
        let continued = session.complete(&restored, false, 4, 123).await.unwrap();
        phase("restored_completed");
        eprintln!("restore final simulated_time_ns={}", session.client.describe(&restored).await.unwrap().status.simulated_time_ns);
        let continued_output = session.output(&restored).await;
        assert!(!continued_output.windows(6).any(|part| part == b"START="),
            "restored workload printed START again (fresh restart)");
        assert_eq!(continued_output.windows(4).filter(|part| *part == b"END=").count(), 1);
        eprintln!("C2 restored output: START_count=0 END_count=1; CPU-only loop has no timers or devices in flight across capture");
        assert_eq!(uninterrupted, continued, "raw uninterrupted/continued native witnesses differ");
        let comparison = session.client.compare(&baseline, &restored).await.unwrap();
        assert_eq!(comparison.witness_matches, Some(true));
        eprintln!("public native checkpoint continuation: raw_equal=true witness_matches={:?} baseline={baseline} restored={restored} witness={continued}", comparison.witness_matches);
        session.close().await;
    }).await.expect("checkpoint roundtrip hit the host hang guard without finishing");
}
// The in-flight checkpoint world: the oracle is the stock clock fixture's
// `idle` role (one 12.5 s nanosleep, then exit 0), so its pending timer decides
// termination. The shared `network-probe` server and client exchange UDP, then
// a TCP stream, early in that sleep. Service machines run in configured
// container environments, whose /dev holds no raw /dev/vda (disks back named
// volumes), so the raw-block `server-disk` mode cannot run here.
const PROBE_PORT: &str = "7000";
const PROBE_BYTES: &str = "262144";
const IDLE_SLEEP_NS: u64 = 12_500_000_000;
const PROBE_READY: &str = "NETPROBE ready port=7000\n";
const PROBE_TCP_DONE: &str = "NETPROBE tcp ok bytes=262144 connects=";
const PROBE_CLIENT_MID: &str = "NETPROBE arp ok ";
struct ProbeMachine {
    name: &'static str,
    binary: &'static str,
    argv: &'static [&'static str],
    interface: Option<(u8, [u8; 4])>,
}
const PROBE_MACHINES: [ProbeMachine; 3] = [
    ProbeMachine {
        name: "oracle",
        binary: "SEEDBOX_PV_CLOCK_APPLICATION",
        argv: &["idle"],
        interface: None,
    },
    ProbeMachine {
        name: "server",
        binary: "SEEDBOX_PV_NETWORK_APPLICATION",
        argv: &["server", PROBE_PORT],
        interface: Some((1, [10, 0, 0, 1])),
    },
    ProbeMachine {
        name: "client",
        binary: "SEEDBOX_PV_NETWORK_APPLICATION",
        argv: &["client", "10.0.0.1", PROBE_PORT, PROBE_BYTES],
        interface: Some((2, [10, 0, 0, 2])),
    },
];
fn network_submission(
    backend: DriverBackend,
    root: &Path,
    store: &ContentStore,
    seed: u64,
) -> SubmitRequest {
    use seedbox_machine::definition::{MacAddress, NetworkInterface};
    let checkout = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap();
    let executable = checkout.join("target/release/seedbox");
    let archive = checkout.join("build/assets/driver-runtime.tar.gz");
    assert!(
        executable.is_file() && archive.is_file(),
        "run cargo xtask build-assets and cargo build --release first"
    );
    let runtime = Runtime::from_archive(&archive, root).unwrap();
    let selected = runtime
        .select(
            runtime_backend(backend),
            seedbox_spec::definition::Architecture::X86_64,
        )
        .unwrap();
    let assets = &selected.guest_assets;
    // Both backends run the same static x86 fixtures; the variable names are
    // the fixture package's, not a backend selection.
    let machines = PROBE_MACHINES
        .iter()
        .map(|machine| {
            let path = std::env::var_os(machine.binary)
                .unwrap_or_else(|| panic!("{} fixture", machine.binary));
            let filesystem = application_tar(
                &root.join(format!("{}.tar", machine.name)),
                &fs::read(&path).unwrap(),
            );
            let launch = GuestLaunch {
                argv: std::iter::once("/application")
                    .chain(machine.argv.iter().copied())
                    .map(String::from)
                    .collect(),
                environment: BTreeMap::new(),
                working_directory: "/".into(),
                user: "0:0".into(),
            };
            let mac = machine.interface.map(|(index, _)| [2, 0, 0, 0, 0, index]);
            (machine, filesystem, launch, mac)
        })
        .collect::<Vec<_>>();
    // Size the one simulation-wide memory from the largest real boot archive.
    let mut largest = 0u64;
    for (machine, filesystem, launch, mac) in &machines {
        let definition = MachineDefinition {
            name: machine.name.into(),
            resources: Resources { memory_bytes: 0 },
            application: PreparedApplication {
                filesystem: FilesystemArchive::new(fs::read(filesystem).unwrap()).unwrap(),
                launch: launch.clone(),
            },
            disk: None,
            network: machine.interface.map(|(_, address)| {
                NetworkInterface::new(MacAddress::new(mac.unwrap()).unwrap(), address.into(), 24)
                    .unwrap()
            }),
        };
        let environment = GuestEnvironment::new(machine.name, "127.0.0.1 localhost\n", false);
        let boot = match backend {
            DriverBackend::Pv => {
                seedbox_machine_pv::machine::pv::Factory::new(&executable, assets, 946_684_800)
                    .unwrap()
                    .with_blocks_per_progress_call(NonZeroU32::new(32).unwrap())
                    .boot_archive_configured(&definition, &environment)
                    .unwrap()
            }
            DriverBackend::Tcg => seedbox_machine_tcg::machine::tcg::Factory::new(
                selected
                    .qemu
                    .as_ref()
                    .expect("TCG runtime must include its QEMU"),
                assets,
                946_684_800,
            )
            .unwrap()
            .boot_archive_configured(&definition, &environment)
            .unwrap(),
        };
        largest = largest.max(u64::try_from(boot.len()).unwrap());
    }
    let floor = largest
        .checked_mul(3)
        .unwrap()
        .checked_add(128 << 20)
        .unwrap();
    let memory_mb = u32::try_from(floor.div_ceil(1 << 20)).unwrap();
    let memory_bytes = u64::from(memory_mb) << 20;
    eprintln!(
        "native in-flight checkpoint world: backend={} largest_boot={largest} memory_mib={memory_mb} runtime_archive={} selected_guest_assets={}",
        label(backend),
        archive.display(),
        assets.display(),
    );
    let mut semantic = SemanticParameters::new(memory_mb);
    semantic.engine = Some(DriverExecutor::descriptor(backend, "x86_64"));
    let mut images = BTreeMap::new();
    let mut guests = BTreeMap::new();
    let mut environments = BTreeMap::new();
    let mut wire_machines = Vec::new();
    let limits = GuestEnvironment::new("oracle", "127.0.0.1 localhost\n", false).limits;
    for (machine, filesystem, launch, mac) in &machines {
        let digest = store.put_file(filesystem).unwrap();
        images.insert(
            machine.name.to_owned(),
            PreparedImage {
                manifest: digest.clone(),
                config: digest.clone(),
                layers: vec![],
                launch: Launch {
                    argv: launch.argv.clone(),
                    environment: launch.environment.clone(),
                    working_dir: launch.working_directory.clone(),
                    user: launch.user.clone(),
                    volumes: vec![],
                    disk_volumes: vec![],
                },
            },
        );
        guests.insert(machine.name.to_owned(), digest);
        environments.insert(
            machine.name.to_owned(),
            wire::Environment {
                hostname: machine.name.into(),
                hosts: "127.0.0.1 localhost\n".into(),
                anonymous_volumes: vec![],
                named_volumes: vec![],
                configs: BTreeMap::new(),
                limits: wire::GuestLimits {
                    max_map_count: limits.max_map_count,
                    nofile: limits.nofile,
                },
                control_socket: false,
            },
        );
        wire_machines.push(wire::Machine {
            name: machine.name.into(),
            memory_bytes,
            filesystem: artifact(filesystem),
            process: wire::Process {
                argv: launch.argv.clone(),
                environment: launch.environment.clone(),
                working_directory: launch.working_directory.clone(),
                user: launch.user.clone(),
            },
            disk: None,
            interface: machine.interface.map(|(_, address)| wire::Interface {
                mac: mac.unwrap(),
                address: address.into(),
                prefix_len: 24,
            }),
        });
    }
    let mut inputs = PreparedInputs {
        wall_clock_epoch: Some(946_684_800),
        kernel: store.put_file(&assets.join("kernel")).unwrap(),
        placement: guests
            .keys()
            .map(|name| (name.clone(), name.clone()))
            .collect(),
        images,
        guests,
        profile: ExecutionProfile {
            backend: Backend::Other(format!("driver-{}", label(backend))),
            engine: store.put_file(&executable).unwrap(),
            witness_mode: WitnessMode::Exits,
            semantic_parameters: semantic,
        },
        auxiliary: BTreeMap::new(),
        provenance: BTreeMap::new(),
    };
    let inventory = match backend {
        DriverBackend::Pv => wire::Inventory::Pv {
            policy: None,
            kernel: artifact(&assets.join("kernel")),
            init: artifact(&assets.join("init")),
            command_line: artifact(&assets.join("command-line")),
            runtime: artifact(&archive),
            formatter: Some(artifact(&assets.join("mke2fs"))),
            environments,
            boot_archives: BTreeMap::new(),
            epoch_seconds: 946_684_800,
            executable: artifact(&executable),
            progress: artifact(&assets.join("progress.tar")),
            blocks_per_progress_call: NonZeroU32::new(32).unwrap(),
        },
        DriverBackend::Tcg => wire::Inventory::Tcg {
            policy: None,
            kernel: artifact(&assets.join("kernel")),
            init: artifact(&assets.join("init")),
            command_line: artifact(&assets.join("command-line")),
            runtime: artifact(&archive),
            formatter: Some(artifact(&assets.join("mke2fs"))),
            environments,
            boot_archives: BTreeMap::new(),
            epoch_seconds: 946_684_800,
            executable: artifact(selected.qemu.as_ref().expect("retained TCG QEMU")),
            firmware: artifact(&assets.join("firmware")),
        },
    };
    let oracle = wire_machines.remove(0);
    let prepared = wire::Prepared {
        inventory,
        oracle,
        machines: wire_machines,
        spawnable: vec![],
        quantum_ns: 1_000_000,
        network: wire::Network::default(),
    };
    driver_world::retain_prepared(&mut inputs, &prepared, store).unwrap();
    let mut configuration = seedbox_application::config::parse(
        &format!(
            "name: native-{}-in-flight\nservices:\n  oracle:\n    image: fixture/clock-faults\n  server:\n    image: fixture/network-probe\n  client:\n    image: fixture/network-probe\n",
            label(backend)
        ),
        Path::new("/fixture/compose.yaml"),
        &Default::default(),
    )
    .unwrap();
    configuration.machine.memory_mb = memory_mb;
    configuration.control = None;
    let definition =
        seedbox_application::definition::resolve(&configuration, &inputs, seed, vec![]).unwrap();
    assert!(!definition.simulator.oracle_control);
    assert_eq!(definition.machines.len(), 2);
    let mut request = SubmitRequest {
        metadata: Default::default(),
        options: Default::default(),
        request_id: "in-flight".into(),
        spec: SimulationSpec::resolved(definition),
    };
    request.metadata.name = format!("native-{}-in-flight", label(backend));
    request
}
/// `request` with one input at `at_ns` that no application observes: a one
/// second realtime step of the oracle, whose relative nanosleep is measured on
/// the monotonic clock. It gives the public pre-fault pause a deterministic
/// boundary at that time; runs that compare carry it alike.
fn with_capture_marker(mut request: SubmitRequest, at_ns: u64) -> SubmitRequest {
    let seedbox_api::InputSource::Resolved { definition } = &request.spec.inputs else {
        panic!("resolved submission required")
    };
    let mut definition = definition.as_ref().clone();
    definition.overrides.push(seedbox_spec::definition::Override {
        id: "capture-marker".into(),
        at_ns,
        action: serde_json::json!({
            "action": "clock_skew", "handle": "capture-marker", "machine": "oracle", "offset_ns": 1_000_000_000u64
        }),
        timeout_ns: seedbox_spec::definition::default_timeout_ns(),
    });
    // The submitted program is derived from the definition's overrides.
    request.spec = SimulationSpec::resolved(definition);
    assert_eq!(request.spec.program.len(), 1);
    request
}
/// Every frame a simulation publishes after this subscription, until its
/// stream closes, and the number of batch sequence numbers reported missing.
async fn collect_frames(
    client: &Client,
    id: &seedbox_api::SimulationId,
) -> tokio::task::JoinHandle<(Vec<seedbox_api::Frame>, u64)> {
    use seedbox_api::FrameDelivery;
    let mut stream = client.frames(id).await.unwrap();
    tokio::spawn(async move {
        let (mut frames, mut missing) = (Vec::new(), 0u64);
        while let Some(delivery) = stream.next().await {
            match delivery.unwrap() {
                FrameDelivery::Frames { frames: batch, .. } => frames.extend(batch),
                FrameDelivery::Gap { first, last } => missing += last - first + 1,
                FrameDelivery::Closed => break,
            }
        }
        (frames, missing)
    })
}
/// Frames longer than ARP, the UDP token and bare TCP control segments
/// carry stream data.
const DATA_FRAME_BYTES: u32 = 128;
/// Scheduled stream-data frames from `from` to `to`.
fn data_frames<'a>(
    frames: &'a [seedbox_api::Frame],
    from: &'a str,
    to: &'a str,
) -> impl Iterator<Item = &'a seedbox_api::Frame> {
    frames.iter().filter(move |f| {
        f.from == from
            && f.to == to
            && f.fate == seedbox_api::FrameFate::Scheduled
            && f.bytes > DATA_FRAME_BYTES
    })
}
fn count(text: &str, marker: &str) -> usize {
    text.matches(marker).count()
}
/// The simulated time of the record that completed the first `marker`.
fn marker_time((text, times): &(String, Vec<(usize, u64)>), marker: &str) -> u64 {
    let end = text.find(marker).expect("marker in output") + marker.len();
    times.iter().find(|(offset, _)| *offset >= end).unwrap().1
}
/// Checkpoint, export and import a world captured while the oracle's sleep is
/// pending and the client's TCP stream is mid-transfer; the continuation must
/// reach the uninterrupted raw witness and output.
pub async fn checkpoint_in_flight_continuation(backend: DriverBackend) {
    use seedbox_api::{CheckpointState, ControlCommand, ControlState, InputSource, PlaybackRate};
    let began = Instant::now();
    let phase = |name: &str| {
        eprintln!(
            "in-flight checkpoint phase: backend={} elapsed_ms={} {name}",
            label(backend),
            began.elapsed().as_millis()
        )
    };
    // Hang guard only: every decision below is taken from guest events.
    tokio::time::timeout(HANG_GUARD, async {
        let seed = 123;
        let mut session = NativeSession::new(backend);
        let request = network_submission(backend, session.root.path(), &session.store, seed);
        // An unmarked run only locates the transfer in simulated time.
        let plain = session.submit(request.clone(), 1).await;
        session.worker(&plain).await;
        session.complete(&plain, false, 1, seed).await.unwrap();
        let plain_final_ns = session.client.describe(&plain).await.unwrap().status.simulated_time_ns;
        let client_timed = session.stdout_timed(&plain, "client", false).await;
        // The client prints ARP completion just before it connects, and TCP
        // completion after the whole stream echoed back. Same seed and input
        // make the source follow this trajectory, so capturing at the
        // midpoint lands inside the transfer, with data sent and unechoed.
        let tcp_begin_ns = marker_time(&client_timed, PROBE_CLIENT_MID);
        let tcp_end_ns = marker_time(&client_timed, PROBE_TCP_DONE);
        let target_ns = tcp_begin_ns + (tcp_end_ns - tcp_begin_ns) / 2;
        eprintln!("uninterrupted transfer: arp_ok_ns={tcp_begin_ns} tcp_ok_ns={tcp_end_ns} target_ns={target_ns}");
        assert!(tcp_begin_ns < target_ns && target_ns < tcp_end_ns && tcp_end_ns < IDLE_SLEEP_NS);
        let request = with_capture_marker(request, target_ns);
        let baseline = session.submit(request.clone(), 1).await;
        session.worker(&baseline).await;
        let uninterrupted = session.complete(&baseline, false, 1, seed).await.unwrap();
        let final_ns = session.client.describe(&baseline).await.unwrap().status.simulated_time_ns;
        let expected = [
            session.stdout(&baseline, "oracle", false).await,
            session.stdout(&baseline, "server", false).await,
            session.stdout(&baseline, "client", false).await,
        ];
        // No application observes the marker: output and end are unchanged.
        assert_eq!(expected[2], client_timed.0);
        assert_eq!(final_ns, plain_final_ns);
        phase("baseline_completed");
        eprintln!("uninterrupted: final_ns={final_ns} witness={uninterrupted} stdout={expected:?}");
        // The uninterrupted world exchanged the whole stream inside the
        // oracle's sleep, which alone ended the run.
        assert!(final_ns >= IDLE_SLEEP_NS);
        assert_eq!(expected[0], "");
        assert_eq!(expected[1], PROBE_READY);
        assert_eq!(count(&expected[2], PROBE_TCP_DONE), 1);
        assert!(expected[2].ends_with("NETPROBE done\n"));

        let mut capture_request = request;
        capture_request.options.start_paused = true;
        let source = session.submit(capture_request, 1).await;
        session.worker(&source).await;
        session
            .control(&source, "verify-paused-source", ControlCommand::SetRate { rate: PlaybackRate::Paused })
            .await;
        // Every frame the source routes, subscribed before it first runs.
        let source_frames = collect_frames(&session.client, &source).await;
        // Run to the boundary before the marker takes effect; it pauses there.
        session
            .control(&source, "run-to-marker", ControlCommand::PauseBeforeNextFault { rate: PlaybackRate::Unlimited })
            .await;
        loop {
            let status = session.client.playback(&source).await.unwrap().state;
            assert!(!status.terminal, "fixture ended before the in-flight capture");
            if status.rate == PlaybackRate::Paused && !status.stepping {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        // Capture at an ordinary paused boundary: one public step applies the
        // marker and commits one round, still inside the transfer. Capture at
        // the held pre-fault stop itself is covered by native_held_checkpoint.
        let before = session.client.playback(&source).await.unwrap().state.position.completed_rounds;
        session.control(&source, "apply-marker", ControlCommand::Step).await;
        loop {
            let status = session.client.playback(&source).await.unwrap().state;
            assert!(!status.terminal, "fixture ended before the in-flight capture");
            if !status.stepping && status.position.completed_rounds > before {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let captured = session.control(&source, "saved", ControlCommand::Checkpoint).await;
        let (captured_rounds, captured_ns) = match &captured.state {
            ControlState::Acknowledged { checkpoint: Some(checkpoint), error: None, .. }
                if checkpoint.state == CheckpointState::Captured =>
            {
                (checkpoint.position.completed_rounds, checkpoint.position.simulated_time_ns)
            }
            other => panic!("checkpoint not captured: {other:?}"),
        };
        assert!(captured_rounds > 0 && captured_ns > tcp_begin_ns && captured_ns < tcp_end_ns);
        let world = session.client.world(&source).await.unwrap();
        let oracle = world.machines.iter().find(|machine| machine.oracle).unwrap();
        assert_eq!(oracle.name, "oracle");
        assert_eq!(oracle.exit_code, None, "oracle sleep must be pending at capture");
        phase("captured");
        let exported = session.client.checkpoint_definition(&source, "saved").await.unwrap();
        let InputSource::Resolved { definition } = &exported.spec.inputs else {
            panic!("resolved export required")
        };
        let seedbox_spec::definition::Start::Checkpoint { assets, .. } = &definition.start else {
            panic!("checkpoint export required")
        };
        assert!(!assets.is_empty());
        session.client.stop(&source).await.unwrap();
        session.complete(&source, true, 1, seed).await;
        let prefix = [
            session.stdout(&source, "oracle", true).await,
            session.stdout(&source, "server", true).await,
            session.stdout(&source, "client", true).await,
        ];
        eprintln!("source at capture: rounds={captured_rounds} ns={captured_ns} stdout={prefix:?}");
        // At capture: the oracle's sleep was pending, the server was serving,
        // and the client had connected but its stream had not completed.
        assert_eq!(prefix[0], "");
        assert_eq!(prefix[1], PROBE_READY);
        assert!(prefix[2].contains(PROBE_CLIENT_MID));
        assert_eq!(count(&prefix[2], PROBE_TCP_DONE), 0);
        // The stream's data crossed both ways before capture, and routed
        // frames were in flight across the capture boundary.
        let (frames, missing) = source_frames.await.unwrap();
        assert_eq!(missing, 0, "source frame batches omitted");
        assert!(frames.iter().all(|f| f.sent_ns <= captured_ns));
        let in_flight = frames
            .iter()
            .filter(|f| {
                f.fate == seedbox_api::FrameFate::Scheduled
                    && f.due_ns.is_some_and(|due| due > captured_ns)
            })
            .collect::<Vec<_>>();
        let up = data_frames(&frames, "client", "server").count();
        let down = data_frames(&frames, "server", "client").count();
        eprintln!("in flight at capture: frames={} data_client_to_server={up} data_server_to_client={down} in_flight={in_flight:?} oracle_sleep_pending=true captured_ns={captured_ns}", frames.len());
        assert!(up > 0 && down > 0, "stream data had not crossed both ways");
        assert!(!in_flight.is_empty(), "no routed frame in flight at capture");
        session.client.delete_checkpoint(&source, "saved").await.unwrap();
        session.client.delete(&source).await.unwrap();
        assert!(!session.execution_root.join("runs").join(source.as_str()).exists());

        // Import from the exported content closure alone, on four workers.
        session.workers.store(4, Ordering::SeqCst);
        let restored = session
            .client
            .submit_definition(&serde_json::to_value(exported).unwrap(), true)
            .await
            .unwrap()
            .metadata
            .id;
        session.submitted += 1;
        assert_ne!(restored, source);
        session.worker(&restored).await;
        let launch = session.launch_request(&restored);
        assert!(launch.resume_checkpoint.as_ref().expect("restore launch must name its checkpoint").is_file());
        session
            .control(&restored, "verify-paused-restore", ControlCommand::SetRate { rate: PlaybackRate::Paused })
            .await;
        let paused = session.client.playback(&restored).await.unwrap().state;
        assert_eq!(paused.rate, PlaybackRate::Paused);
        assert!(!paused.stepping && !paused.terminal);
        assert_eq!(paused.position.completed_rounds, captured_rounds);
        assert_eq!(paused.position.simulated_time_ns, captured_ns);
        let restored_frames = collect_frames(&session.client, &restored).await;
        session
            .control(&restored, "continue", ControlCommand::SetRate { rate: PlaybackRate::Unlimited })
            .await;
        let continued = session.complete(&restored, false, 4, seed).await.unwrap();
        assert_eq!(
            session.client.describe(&restored).await.unwrap().status.simulated_time_ns,
            final_ns
        );
        phase("restored_completed");
        // The stream carried on from the captured connection: only frames
        // routed after the capture, including more of its data, appear.
        let (frames, missing) = restored_frames.await.unwrap();
        assert_eq!(missing, 0, "restored frame batches omitted");
        assert!(frames.iter().all(|f| f.sent_ns >= captured_ns));
        let up = data_frames(&frames, "client", "server").count();
        let down = data_frames(&frames, "server", "client").count();
        eprintln!("restored frames={} data_client_to_server={up} data_server_to_client={down}", frames.len());
        assert!(up > 0 && down > 0, "stream data did not continue after restore");
        // The continuation resumed every guest rather than restarting any: its
        // output is exactly the uninterrupted remainder after the capture.
        for (index, machine) in ["oracle", "server", "client"].into_iter().enumerate() {
            let rest = session.stdout(&restored, machine, false).await;
            assert_eq!(format!("{}{rest}", prefix[index]), expected[index], "{machine}");
        }
        assert_eq!(uninterrupted, continued, "raw uninterrupted/continued native witnesses differ");
        let comparison = session.client.compare(&baseline, &restored).await.unwrap();
        assert_eq!(comparison.witness_matches, Some(true));
        eprintln!("in-flight checkpoint continuation: backend={} raw_equal=true witness={continued}", label(backend));
        session.close().await;
    })
    .await
    .expect("in-flight checkpoint exceeded its hang guard");
}
/// Bytes and frames scheduled from `from` to `to`.
fn scheduled(frames: &[seedbox_api::Frame], from: &str, to: &str) -> (u64, u64) {
    frames
        .iter()
        .filter(|f| f.from == from && f.to == to && f.fate == seedbox_api::FrameFate::Scheduled)
        .fold((0, 0), |(count, bytes), f| {
            (count + 1, bytes + u64::from(f.bytes))
        })
}
/// Frames are observation only: in the bd-f50 network world, fresh runs with
/// observations on and off, on one and four workers, and continuations of a
/// mid-transfer checkpoint with observations on and off, all reach one raw
/// witness. Observed runs publish their routed frames and link counters.
pub async fn pv_network_frames_are_observation_only() {
    use seedbox_api::{CheckpointState, ControlCommand, ControlState, PlaybackRate};
    let backend = DriverBackend::Pv;
    // Hang guard only: every decision below is taken from guest events.
    tokio::time::timeout(HANG_GUARD, async {
        let seed = 123;
        let mut session = NativeSession::new(backend);
        let request = network_submission(backend, session.root.path(), &session.store, seed);
        let plain = session.submit(request.clone(), 1).await;
        session.worker(&plain).await;
        session.complete(&plain, false, 1, seed).await.unwrap();
        let client_timed = session.stdout_timed(&plain, "client", false).await;
        let tcp_begin_ns = marker_time(&client_timed, PROBE_CLIENT_MID);
        let tcp_end_ns = marker_time(&client_timed, PROBE_TCP_DONE);
        let request =
            with_capture_marker(request, tcp_begin_ns + (tcp_end_ns - tcp_begin_ns) / 2);

        // Fresh runs: subscribe while paused at start, so every frame is seen.
        let mut witnesses = Vec::new();
        let mut reference = None;
        for (observed, workers) in [(true, 1), (true, 4), (false, 1), (false, 4)] {
            session.observed.store(observed, Ordering::SeqCst);
            let mut paused = request.clone();
            paused.options.start_paused = true;
            let id = session.submit(paused, workers).await;
            session.worker(&id).await;
            session
                .control(&id, "frames-paused", ControlCommand::SetRate { rate: PlaybackRate::Paused })
                .await;
            let frames = collect_frames(&session.client, &id).await;
            session
                .control(&id, "frames-run", ControlCommand::SetRate { rate: PlaybackRate::Unlimited })
                .await;
            let witness = session.complete(&id, false, workers, seed).await.unwrap();
            let (frames, missing) = frames.await.unwrap();
            let up = scheduled(&frames, "client", "server");
            let down = scheduled(&frames, "server", "client");
            eprintln!("fresh observed={observed} workers={workers} witness={witness} frames={} missing={missing} client_to_server={up:?} server_to_client={down:?}", frames.len());
            if observed {
                // The whole echoed stream crosses each direction, every frame
                // scheduled for a later delivery, with nothing omitted.
                assert_eq!(missing, 0);
                assert!(up.1 >= 262_144 && down.1 >= 262_144, "{up:?} {down:?}");
                assert!(frames.iter().all(|f| f.fate == seedbox_api::FrameFate::Scheduled
                    && f.due_ns.is_some_and(|due| due > f.sent_ns)));
                let world = session.client.world(&id).await.unwrap();
                for (from, to) in [("client", "server"), ("server", "client")] {
                    let link = world.links.iter().find(|l| l.from == from && l.to == to)
                        .unwrap_or_else(|| panic!("{from}->{to} link: {:?}", world.links));
                    let (count, bytes) = scheduled(&frames, from, to);
                    assert_eq!((link.counters.scheduled, link.counters.bytes), (count, bytes));
                    assert_eq!(
                        link.counters.last_sent_ns,
                        frames.iter().filter(|f| f.from == from && f.to == to).map(|f| f.sent_ns).max()
                    );
                }
                let stream = (frames.len(), up, down);
                assert_eq!(*reference.get_or_insert(stream), stream, "frames depend on workers");
            } else {
                assert!(frames.is_empty() && missing == 0, "unobserved run published frames");
            }
            witnesses.push(witness);
        }
        assert!(witnesses.windows(2).all(|pair| pair[0] == pair[1]), "{witnesses:?}");
        let uninterrupted = witnesses[0].clone();

        // Capture mid-transfer, as in the bd-f50 continuation, while observed.
        session.observed.store(true, Ordering::SeqCst);
        let mut capture = request.clone();
        capture.options.start_paused = true;
        let source = session.submit(capture, 1).await;
        session.worker(&source).await;
        session
            .control(&source, "verify-paused-source", ControlCommand::SetRate { rate: PlaybackRate::Paused })
            .await;
        let source_frames = collect_frames(&session.client, &source).await;
        session
            .control(&source, "run-to-marker", ControlCommand::PauseBeforeNextFault { rate: PlaybackRate::Unlimited })
            .await;
        loop {
            let status = session.client.playback(&source).await.unwrap().state;
            assert!(!status.terminal, "fixture ended before the in-flight capture");
            if status.rate == PlaybackRate::Paused && !status.stepping {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let before = session.client.playback(&source).await.unwrap().state.position.completed_rounds;
        session.control(&source, "apply-marker", ControlCommand::Step).await;
        loop {
            let status = session.client.playback(&source).await.unwrap().state;
            assert!(!status.terminal);
            if !status.stepping && status.position.completed_rounds > before {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let captured = session.control(&source, "saved", ControlCommand::Checkpoint).await;
        let captured_ns = match &captured.state {
            ControlState::Acknowledged { checkpoint: Some(checkpoint), error: None, .. }
                if checkpoint.state == CheckpointState::Captured =>
            {
                checkpoint.position.simulated_time_ns
            }
            other => panic!("checkpoint not captured: {other:?}"),
        };
        assert!(captured_ns > tcp_begin_ns && captured_ns < tcp_end_ns);
        let exported = session.client.checkpoint_definition(&source, "saved").await.unwrap();
        session.client.stop(&source).await.unwrap();
        session.complete(&source, true, 1, seed).await;
        let (frames, missing) = source_frames.await.unwrap();
        assert_eq!(missing, 0);
        assert!(frames.iter().all(|f| f.sent_ns <= captured_ns));
        let in_flight = frames
            .iter()
            .filter(|f| f.due_ns.is_some_and(|due| f.sent_ns <= captured_ns && due > captured_ns))
            .count();
        eprintln!("source capture: captured_ns={captured_ns} frames={} in_flight={in_flight}", frames.len());
        assert!(in_flight > 0, "no routed frame in flight at capture");

        // Continue the one exported capture observed and unobserved.
        let definition = serde_json::to_value(exported).unwrap();
        for (observed, workers) in [(true, 4), (false, 1)] {
            session.observed.store(observed, Ordering::SeqCst);
            session.workers.store(workers, Ordering::SeqCst);
            let restored = session.client.submit_definition(&definition, true).await.unwrap().metadata.id;
            session.submitted += 1;
            session.worker(&restored).await;
            assert!(session.launch_request(&restored).resume_checkpoint.is_some());
            session
                .control(&restored, "verify-paused-restore", ControlCommand::SetRate { rate: PlaybackRate::Paused })
                .await;
            let paused = session.client.playback(&restored).await.unwrap().state;
            assert_eq!(paused.position.simulated_time_ns, captured_ns);
            let frames = collect_frames(&session.client, &restored).await;
            session
                .control(&restored, "continue", ControlCommand::SetRate { rate: PlaybackRate::Unlimited })
                .await;
            let continued = session.complete(&restored, false, workers, seed).await.unwrap();
            let (frames, missing) = frames.await.unwrap();
            eprintln!("restored observed={observed} workers={workers} witness={continued} frames={} missing={missing}", frames.len());
            if observed {
                // Only frames routed after the capture are published again.
                assert_eq!(missing, 0);
                assert!(!frames.is_empty());
                assert!(frames.iter().all(|f| f.sent_ns >= captured_ns));
            } else {
                assert!(frames.is_empty() && missing == 0);
            }
            assert_eq!(continued, uninterrupted, "restored observed={observed} workers={workers}");
        }
        session.client.delete_checkpoint(&source, "saved").await.unwrap();
        session.client.delete(&source).await.unwrap();
        eprintln!("frames observation-only: witness={uninterrupted} fresh=on1,on4,off1,off4 restored=on4,off1");
        session.close().await;
    })
    .await
    .expect("frames observation proof exceeded its hang guard");
}
fn runtime_backend(backend: DriverBackend) -> runtime_assets::Backend {
    match backend {
        DriverBackend::Pv => runtime_assets::Backend::Pv,
        DriverBackend::Tcg => runtime_assets::Backend::Tcg,
    }
}
fn label(backend: DriverBackend) -> &'static str {
    match backend {
        DriverBackend::Pv => "pv",
        DriverBackend::Tcg => "tcg",
    }
}
fn scratch_prefix(backend: DriverBackend) -> &'static str {
    match backend {
        DriverBackend::Pv => "seedbox-pv-",
        DriverBackend::Tcg => "seedbox-tcg-",
    }
}
