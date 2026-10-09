//! Adapter for the controlled Seedbox KVM engine. World JSON is an internal
//! execution artifact generated from verified inputs, not the authoring format.

use crate::{
    counter::CounterProfile,
    execution::*,
    store::{ContentDigest, ContentStore},
    Error, Result,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
};

use seedbox_engine_core::checkpoint;
/// Execution failure whose VMM stderr may be excerpted by the CLI.
pub const VMM_FAILURE_MESSAGE: &str = "VMM failed; see vmm.stderr";

/// VMM `--out` directory relative to run output; recursive retention keeps this prefix.
pub(crate) const MACHINE_OUTPUT_DIR: &str = "machines";

/// Manifest-relative page evidence, beside the VMM's per-machine traces.
pub(crate) fn page_hash_log(machine: &str) -> String {
    format!("{MACHINE_OUTPUT_DIR}/{machine}.pages")
}

#[derive(Clone, Debug)]
pub struct VmmExecutor {
    /// Host CPU to pin the VMM to; `None` leases the first free eligible core.
    pub cpu: Option<u32>,
    counter: CounterProfile,
    profile: ExecutionProfile,
    performance: BTreeMap<String, String>,
    admission: std::sync::Arc<std::sync::Mutex<Vec<Vec<crate::admission::CoreObservation>>>>,
}

/// Successful compatibility observation; it is not empirical replay qualification.
#[derive(Debug, Serialize)]
pub struct DoctorReport {
    pub timebase: Timebase,
    pub cpu: u32,
    pub cpus: Vec<u32>,
    pub engine: ContentDigest,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub simulator_version: Option<String>,
    pub kernel: ContentDigest,
    pub observations: Vec<crate::admission::CoreObservation>,
}

struct AdmissionProbe {
    cpu: Option<u32>,
    observations: Vec<crate::admission::CoreObservation>,
    result: Result<()>,
}

impl VmmExecutor {
    /// Detect the launch contract of an explicitly supplied executable.
    pub fn new_detected(
        engine: &Path,
        store: &ContentStore,
        cpu: Option<u32>,
        memory_mb: u32,
        witness_mode: WitnessMode,
        counter: CounterProfile,
    ) -> Result<Self> {
        Self::new(engine, store, cpu, memory_mb, witness_mode, counter)?
            .with_detected_contract(store, engine)
    }

    /// Preserve simulation settings and use the replacement's own argv contract.
    /// Normal replay uses `from_retained` without probing or rewriting metadata.
    pub fn from_override(
        engine: &Path,
        store: &ContentStore,
        profile: ExecutionProfile,
        cpu: Option<u32>,
        counter: CounterProfile,
    ) -> Result<Self> {
        Self::from_retained(engine, store, profile, cpu, counter)?
            .with_detected_contract(store, engine)
    }

    /// Detect a retained executable after resolving other inputs.
    /// `supplied_path` identifies the original --vmm value in diagnostics.
    pub fn with_detected_contract(
        mut self,
        store: &ContentStore,
        supplied_path: &Path,
    ) -> Result<Self> {
        // Probe the retained copy, so the metadata describes the bytes we run.
        let scratch = tempfile::tempdir()?;
        let engine = store.materialize(&self.profile.engine, scratch.path())?;
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&engine, std::fs::Permissions::from_mode(0o500))?;
        let version = crate::worker_process::replacement_version(&engine, supplied_path)?;
        for key in crate::engine::DESCRIPTOR_FIELDS {
            self.profile.semantic_parameters.remove(key);
        }
        if let Some(version) = version {
            self.profile.semantic_parameters.extend(BTreeMap::from([
                (
                    "engine_executable_kind".into(),
                    crate::engine::INTERNAL_WORKER_V1.into(),
                ),
                ("simulator_version".into(), version),
                ("host_os".into(), "linux".into()),
                ("host_arch".into(), "x86_64".into()),
                ("guest_arch".into(), "x86_64".into()),
                ("engine_backend".into(), "kvm".into()),
                ("engine_timebase".into(), "counter-pv-v1".into()),
            ]));
        }
        Ok(self)
    }

    /// Retain this Seedbox executable and explicitly record its worker contract.
    pub fn new_internal(
        engine: &Path,
        store: &ContentStore,
        cpu: Option<u32>,
        memory_mb: u32,
        witness_mode: WitnessMode,
        counter: CounterProfile,
    ) -> Result<Self> {
        let descriptor = crate::engine::internal_parameters()?;
        let mut executor = Self::new(engine, store, cpu, memory_mb, witness_mode, counter)?;
        executor.profile.semantic_parameters.extend(descriptor);
        Ok(executor)
    }

    /// Preserve retained metadata exactly, without detecting a new launch contract.
    /// A replacement executable changes only
    /// its digest, so the orchestrator can retain profile/admission rejection.
    pub fn from_retained(
        engine: &Path,
        store: &ContentStore,
        mut profile: ExecutionProfile,
        cpu: Option<u32>,
        counter: CounterProfile,
    ) -> Result<Self> {
        profile.engine = store.put_file(engine)?;
        Ok(Self {
            cpu,
            counter,
            profile,
            performance: BTreeMap::from([
                ("SEEDBOX_MARGIN".into(), "adaptive".into()),
                ("SEEDBOX_UNDO".into(), "256".into()),
            ]),
            admission: Default::default(),
        })
    }

    pub fn new(
        engine: &Path,
        store: &ContentStore,
        cpu: Option<u32>,
        memory_mb: u32,
        witness_mode: WitnessMode,
        counter: CounterProfile,
    ) -> Result<Self> {
        if !(64..=65536).contains(&memory_mb) {
            return Err(Error::Execution(
                "guest memory must be 64..65536 MiB".into(),
            ));
        }
        Self::from_engine_digest(
            store.put_file(engine)?,
            cpu,
            memory_mb,
            witness_mode,
            counter,
        )
    }

    fn from_engine_digest(
        engine: ContentDigest,
        cpu: Option<u32>,
        memory_mb: u32,
        witness_mode: WitnessMode,
        counter: CounterProfile,
    ) -> Result<Self> {
        if !(64..=65536).contains(&memory_mb) {
            return Err(Error::Execution(
                "guest memory must be 64..65536 MiB".into(),
            ));
        }
        let mut semantic_parameters = BTreeMap::from([
            ("guest_abi".into(), "oci-init-v1".into()),
            ("control_channel".into(), "virtio-console-v1".into()),
            ("memory_mb".into(), memory_mb.to_string()),
        ]);
        if counter != CounterProfile::Zen {
            semantic_parameters.insert(COUNTER.into(), counter.name().into());
        }
        Ok(Self {
            cpu,
            counter,
            admission: Default::default(),
            profile: ExecutionProfile {
                backend: "seedbox-kvm".into(),
                engine,
                witness_mode,
                semantic_parameters,
            },
            performance: BTreeMap::from([
                ("SEEDBOX_MARGIN".into(), "adaptive".into()),
                ("SEEDBOX_UNDO".into(), "256".into()),
            ]),
        })
    }

    /// Defer virtio-net TX kicks to deterministic polls (semantic, off by default).
    pub fn with_deferred_tx(mut self, enabled: bool) -> Self {
        if enabled {
            self.profile
                .semantic_parameters
                .insert("deferred_tx".into(), "true".into());
        } else {
            self.profile.semantic_parameters.remove("deferred_tx");
        }
        self
    }

    /// Counter is recorded by omission to preserve existing run identities.
    /// PV reads no branch counter, so its identity carries no counter profile.
    pub fn with_timebase(mut self, timebase: Timebase) -> Self {
        self.profile.semantic_parameters.remove(COUNTER);
        if timebase == Timebase::Counter {
            self.profile.semantic_parameters.remove("timebase");
            if self.counter != CounterProfile::Zen {
                self.profile
                    .semantic_parameters
                    .insert(COUNTER.into(), self.counter.name().into());
            }
        } else {
            self.profile
                .semantic_parameters
                .insert("timebase".into(), timebase.name().into());
        }
        self
    }

    /// Page timer is recorded only when requested; off preserves legacy identities.
    pub fn with_page_timer(mut self, enabled: bool) -> Self {
        if enabled {
            self.profile
                .semantic_parameters
                .insert("page_timer".into(), "true".into());
        } else {
            self.profile.semantic_parameters.remove("page_timer");
        }
        self
    }

    /// Whether the engine reads the host branch counter; PV does not.
    fn counted(&self) -> bool {
        recorded_timebase(&self.profile).map_or(true, |timebase| timebase == Timebase::Counter)
    }

    /// Check the existing execution policy without storing inputs or starting guests.
    /// Host observations are the same bounded probe used by execution preflight.
    pub fn doctor(engine: &Path, kernel: &Path, cpu: Option<u32>) -> Result<DoctorReport> {
        Self::doctor_with_timebase(engine, kernel, cpu, Timebase::Counter)
    }

    /// Check compatibility for the selected guest progress mode.
    pub fn doctor_with_timebase(
        engine: &Path,
        kernel: &Path,
        cpu: Option<u32>,
        timebase: Timebase,
    ) -> Result<DoctorReport> {
        let executor = Self::from_engine_digest(
            crate::admission::file_digest(engine)?,
            cpu,
            1024,
            WitnessMode::Exits,
            CounterProfile::Zen,
        )?;
        Self::doctor_executor(executor.with_timebase(timebase), kernel)
    }

    /// Inspect the built-in worker without storing bytes or invoking it.
    pub fn doctor_internal(engine: &Path, kernel: &Path, cpu: Option<u32>) -> Result<DoctorReport> {
        Self::doctor_internal_with_timebase(engine, kernel, cpu, Timebase::Counter)
    }

    /// Check the built-in worker for the selected progress mode without starting it.
    pub fn doctor_internal_with_timebase(
        engine: &Path,
        kernel: &Path,
        cpu: Option<u32>,
        timebase: Timebase,
    ) -> Result<DoctorReport> {
        let descriptor = crate::engine::internal_parameters()?;
        let mut executor = Self::from_engine_digest(
            crate::admission::file_digest(engine)?,
            cpu,
            1024,
            WitnessMode::Exits,
            CounterProfile::Zen,
        )?;
        executor.profile.semantic_parameters.extend(descriptor);
        Self::doctor_executor(executor.with_timebase(timebase), kernel)
    }

    fn doctor_executor(executor: Self, kernel: &Path) -> Result<DoctorReport> {
        crate::admission::validate_profile(&executor.profile)?;
        let kernel = crate::admission::file_digest(kernel)?;
        if !executor.counted() {
            #[cfg(target_os = "linux")]
            {
                crate::host::check_kvm_access()?;
                let mut report = None;
                with_capacity(
                    1,
                    || crate::host::CpuReservation::acquire(executor.cpu),
                    |reservations| {
                        let reservation = reservations.first().ok_or_else(|| {
                            Error::Integrity("doctor did not reserve a CPU".into())
                        })?;
                        report = Some(DoctorReport {
                            timebase: Timebase::Pv,
                            cpu: reservation.cpu,
                            cpus: reservation.cpus.iter().copied().collect(),
                            engine: executor.profile.engine.clone(),
                            simulator_version: executor
                                .profile
                                .semantic_parameters
                                .get("simulator_version")
                                .cloned(),
                            kernel: kernel.clone(),
                            observations: Vec::new(),
                        });
                        Ok(())
                    },
                )?;
                return report.ok_or_else(|| {
                    Error::Integrity("doctor did not collect its reserved CPU".into())
                });
            }
            #[cfg(not(target_os = "linux"))]
            return Err(Error::Setup(
                "PV compatibility checks require a Linux KVM host".into(),
            ));
        }
        let probe = executor.probe();
        executor.doctor_report(kernel, probe)
    }

    fn doctor_report(&self, kernel: ContentDigest, probe: AdmissionProbe) -> Result<DoctorReport> {
        probe.result?;
        crate::admission::validate_profile(&self.profile)?;
        for observation in &probe.observations {
            crate::admission::validate_observation(observation)?;
        }
        Ok(DoctorReport {
            timebase: recorded_timebase(&self.profile)?,
            cpu: probe
                .cpu
                .ok_or_else(|| Error::Execution("host probe did not select a CPU".into()))?,
            cpus: probe
                .observations
                .iter()
                .map(|observation| observation.cpu)
                .collect(),
            engine: self.profile.engine.clone(),
            simulator_version: self
                .profile
                .semantic_parameters
                .get("simulator_version")
                .cloned(),
            kernel,
            observations: probe.observations,
        })
    }

    fn probe(&self) -> AdmissionProbe {
        self.probe_capacity(1)
    }

    fn probe_capacity(&self, workers: u32) -> AdmissionProbe {
        let mut selected = None;
        let mut observations = Vec::new();
        let result = (|| -> Result<()> {
            #[cfg(target_os = "linux")]
            {
                with_capacity(
                    workers,
                    || crate::host::CpuReservation::acquire(self.cpu),
                    |reservations| {
                        selected = reservations.first().map(|reservation| reservation.cpu);
                        for reservation in reservations {
                            crate::admission::collect_admitted(
                                reservation.cpus.iter().copied(),
                                &mut observations,
                                |cpu| crate::admission::observe_counter(cpu, self.counter),
                            )?;
                        }
                        Ok(())
                    },
                )?; // All reservations/perf/affinity close before publication or OCI work.
            }
            #[cfg(not(target_os = "linux"))]
            {
                let _ = (&mut selected, workers);
                observations.push(crate::admission::observe_counter(0, self.counter)?);
            }
            Ok(())
        })();
        AdmissionProbe {
            cpu: selected,
            observations,
            result,
        }
    }

    /// Bind a runtime checkpoint and its branch seed into retained semantics.
    pub fn with_checkpoint(mut self, at_ns: u64, branch_seed: u64) -> Self {
        self.profile
            .semantic_parameters
            .insert("checkpoint_at_ns".into(), at_ns.to_string());
        self.profile
            .semantic_parameters
            .insert("checkpoint_branch_seed".into(), branch_seed.to_string());
        self
    }

    /// Reconstruct checkpoint settings from retained inputs during replay.
    pub fn with_recorded_checkpoint(self, profile: &ExecutionProfile) -> Result<Self> {
        Ok(match recorded_checkpoint(profile)? {
            Some((at, seed)) => self.with_checkpoint(at, seed),
            None => self,
        })
    }

    /// Seeded survival of the latest unflushed 512-byte sectors on crash.
    /// Off is represented by omission to preserve existing profile bytes.
    pub fn with_torn_writes(mut self, enabled: bool) -> Self {
        if enabled {
            self.profile
                .semantic_parameters
                .insert(DISK_LOSS_SECTOR_BYTES.into(), "512".into());
            self.profile
                .semantic_parameters
                .insert(DISK_LOSS_SURVIVAL_PROBABILITY.into(), "0.5".into());
        } else {
            self.profile
                .semantic_parameters
                .remove(DISK_LOSS_SECTOR_BYTES);
            self.profile
                .semantic_parameters
                .remove(DISK_LOSS_SURVIVAL_PROBABILITY);
        }
        self
    }

    pub fn with_margin(mut self, margin: &str) -> Result<Self> {
        if margin != "adaptive" && margin.parse::<u64>().ok().is_none_or(|value| value == 0) {
            return Err(Error::Execution(
                "margin must be adaptive or a positive integer".into(),
            ));
        }
        self.performance
            .insert("SEEDBOX_MARGIN".into(), margin.into());
        Ok(self)
    }

    /// Initial undo margin in branches; zero disables speculative stop recovery.
    pub fn with_undo(mut self, branches: u64) -> Result<Self> {
        if branches > (1 << 18) {
            return Err(Error::Execution("undo margin must be 0..262144".into()));
        }
        self.performance
            .insert("SEEDBOX_UNDO".into(), branches.to_string());
        Ok(self)
    }
}

/// Checkpoints are optional, but partial or malformed retained settings fail closed.
pub fn recorded_checkpoint(profile: &ExecutionProfile) -> Result<Option<(u64, u64)>> {
    let parameters = &profile.semantic_parameters;
    match (
        parameters.get("checkpoint_at_ns"),
        parameters.get("checkpoint_branch_seed"),
    ) {
        (None, None) => Ok(None),
        (Some(at), Some(seed)) => {
            let invalid = || Error::Integrity("invalid checkpoint semantic parameters".into());
            let t: u64 = at.parse().map_err(|_| invalid())?;
            let b: u64 = seed.parse().map_err(|_| invalid())?;
            if at != &t.to_string() || seed != &b.to_string() {
                return Err(invalid());
            }
            Ok(Some((t, b)))
        }
        _ => Err(Error::Integrity(
            "incomplete checkpoint semantic parameters".into(),
        )),
    }
}

const COUNTER: &str = "counter";
const DISK_LOSS: &str = "disk_loss";
const DISK_LOSS_SECTOR_BYTES: &str = "disk_loss_sector_bytes";
const DISK_LOSS_SURVIVAL_PROBABILITY: &str = "disk_loss_survival_probability";

/// Recover the disk policy from a retained profile, rejecting unknown semantics.
pub fn recorded_torn_writes(profile: &ExecutionProfile) -> Result<bool> {
    if profile.semantic_parameters.contains_key(DISK_LOSS) {
        return Err(Error::Integrity(
            "legacy disk_loss semantic parameter is not replay-compatible; use the retained previous CLI for historical evidence".into(),
        ));
    }
    let sector_bytes = profile.semantic_parameters.get(DISK_LOSS_SECTOR_BYTES);
    let survival_probability = profile
        .semantic_parameters
        .get(DISK_LOSS_SURVIVAL_PROBABILITY);
    if sector_bytes.is_none() && survival_probability.is_none() {
        return Ok(false);
    }
    match (sector_bytes, survival_probability) {
        (Some(sector_bytes), Some(survival_probability))
            if sector_bytes == "512" && survival_probability == "0.5" =>
        {
            Ok(true)
        }
        _ => Err(Error::Integrity("unknown disk loss parameters".into())),
    }
}

/// The branch counter profile a recorded run was made with.
pub fn recorded_counter(profile: &ExecutionProfile) -> Result<CounterProfile> {
    match profile.semantic_parameters.get(COUNTER) {
        None => Ok(CounterProfile::Zen),
        Some(name) => CounterProfile::from_name(name)
            .ok_or_else(|| Error::Integrity(format!("unknown counter profile {name}"))),
    }
}

impl VmmExecutor {
    fn preflight_capacity(&self, workers: u32, output: Option<&Path>) -> Result<()> {
        recorded_checkpoint(&self.profile)?;
        crate::admission::validate_profile(&self.profile)?;
        if !self.counted() {
            return self.uncounted_preflight(workers);
        }
        let probe = self.probe_capacity(workers);
        let observations = probe.observations;
        let collection = probe.result;
        crate::admission::finish_admission(collection, |collection| {
            let mut saved = self
                .admission
                .lock()
                .map_err(|_| Error::Execution("host admission evidence lock poisoned".into()))?;
            if saved.len() == 4 {
                saved.remove(0);
            }
            saved.push(observations.clone());
            if let Some(output) = output {
                std::fs::write(
                    output.join("host-static.json"),
                    serde_json::to_vec_pretty(&*saved)?,
                )?;
                if let Err(error) = collection {
                    std::fs::write(output.join("host-static-error.txt"), error.to_string())?;
                }
            }
            drop(saved);
            Ok(())
        })
    }

    /// PV needs only KVM and free cores: no perf event or workaround MSR is opened.
    fn uncounted_preflight(&self, workers: u32) -> Result<()> {
        #[cfg(target_os = "linux")]
        {
            crate::host::check_kvm_access()?;
            with_capacity(
                workers,
                || crate::host::CpuReservation::acquire(self.cpu),
                |_| Ok(()),
            )
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = workers;
            Err(Error::Setup(
                "PV execution requires a Linux KVM host".into(),
            ))
        }
    }
}

/// Keep the entire capacity reserved during admission, then release it before
/// returning on either success or failure. The reservation enforces disjoint cores.
#[cfg(any(target_os = "linux", test))]
fn with_capacity<T>(
    workers: u32,
    mut reserve: impl FnMut() -> Result<T>,
    check: impl FnOnce(&[T]) -> Result<()>,
) -> Result<()> {
    let mut reservations = Vec::new();
    for _ in 0..workers {
        reservations.push(reserve().map_err(|error| {
            if workers > 1 {
                Error::Setup(format!("--workers {workers}: cannot reserve {workers} distinct eligible physical cores: {error}"))
            } else {
                error
            }
        })?);
    }
    check(&reservations)
}

/// Build the retained executable's launch contract without consulting the host.
pub(crate) fn engine_command(
    engine: &Path,
    profile: &ExecutionProfile,
) -> Result<std::process::Command> {
    let mut command = std::process::Command::new(engine);
    match crate::engine::recorded_launch(profile)? {
        crate::engine::ExecutableKind::Standalone => {}
        crate::engine::ExecutableKind::InternalWorker => {
            command.args(["__engine", crate::engine::INTERNAL_WORKER_V1]);
        }
        crate::engine::ExecutableKind::ArmWorld => {
            command.args(["__engine", "arm64-world-v1"]);
            return Ok(command);
        }
    }
    let timebase = recorded_timebase(profile)?;
    if timebase != Timebase::Counter {
        command.arg("--timebase").arg(timebase.name());
    }
    if recorded_page_timer(profile) {
        command.arg("--page-timer");
    }
    if recorded_deferred_tx(profile)? {
        command.arg("--deferred-tx");
    }
    Ok(command)
}

/// Missing page timer means the legacy port transport.
pub fn recorded_page_timer(profile: &ExecutionProfile) -> bool {
    profile
        .semantic_parameters
        .get("page_timer")
        .is_some_and(|v| v == "true")
}

/// Older engines and profiles service TX without deferring kicks.
pub fn recorded_deferred_tx(profile: &ExecutionProfile) -> Result<bool> {
    match profile.semantic_parameters.get("deferred_tx").map(String::as_str) {
        None => Ok(false),
        Some("true") => Ok(true),
        Some(value) => Err(Error::Integrity(format!(
            "recorded --deferred-tx {value}: expected true or an absent parameter; use a run recorded with a supported TX mode"
        ))),
    }
}

/// Missing timebase means counter, including records from older engines.
pub fn recorded_timebase(profile: &ExecutionProfile) -> Result<Timebase> {
    match profile.semantic_parameters.get("timebase").map(String::as_str) {
        None | Some("counter") => Ok(Timebase::Counter),
        Some("pv") => Ok(Timebase::Pv),
        Some(value) => Err(Error::Integrity(format!(
            "recorded --timebase {value}: expected counter or pv; use a run recorded with a supported timebase"
        ))),
    }
}

impl ExecutionBackend for VmmExecutor {
    fn preflight_workers(&self, workers: u32) -> Result<()> {
        if !(1..=32).contains(&workers) {
            return Err(Error::Setup("--workers: use 1..32".into()));
        }
        if workers > 1 && self.cpu.is_some() {
            return Err(Error::Setup(
                "--workers: parallel exploration requires automatic core selection; omit --cpu and SEEDBOX_CPU".into(),
            ));
        }
        self.preflight_capacity(workers, None)
    }

    fn preflight(&self, output: Option<&Path>) -> Result<()> {
        self.preflight_capacity(1, output)
    }
    fn validate_retained(&self, inputs: &PreparedInputs) -> Result<()> {
        crate::admission::validate_profile(&inputs.profile)?;
        if self.counted() && recorded_counter(&inputs.profile)? != self.counter {
            return Err(Error::Integrity(
                "recorded branch counter differs from executor".into(),
            ));
        }
        Ok(())
    }

    fn profile(&self) -> ExecutionProfile {
        self.profile.clone()
    }
    fn performance_parameters(&self) -> BTreeMap<String, String> {
        self.performance.clone()
    }
    fn execute(&self, request: ExecutionRequest<'_>) -> Result<ExecutionReport> {
        #[cfg(target_os = "linux")]
        {
            linux::execute(self, request)
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = request;
            Err(Error::Execution(
                "KVM execution requires a Linux x86-64 host".into(),
            ))
        }
    }
}

fn control_latency(request: &ExecutionRequest<'_>) -> Result<u64> {
    request
        .network
        .latency_ns
        .checked_mul(2)
        .and_then(|n| n.checked_add(request.schedule.stop_tolerance_ns.checked_mul(2)?))
        .and_then(|n| n.checked_add(1_000_000))
        .ok_or_else(|| Error::Execution("control latency overflow".into()))
}

/// Size of the retained disk of a machine whose service mounts named volumes.
/// Part of the execution semantics: changing it changes runs.
pub const VOLUME_DISK_MIB: u64 = 1024;

/// Translate an execution request without consulting registry tags or source
/// paths. Exposed for preparation tools and report inspection without KVM.
pub fn world(request: &ExecutionRequest<'_>) -> Result<Value> {
    if request
        .control
        .is_some_and(|c| c.service != request.check.service)
    {
        return Err(Error::Execution(
            "controller must also be the check service".into(),
        ));
    }
    if request.prepared.guests.is_empty() || request.prepared.guests.len() > 254 {
        return Err(Error::Integrity("expected 1..254 prepared machines".into()));
    }
    let memory: u32 = request
        .prepared
        .profile
        .semantic_parameters
        .get("memory_mb")
        .ok_or_else(|| Error::Integrity("profile missing memory_mb".into()))?
        .parse()
        .map_err(|_| Error::Integrity("invalid memory_mb".into()))?;
    let artifact = |digest: &ContentDigest| {
        let path = request.artifacts.get(digest).ok_or_else(|| {
            Error::Integrity(format!("missing verified artifact {}", digest.hex()))
        })?;
        Ok::<_, Error>(std::path::absolute(path)?)
    };
    let workload_machine = request
        .prepared
        .placement
        .get(&request.check.service)
        .ok_or_else(|| Error::Integrity("missing workload placement".into()))?;
    // This guest ABI launches one application in each machine. The public
    // execution contract also permits other backends to co-locate services.
    if request.prepared.placement.len() != request.prepared.guests.len()
        || request.prepared.placement.values().collect::<BTreeSet<_>>()
            != request.prepared.guests.keys().collect::<BTreeSet<_>>()
    {
        return Err(Error::Execution(
            "oci-init-v1 requires one service per machine".into(),
        ));
    }
    let control = request.control.map(|c| -> Result<Value> {
        let machine = request.prepared.placement.get(&c.service).ok_or_else(|| Error::Execution("controller placement missing".into()))?;
        let latency_ns = control_latency(request)?;
        Ok(json!({"machine":machine,"latency_ns":latency_ns,"placement":request.prepared.placement}))
    }).transpose()?;
    let machines = request
        .prepared
        .guests
        .iter()
        .map(|(name, digest)| {
            let mut machine = json!({"name":name,"initrd":artifact(digest)?});
            // A machine gets a disk when a service placed on it mounts a volume.
            let volumes = request.prepared.placement.iter().any(|(service, machine)| {
                machine == name
                    && request
                        .prepared
                        .images
                        .get(service)
                        .is_some_and(|image| !image.launch.disk_volumes.is_empty())
            });
            if volumes {
                machine["disk"] = json!({"size_mib": VOLUME_DISK_MIB});
            }
            Ok(machine)
        })
        .collect::<Result<Vec<_>>>()?;
    let links = request.network.links.iter().map(|link| {
        let machine = |service: &str| request.prepared.placement.get(service).ok_or_else(||Error::Integrity(format!("missing link placement for {service}")));
        let mut policy = json!({"from":machine(&link.from)?,"to":machine(&link.to)?,"latency_ns":link.latency_ns,"jitter_ns":link.jitter_ns,"loss_ppm":link.loss_ppm});
        if let Some(rate) = link.bandwidth_bps { policy["bandwidth_bps"] = json!(rate); }
        if let Some(bound) = link.queue_bound_ns { policy["queue_bound_ns"] = json!(bound); }
        Ok(policy)
    }).collect::<Result<Vec<_>>>()?;
    let mut network =
        json!({"latency_ns":request.network.latency_ns,"jitter_ns":request.network.jitter_ns});
    if let Some(rate) = request.network.bandwidth_bps {
        network["bandwidth_bps"] = json!(rate);
    }
    if let Some(bound) = request.network.queue_bound_ns {
        network["queue_bound_ns"] = json!(bound);
    }
    // Absent unless used, so a world without link overrides is unchanged.
    if !links.is_empty() {
        network["links"] = json!(links);
    }
    let mut world = json!({
        "kernel":artifact(&request.prepared.kernel)?, "mem_mb":memory,
        "duration_ns":request.limits.simulated_time_ns,"end_on_exit":workload_machine,
        "timer_jitter_ns":request.schedule.timer_jitter_ns,"stop_tolerance_ns":request.schedule.stop_tolerance_ns,
        "network":network,
        "machines":machines,"control":control,
        "control_channel":"virtio-console-v1",
    });
    let lookahead = request
        .network
        .latency_ns
        .saturating_sub(request.network.jitter_ns);
    if let Some(skew) = crate::engine::arm_launch_skew(&request.prepared.profile, lookahead)? {
        world["launch_skew_ns"] = json!(skew);
    }
    if recorded_torn_writes(&request.prepared.profile)? {
        world[DISK_LOSS] = match request.prepared.profile.semantic_parameters.get(DISK_LOSS) {
            Some(model) => json!(model), // The retained historical worker owns its disk semantics.
            None => json!({"sector_bytes": 512, "survival_probability": 0.5}),
        };
    }
    if let Some(checkpoint) = checkpoint_metadata(request, &world)? {
        world["checkpoint"] = checkpoint;
    }
    Ok(world)
}

fn checkpoint_metadata(request: &ExecutionRequest<'_>, world: &Value) -> Result<Option<Value>> {
    let Some((at, seed)) = recorded_checkpoint(&request.prepared.profile)? else {
        return Ok(None);
    };
    if at >= request.limits.simulated_time_ns {
        return Err(Error::Execution(
            "checkpoint time must be before the simulated time limit".into(),
        ));
    }
    let parent = checkpoint::parent_identity(
        world,
        request.seed,
        request.prepared.profile.engine.hex(),
        &checkpoint::identity_timebase(
            recorded_timebase(&request.prepared.profile)?.name(),
            recorded_deferred_tx(&request.prepared.profile)?,
        ),
        recorded_page_timer(&request.prepared.profile),
        |path| {
            request
                .artifacts
                .iter()
                .find_map(|(digest, artifact)| {
                    std::path::absolute(artifact)
                        .ok()
                        .filter(|p| p == Path::new(path))
                        .map(|_| digest.hex().to_owned())
                })
                .ok_or_else(|| format!("checkpoint artifact {path} is not verified"))
        },
    )
    .map_err(Error::Integrity)?;
    Ok(Some(
        json!({"at_ns":at,"branch_seed":seed,"parent_identity":parent,
        "branch_identity":checkpoint::branch_identity(&parent,at,seed)}),
    ))
}

#[derive(Deserialize)]
struct RawFault {
    request: seedbox_control::Plan,
    id: u32,
    requested_at_ns: u64,
    applied_at_ns: Option<u64>,
    action: String,
    machine: Option<String>,
    status: String,
    detail: String,
}

/// State parts emitted by `Machine::state_digest`: `events` is descriptive
/// text, while every other part is a lowercase SHA-256 digest. Keep unknown
/// part names digest-checked too, so new hashed parts remain safe to compare.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(try_from = "BTreeMap<String, String>")]
pub(crate) struct StateDigestParts(BTreeMap<String, String>);

impl StateDigestParts {
    pub(crate) fn ram_only_difference(&self, other: &Self) -> bool {
        self.0
            .get("ram")
            .zip(other.0.get("ram"))
            .is_some_and(|(a, b)| a != b)
            && self
                .0
                .iter()
                .filter(|(k, _)| k.as_str() != "ram")
                .eq(other.0.iter().filter(|(k, _)| k.as_str() != "ram"))
    }
}

impl TryFrom<BTreeMap<String, String>> for StateDigestParts {
    type Error = Error;

    fn try_from(parts: BTreeMap<String, String>) -> Result<Self> {
        for (name, value) in &parts {
            if name != "events" {
                ContentDigest::try_from(value.clone())?;
            }
        }
        Ok(Self(parts))
    }
}

#[derive(Deserialize)]
pub(crate) struct MachineReport {
    pub(crate) name: String,
    pub(crate) infrastructure_error: bool,
    pub(crate) guest_exit_code: Option<u8>,
    pub(crate) guest_init_error: Option<u8>,
    pub(crate) state_digest: Option<ContentDigest>,
    pub(crate) state_digest_parts: Option<StateDigestParts>,
    pub(crate) witness: Option<ContentDigest>,
    pub(crate) branches: Option<u64>,
    pub(crate) pending_io: Option<Value>,
    /// Final machine state, `Exited(code)` once the VM has exited.
    #[serde(default)]
    pub(crate) state: Option<String>,
    /// Final simulated time; an exited VM's clock stops at its exit.
    #[serde(default)]
    pub(crate) time_ns: Option<u64>,
    #[serde(default)]
    crashed_incarnations: Vec<CrashedIncarnation>,
}
#[derive(Deserialize)]
struct CrashedIncarnation {
    crashed_at_ns: u64,
}
#[derive(Deserialize)]
pub(crate) struct VmmReport {
    #[serde(default)]
    checkpoint: Option<Value>,
    /// Older engines always used counter.
    timebase: Option<String>,
    #[serde(default)]
    deferred_tx: bool,
    pub(crate) report_version: u32,
    seed: u64,
    /// Absent from engines older than counter profiles, which are all Zen.
    counter_profile: Option<String>,
    outcome: String,
    pub(crate) world_witness: ContentDigest,
    pub(crate) machines: Vec<MachineReport>,
    #[serde(default)]
    faults: Vec<RawFault>,
    #[serde(default)]
    fault_error: Option<String>,
    #[serde(default)]
    control_history: Vec<Value>,
    #[serde(default)]
    control_latency_ns: Option<u64>,
    #[serde(default)]
    triggers: Option<Vec<seedbox_control::TriggerRecord>>,
}

#[derive(Default)]
struct ValidatedControlHistory {
    plans: Vec<seedbox_control::Plan>,
    effects: BTreeMap<u32, u64>,
    /// Process faults resolved without an effect at a VM exit or crash, or
    /// admitted while an earlier lifecycle plan left their target down.
    unavailable: BTreeMap<u32, u64>,
    unavailable_while_down: BTreeSet<u32>,
}

/// Correlate guest-originated requests to responses and applied engine actions.
fn validate_control_history(
    history: &[Value],
    latency: Option<u64>,
    request: &ExecutionRequest<'_>,
    failed: bool,
    trigger_plans: Vec<seedbox_control::Plan>,
) -> Result<ValidatedControlHistory> {
    let expected_latency = control_latency(request)?;
    let latency = latency
        .filter(|n| *n == expected_latency)
        .ok_or_else(|| Error::Integrity("control latency mismatch".into()))?;
    use seedbox_control::{Action, Operation, Plan, Request, Target, PROCESS_EFFECT_TIMEOUT_NS};
    let controller = request
        .control
        .and_then(|control| request.prepared.placement.get(&control.service));
    let resolve = |target: &Target| -> Result<String> {
        let machine = match target {
            Target::Service(service) => request.prepared.placement.get(service).map(String::as_str),
            Target::Machine(machine) => Some(machine.as_str()),
        }
        .ok_or_else(|| Error::Integrity("invalid control target".into()))?;
        if !request.prepared.guests.contains_key(machine)
            || controller.is_some_and(|name| name == machine)
        {
            return Err(Error::Integrity(
                "invalid fault machine/controller target".into(),
            ));
        }
        Ok(machine.to_owned())
    };
    let bad = || Error::Integrity("control request/response/time correlation mismatch".into());
    let mut expected = ValidatedControlHistory::default();
    let mut previous = 0;
    let mut session = 0;
    let mut public_id = 0u64;
    let mut accepted = 0u64;
    let mut next_fault_id = 1u32;
    let mut down = BTreeSet::new();
    for (i, row) in history.iter().enumerate() {
        if row["status"] == "rejected" && failed {
            continue;
        }
        while trigger_plans.iter().any(|plan| plan.id == next_fault_id) {
            next_fault_id = next_fault_id.checked_add(1).ok_or_else(bad)?;
        }
        accepted += 1;
        let r = &row["request"];
        let current_session = match row.get("session_id") {
            Some(value) => value.as_u64().ok_or_else(bad)?,
            None => 1,
        };
        if current_session == 0 || current_session > u32::MAX as u64 || current_session < session {
            return Err(bad());
        }
        public_id = if current_session == session {
            public_id + 1
        } else {
            1
        };
        session = current_session;
        let cutoff = row["status"] == "cut_off";
        let response = row["response"].is_object();
        let metadata = if response { &row["response"] } else { row };
        let t = metadata["requested_at_ns"].as_u64().ok_or_else(bad)?;
        let a = metadata["admitted_at_ns"].as_u64().ok_or_else(bad)?;
        let cut_at = if cutoff {
            Some(row["cut_off_at_ns"].as_u64().ok_or_else(bad)?)
        } else {
            None
        };
        if row["relay_id"].as_u64() != Some(accepted)
            || r["version"] != 1
            || r["id"].as_u64() != Some(public_id)
            || public_id > u32::MAX as u64
            || accepted > u32::MAX as u64
            || t < previous
            || t.checked_add(latency) != Some(a)
            || cut_at.is_some_and(|c| {
                c < t || c > request.limits.simulated_time_ns || i + 1 != history.len()
            })
        {
            return Err(bad());
        }
        let (e, reply) = (
            metadata["effect_at_ns"].as_u64(),
            metadata["replied_at_ns"].as_u64(),
        );
        if e.is_some_and(|e| e < a)
            || reply.is_some_and(|r| e.and_then(|e| e.checked_add(latency)) != Some(r))
        {
            return Err(bad());
        }
        if response {
            let e = e.ok_or_else(bad)?;
            let reply = reply.ok_or_else(bad)?;
            if metadata["version"] != 1
                || metadata["id"] != r["id"]
                || e.checked_add(latency) != Some(reply)
                || (!cutoff && reply >= request.limits.simulated_time_ns)
                || if cut_at.is_some() {
                    !row["delivered_at_ns"].is_null()
                } else {
                    row["delivered_at_ns"].as_u64() != Some(reply) && !failed
                }
            {
                return Err(bad());
            }
            previous = reply;
            if metadata["ok"] != true {
                if metadata["ok"] != false || !metadata["error"].is_object() {
                    return Err(Error::Integrity("invalid control error reply".into()));
                }
                if metadata["error"]["code"] == "invalid_state"
                    && (!row["fault_id"].is_null() || e != a)
                {
                    return Err(bad());
                }
                if row["fault_id"].is_null() || metadata["error"]["code"] != "target_unavailable" {
                    continue;
                }
                if row["fault_id"].as_u64() != Some(u64::from(next_fault_id)) {
                    return Err(bad());
                }
            }
        } else {
            let c = cut_at.ok_or_else(bad)?;
            let admitted = row["admitted_done"].as_bool().ok_or_else(bad)?;
            if (admitted && a > c) || (!admitted && a < c) {
                return Err(bad());
            }
            if row["fault_id"].is_null() {
                if !row["plan"].is_null() && admitted {
                    return Err(bad());
                }
                continue;
            }
            if !admitted || row["fault_id"].as_u64() != Some(u64::from(next_fault_id)) {
                return Err(bad());
            }
        }
        let unavailable = if response {
            metadata["ok"] == false
        } else {
            e.is_some() && row["error"].is_string() && row["fatal"] == false
        };
        let call: Request = serde_json::from_value(r.clone()).map_err(|_| bad())?;
        let action = match call.operation {
            Operation::Stop { target } => Action::Stop {
                machine: resolve(&target)?,
            },
            Operation::Restart { target } => Action::Restart {
                machine: resolve(&target)?,
            },
            Operation::PauseProcess { target } => Action::PauseProcess {
                machine: resolve(&target)?,
            },
            Operation::ResumeProcess { target } => Action::ResumeProcess {
                machine: resolve(&target)?,
            },
            Operation::Pause { target } => Action::Pause {
                machine: resolve(&target)?,
            },
            Operation::Resume { target } => Action::Resume {
                machine: resolve(&target)?,
            },
            Operation::Crash { target } => Action::Crash {
                machine: resolve(&target)?,
            },
            Operation::Reboot { target } => Action::Reboot {
                machine: resolve(&target)?,
            },
            Operation::Partition { left, right } => {
                let handle = if response {
                    &metadata["result"]["handle"]
                } else {
                    &row["plan"]["handle"]
                };
                let handle = handle.as_str().ok_or_else(bad)?.to_owned();
                if handle.is_empty() || (response && e != Some(a)) {
                    return Err(bad());
                }
                Action::Partition {
                    handle,
                    left: left.iter().map(&resolve).collect::<Result<_>>()?,
                    right: right.iter().map(&resolve).collect::<Result<_>>()?,
                }
            }
            Operation::Shape {
                from,
                to,
                latency_ns,
                jitter_ns,
                loss_ppm,
                bandwidth_bps,
                queue_bound_ns,
                network_faults,
            } => {
                let handle = if response {
                    &metadata["result"]["handle"]
                } else {
                    &row["plan"]["handle"]
                };
                let handle = handle.as_str().ok_or_else(bad)?.to_owned();
                if handle.is_empty()
                    || (response && e != Some(a))
                    || latency_ns < request.network.latency_ns
                    || jitter_ns >= latency_ns
                    || latency_ns
                        .checked_add(jitter_ns)
                        .is_none_or(|n| n > 3_600_000_000_000)
                    || loss_ppm > 1_000_000
                    || bandwidth_bps == Some(0)
                    || (queue_bound_ns.is_some() && bandwidth_bps.is_none())
                    || network_faults.validate(latency_ns, jitter_ns).is_err()
                {
                    return Err(bad());
                }
                let from: Vec<_> = from.iter().map(&resolve).collect::<Result<_>>()?;
                let to: Vec<_> = to.iter().map(&resolve).collect::<Result<_>>()?;
                if from.is_empty()
                    || to.is_empty()
                    || from.iter().chain(&to).collect::<BTreeSet<_>>().len()
                        != from.len() + to.len()
                {
                    return Err(bad());
                }
                Action::Shape {
                    handle,
                    from,
                    to,
                    latency_ns,
                    jitter_ns,
                    loss_ppm,
                    bandwidth_bps,
                    queue_bound_ns,
                    network_faults,
                }
            }
            Operation::DropConnection { connection } => {
                let handle = if response {
                    &metadata["result"]["handle"]
                } else {
                    &row["plan"]["handle"]
                };
                let handle = handle.as_str().ok_or_else(bad)?.to_owned();
                if handle.is_empty()
                    || (response && e != Some(a))
                    || connection.incarnation == 0
                    || connection.src_port == 0
                    || connection.dst_port == 0
                    || resolve(&seedbox_control::Target::Machine(connection.from.clone()))?
                        == resolve(&seedbox_control::Target::Machine(connection.to.clone()))?
                {
                    return Err(bad());
                }
                Action::DropConnection { handle, connection }
            }
            Operation::Connections { from, to, .. } => {
                if e != Some(a) || resolve(&from)? == resolve(&to)? {
                    return Err(bad());
                }
                continue;
            }
            Operation::Heal { handle } => {
                if response && e != Some(a) {
                    return Err(bad());
                }
                Action::Heal { handle }
            }
            Operation::Now {} => {
                if e != Some(a) || metadata["result"]["now_ns"].as_u64() != e {
                    return Err(bad());
                }
                continue;
            }
            Operation::Inspect {} => {
                if e != Some(a) {
                    return Err(bad());
                }
                continue;
            }
            Operation::ArmTrigger { .. } | Operation::CancelTrigger { .. } => {
                if e != Some(a) {
                    return Err(bad());
                }
                continue;
            }
            Operation::Sleep { duration_ns } => {
                if a.checked_add(duration_ns) != e {
                    return Err(bad());
                }
                continue;
            }
        };
        let lifecycle = matches!(
            action,
            Action::Pause { .. }
                | Action::Resume { .. }
                | Action::Crash { .. }
                | Action::Reboot { .. }
        );
        if lifecycle && (response || e.is_some()) && e != Some(a) {
            return Err(bad());
        }
        let plan = Plan {
            id: next_fault_id,
            at_ns: a,
            deadline_ns: a.checked_add(PROCESS_EFFECT_TIMEOUT_NS).ok_or_else(bad)?,
            action,
        };
        if !response
            && serde_json::from_value::<Plan>(row["plan"].clone()).map_err(|_| bad())? != plan
        {
            return Err(bad());
        }
        if unavailable {
            if !plan.action.is_process() || e.is_none_or(|e| e > plan.deadline_ns) {
                return Err(bad());
            }
            expected.unavailable.insert(plan.id, e.ok_or_else(bad)?);
            if e == Some(a) && plan.action.machine().is_some_and(|m| down.contains(m)) {
                expected.unavailable_while_down.insert(plan.id);
            }
        } else if response {
            expected.effects.insert(plan.id, e.ok_or_else(bad)?);
        }
        if !unavailable && e == Some(a) {
            match &plan.action {
                Action::Crash { machine } => {
                    down.insert(machine.clone());
                }
                Action::Reboot { machine } => {
                    down.remove(machine);
                }
                _ => {}
            }
        }
        expected.plans.push(plan);
        next_fault_id = next_fault_id.checked_add(1).ok_or_else(bad)?;
    }
    expected.plans.extend(trigger_plans);
    expected.plans.sort_by_key(|p| p.id);
    if expected
        .plans
        .iter()
        .enumerate()
        .any(|(i, p)| p.id as usize != i + 1)
    {
        return Err(bad());
    }
    Ok(expected)
}

/// Check the bounded semantic trigger records against arm/cancel RPCs and
/// derive the sole fault each committed match may schedule. The engine owns
/// commitment; this validator rejects malformed or uncorrelated evidence.
fn validate_triggers(
    records: Option<&[seedbox_control::TriggerRecord]>,
    history: &[Value],
    request: &ExecutionRequest<'_>,
) -> Result<Vec<seedbox_control::Plan>> {
    use seedbox_control::{
        Action, Operation, Plan, Request, Target, TriggerFault, MAX_TRIGGERS, MAX_TRIGGER_MARKER,
        PROCESS_EFFECT_TIMEOUT_NS,
    };
    let bad = || Error::Integrity("invalid committed trigger evidence".into());
    if records.is_some_and(|r| r.is_empty() || r.len() > MAX_TRIGGERS) {
        return Err(bad());
    }
    let records = records.unwrap_or_default();
    let latency = control_latency(request)?;
    let controller = request
        .control
        .and_then(|c| request.prepared.placement.get(&c.service));
    let resolve = |target: &Target, fault: bool| -> Result<String> {
        let name = match target {
            Target::Service(s) => request.prepared.placement.get(s).ok_or_else(bad)?,
            Target::Machine(m) => m,
        };
        if !request.prepared.guests.contains_key(name) || (fault && Some(name) == controller) {
            return Err(bad());
        }
        Ok(name.clone())
    };
    let mut arms = 0;
    let mut cancellations = BTreeMap::new();
    for row in history {
        let metadata = if row["response"].is_object() {
            &row["response"]
        } else {
            row
        };
        let accepted = if row["response"].is_object() {
            metadata["ok"] == true
        } else {
            row["admitted_done"] == true && row["error"].is_null()
        };
        if !accepted {
            continue;
        }
        let op = row["request"]["op"].as_str().unwrap_or_default();
        if op != "arm_trigger" && op != "cancel_trigger" {
            continue;
        }
        let call: Request = serde_json::from_value(row["request"].clone()).map_err(|_| bad())?;
        let at = metadata["admitted_at_ns"].as_u64().ok_or_else(bad)?;
        match call.operation {
            Operation::CancelTrigger { trigger_id } => {
                if trigger_id == 0
                    || trigger_id as usize > arms
                    || cancellations.insert(trigger_id, at).is_some()
                {
                    return Err(bad());
                }
            }
            Operation::ArmTrigger {
                source,
                marker,
                match_timeout_ns,
                fault,
            } => {
                let r = records.get(arms).ok_or_else(bad)?;
                arms += 1;
                if r.id as usize != arms
                    || metadata["result"]["trigger_id"].as_u64() != Some(u64::from(r.id))
                    || r.source != resolve(&source, false)?
                    || r.marker != marker
                    || marker.is_empty()
                    || marker.len() > MAX_TRIGGER_MARKER
                    || match_timeout_ns == 0
                    || r.armed_at_ns != at
                    || at.checked_add(match_timeout_ns) != Some(r.match_deadline_ns)
                {
                    return Err(bad());
                }
                let action = match fault {
                    TriggerFault::Stop { target } => Action::Stop {
                        machine: resolve(&target, true)?,
                    },
                    TriggerFault::Partition { left, right } => {
                        let handle = metadata["result"]["handle"]
                            .as_str()
                            .filter(|h| !h.is_empty())
                            .ok_or_else(bad)?
                            .to_owned();
                        let left = left
                            .iter()
                            .map(|t| resolve(t, true))
                            .collect::<Result<Vec<_>>>()?;
                        let right = right
                            .iter()
                            .map(|t| resolve(t, true))
                            .collect::<Result<Vec<_>>>()?;
                        let unique: BTreeSet<_> = left.iter().chain(&right).collect();
                        if left.is_empty()
                            || right.is_empty()
                            || unique.len() != left.len() + right.len()
                        {
                            return Err(bad());
                        }
                        Action::Partition {
                            handle,
                            left,
                            right,
                        }
                    }
                };
                if r.action != action {
                    return Err(bad());
                }
            }
            _ => return Err(bad()),
        }
    }
    if arms != records.len() {
        return Err(bad());
    }
    let mut plans = Vec::new();
    for r in records {
        let matched = r.matched_at_ns.is_some();
        if r.prefix > r.marker.len()
            || r.seen < r.prefix as u64
            || r.match_deadline_ns
                .checked_add(latency)
                .and_then(|d| d.checked_add(PROCESS_EFFECT_TIMEOUT_NS))
                .is_none()
            || r.effect_at_ns.is_some() != matched
            || (matched && r.prefix != r.marker.len())
            || (!matched && r.prefix == r.marker.len())
            || r.cancelled_at_ns != cancellations.get(&r.id).copied()
            || (r.status != "rejected" && r.rejection_reason.is_some())
        {
            return Err(bad());
        }
        if let Some(m) = r.matched_at_ns {
            if m < r.armed_at_ns
                || m >= r.match_deadline_ns
                || m.checked_add(latency) != r.effect_at_ns
            {
                return Err(bad());
            }
        }
        if r.cancelled_at_ns.is_some_and(|c| {
            c < r.armed_at_ns || c > r.match_deadline_ns || c >= request.limits.simulated_time_ns
        }) || r
            .cut_off_at_ns
            .is_some_and(|c| c < r.armed_at_ns || c > request.limits.simulated_time_ns)
        {
            return Err(bad());
        }
        match r.status.as_str() {
            "rejected"
                if matched
                    && r.cancelled_at_ns.is_none()
                    && r.cut_off_at_ns.is_none()
                    && r.fault_id.is_none()
                    && r.effect_at_ns
                        .is_some_and(|e| e < request.limits.simulated_time_ns)
                    && r.rejection_reason
                        .as_ref()
                        .is_some_and(|reason| !reason.is_empty()) => {}
            "fired" if matched && r.cancelled_at_ns.is_none() && r.cut_off_at_ns.is_none() => {
                let at_ns = r.effect_at_ns.ok_or_else(bad)?;
                if at_ns >= request.limits.simulated_time_ns {
                    return Err(bad());
                }
                plans.push(Plan {
                    id: r.fault_id.ok_or_else(bad)?,
                    at_ns,
                    deadline_ns: at_ns
                        .checked_add(PROCESS_EFFECT_TIMEOUT_NS)
                        .ok_or_else(bad)?,
                    action: r.action.clone(),
                });
            }
            "expired"
                if !matched
                    && r.cancelled_at_ns.is_none()
                    && r.cut_off_at_ns.is_none()
                    && r.match_deadline_ns < request.limits.simulated_time_ns
                    && r.fault_id.is_none() => {}
            "cancelled"
                if !matched
                    && r.cancelled_at_ns.is_some()
                    && r.cut_off_at_ns.is_none()
                    && r.fault_id.is_none() => {}
            "cut_off"
                if r.cancelled_at_ns.is_none()
                    && r.fault_id.is_none()
                    && r.cut_off_at_ns.is_some_and(|c| {
                        if matched {
                            r.effect_at_ns.is_some_and(|e| e >= c)
                        } else {
                            c <= r.match_deadline_ns
                        }
                    }) => {}
            _ => return Err(bad()),
        }
    }
    plans.sort_by_key(|p| p.at_ns);
    if plans.windows(2).any(|p| p[0].id >= p[1].id) {
        return Err(bad());
    }
    Ok(plans)
}

/// Parse the engine's structured completion report. Never infer application
/// exit status from log text or the VMM's infrastructure sentinel exit codes.
pub fn report(bytes: &[u8], request: &ExecutionRequest<'_>) -> Result<ExecutionReport> {
    let raw: VmmReport = serde_json::from_slice(bytes)?;
    if raw.report_version != 1 || raw.seed != request.seed {
        return Err(Error::Integrity("VMM report version/seed mismatch".into()));
    }
    let expected_checkpoint = if recorded_checkpoint(&request.prepared.profile)?.is_some() {
        world(request)?.get("checkpoint").cloned()
    } else {
        None
    };
    if raw.checkpoint != expected_checkpoint {
        return Err(Error::Integrity("VMM checkpoint identity mismatch".into()));
    }
    if raw.deferred_tx != recorded_deferred_tx(&request.prepared.profile)? {
        return Err(Error::Integrity(
            "VMM deferred_tx does not match recorded --deferred-tx".into(),
        ));
    }
    let timebase = recorded_timebase(&request.prepared.profile)?;
    let reported = raw.timebase.as_deref().unwrap_or("counter");
    if reported != timebase.name() {
        return Err(Error::Integrity(format!(
            "VMM --timebase {reported}: does not match recorded --timebase {}",
            timebase.name()
        )));
    }
    let counted = recorded_counter(&request.prepared.profile)?;
    if timebase == Timebase::Counter
        && counted != CounterProfile::Zen
        && raw.counter_profile.is_none()
    {
        return Err(Error::Integrity(
            "VMM report is missing its counter profile".into(),
        ));
    }
    let expected = counted.name();
    if raw
        .counter_profile
        .as_deref()
        .is_some_and(|name| name != expected)
    {
        return Err(Error::Integrity(format!(
            "VMM counted with a different branch counter profile than the recorded {expected}"
        )));
    }
    checked_report(raw, request)
}

/// The ARM adapter validates its own version, boot layout and PV identity
/// before sharing outcome/control correlation checks with the existing engine.
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
pub(crate) fn report_hvf(bytes: &[u8], request: &ExecutionRequest<'_>) -> Result<ExecutionReport> {
    let raw: VmmReport = serde_json::from_slice(bytes)?;
    if raw.report_version != 2 || raw.seed != request.seed || raw.counter_profile.is_some() {
        return Err(Error::Integrity(
            "HVF application report version/seed/timebase mismatch".into(),
        ));
    }
    crate::mac::validate_report(bytes, request)?;
    checked_report(raw, request)
}

fn checked_report(raw: VmmReport, request: &ExecutionRequest<'_>) -> Result<ExecutionReport> {
    let timebase = if crate::engine::recorded_launch(&request.prepared.profile)?
        == crate::engine::ExecutableKind::ArmWorld
    {
        Timebase::Pv
    } else {
        recorded_timebase(&request.prepared.profile)?
    };
    let names: BTreeSet<_> = raw.machines.iter().map(|machine| &machine.name).collect();
    if names.len() != raw.machines.len() || names != request.prepared.guests.keys().collect() {
        return Err(Error::Integrity("VMM report machine set mismatch".into()));
    }
    let workload = request
        .prepared
        .placement
        .get(&request.check.service)
        .ok_or_else(|| Error::Integrity("missing workload placement".into()))?;
    let workload_exit = raw
        .machines
        .iter()
        .find(|machine| &machine.name == workload)
        .and_then(|machine| machine.guest_exit_code)
        .map(|code| WorkloadExit {
            service: request.check.service.clone(),
            code,
        });
    let infrastructure_error = raw
        .machines
        .iter()
        .find(|machine| machine.guest_init_error.is_some() || machine.infrastructure_error);
    let mut reason = if infrastructure_error.is_some() {
        StopReason::Error
    } else {
        match raw.outcome.as_str() {
            "end_machine_exited" => StopReason::Completed,
            "duration" => StopReason::SimulatedTimeLimit,
            "quiescent" => StopReason::Quiescent,
            "fault_failed" => StopReason::Error,
            other => return Err(Error::Integrity(format!("unknown VMM outcome {other}"))),
        }
    };
    let full_state = match request.prepared.profile.witness_mode {
        WitnessMode::Exits => None,
        WitnessMode::FullState => Some(
            raw.machines
                .iter()
                .map(|machine| {
                    Ok((
                        machine.name.clone(),
                        machine.state_digest.clone().ok_or_else(|| {
                            Error::Integrity("missing machine state digest".into())
                        })?,
                    ))
                })
                .collect::<Result<BTreeMap<_, _>>>()?,
        ),
    };
    if request.control.is_none()
        && (!raw.control_history.is_empty()
            || raw.control_latency_ns.is_some_and(|n| n != 0)
            || raw.triggers.is_some())
    {
        return Err(Error::Integrity("unexpected control transcript".into()));
    }
    let expected = if request.control.is_some() {
        validate_control_history(
            &raw.control_history,
            raw.control_latency_ns,
            request,
            raw.fault_error.is_some(),
            validate_triggers(raw.triggers.as_deref(), &raw.control_history, request)?,
        )?
    } else {
        ValidatedControlHistory::default()
    };
    let mut faults = Vec::new();
    let mut fault_failure = raw.fault_error;
    if raw.faults.len() != expected.plans.len()
        && !(request.control.is_some() && fault_failure.is_some())
    {
        fault_failure = Some("fault request history is incomplete".to_string());
    }
    for (i, fault) in raw.faults.iter().enumerate() {
        let plan = expected
            .plans
            .get(i)
            .or_else(|| {
                (request.control.is_some() && fault_failure.is_some()).then_some(&fault.request)
            })
            .ok_or_else(|| Error::Integrity("unexpected fault acknowledgement".into()))?;
        if fault.request != *plan
            || fault.id as usize != i + 1
            || fault.requested_at_ns != plan.at_ns
            || fault.action != plan.action.name()
            || fault.machine.as_deref() != plan.action.machine()
        {
            return Err(Error::Integrity(
                "fault acknowledgement correlation mismatch".into(),
            ));
        }
        match (fault.status.as_str(), fault.applied_at_ns) {
            // Triggered stops have no RPC reply; unavailable effects retain
            // the same exit/crash window and prior-down evidence as process RPCs.
            ("unavailable", None)
                if raw.triggers.as_ref().is_some_and(|records| {
                    records.iter().any(|r| r.fault_id == Some(fault.id))
                }) && matches!(plan.action, seedbox_control::Action::Stop { .. })
                    && (raw.machines.iter().any(|m| {
                        Some(&m.name) == fault.machine.as_ref()
                            && ((m.state.as_deref().is_some_and(|s| {
                                s.strip_prefix("Exited(")
                                    .and_then(|c| c.strip_suffix(')'))
                                    .and_then(|c| c.parse::<u8>().ok())
                                    .is_some_and(|c| s == format!("Exited({c})"))
                            }) && m
                                .time_ns
                                .is_some_and(|t| t >= plan.at_ns && t <= plan.deadline_ns))
                                || m.crashed_incarnations.iter().any(|incarnation| {
                                    incarnation.crashed_at_ns >= plan.at_ns
                                        && incarnation.crashed_at_ns <= plan.deadline_ns
                                }))
                    }) || expected
                        .plans
                        .iter()
                        .take(i)
                        .rev()
                        .find(|prior| {
                            prior.action.machine() == plan.action.machine()
                                && matches!(
                                    prior.action,
                                    seedbox_control::Action::Crash { .. }
                                        | seedbox_control::Action::Reboot { .. }
                                )
                        })
                        .is_some_and(|prior| {
                            matches!(prior.action, seedbox_control::Action::Crash { .. })
                                && prior.at_ns <= plan.at_ns
                                && raw.faults.get(prior.id as usize - 1).is_some_and(|fault| {
                                    fault.status == "applied"
                                        && fault.applied_at_ns == Some(prior.at_ns)
                                })
                        })) => {}
            // An unavailable process fault needs a prior crash without reboot
            // at admission, or the target's exact exit/crash time in its window.
            ("unavailable", None)
                if expected.unavailable.get(&fault.id).is_some_and(|exit| {
                    expected.unavailable_while_down.contains(&fault.id)
                        || raw.machines.iter().any(|machine| {
                            Some(&machine.name) == fault.machine.as_ref()
                                && ((machine.state.as_deref().is_some_and(|state| {
                                    state
                                        .strip_prefix("Exited(")
                                        .and_then(|code| code.strip_suffix(')'))
                                        .and_then(|code| code.parse::<u8>().ok())
                                        .is_some_and(|code| state == format!("Exited({code})"))
                                }) && machine.time_ns == Some(*exit))
                                    || machine.crashed_incarnations.iter().any(|incarnation| {
                                        incarnation.crashed_at_ns == *exit
                                            && *exit >= plan.at_ns
                                            && *exit <= plan.deadline_ns
                                    }))
                        })
                }) => {}
            _ if expected.unavailable.contains_key(&fault.id) => {
                return Err(Error::Integrity("invalid fault effect status/time".into()))
            }
            ("applied", Some(at))
                if at >= fault.requested_at_ns
                    && at <= plan.deadline_ns
                    && (request.control.is_none()
                        || !matches!(
                            plan.action,
                            seedbox_control::Action::Pause { .. }
                                | seedbox_control::Action::Resume { .. }
                                | seedbox_control::Action::Crash { .. }
                                | seedbox_control::Action::Reboot { .. }
                        )
                        || at == plan.at_ns)
                    && (request.control.is_none()
                        || expected.effects.get(&fault.id).is_none_or(|e| *e == at)) =>
            {
                let service = fault
                    .machine
                    .as_ref()
                    .and_then(|machine| {
                        request
                            .prepared
                            .placement
                            .iter()
                            .find(|(_, m)| *m == machine)
                            .map(|(s, _)| s.clone())
                    })
                    .unwrap_or_default();
                faults.push(crate::execution::FaultEvent {
                    requested_at_ns: fault.requested_at_ns,
                    applied_at_ns: at,
                    service,
                    action: fault.action.clone(),
                    detail: format!("id={} {}", fault.id, fault.detail),
                });
            }
            ("requested" | "unrequested", None) if !expected.effects.contains_key(&fault.id) => {}
            ("failed", None) => {
                fault_failure = Some(format!(
                    "fault {} {}: {}",
                    fault.id, fault.status, fault.detail
                ))
            }
            _ => return Err(Error::Integrity("invalid fault effect status/time".into())),
        }
    }
    if fault_failure.is_some() {
        reason = StopReason::Error;
    }
    Ok(ExecutionReport {
        timebase: Some(timebase),
        reason,
        workload_exit,
        witness: Some(raw.world_witness),
        full_state,
        faults,
        engine_qualification: None,
        detail: fault_failure.or_else(|| {
            infrastructure_error
                .map(|machine| format!("machine {} failed in guest bootstrap or VMM", machine.name))
        }),
    })
}

#[cfg(target_os = "linux")]
mod linux {
    use super::*;
    use crate::host::HostLease;
    use std::{fs, os::unix::process::CommandExt};

    pub(super) fn execute(
        backend: &VmmExecutor,
        request: ExecutionRequest<'_>,
    ) -> Result<ExecutionReport> {
        backend.validate_retained(request.prepared)?;
        backend.preflight(Some(request.output))?;
        if request.prepared.profile != backend.profile {
            return Err(Error::Integrity("execution profile mismatch".into()));
        }
        if backend
            .cpu
            .is_some_and(|cpu| cpu >= libc::CPU_SETSIZE as u32)
        {
            return Err(Error::Execution("CPU exceeds affinity mask".into()));
        }
        let timebase = recorded_timebase(&backend.profile)?;
        use crate::worker_process::{incomplete, WorkerLaunch};
        let mut launch = WorkerLaunch::prepare(&request)?;
        // Performance values are explicit; hidden process settings stay cleared.
        launch.command.envs(request.performance_parameters);

        if request.cancellation.is_cancelled() {
            return Ok(incomplete(
                StopReason::Cancelled,
                recorded_timebase(&backend.profile)?,
            ));
        }
        let static_evidence = backend
            .admission
            .lock()
            .map_err(|_| Error::Execution("host admission evidence lock poisoned".into()))?
            .clone();
        let counted = timebase == Timebase::Counter;
        let workaround = backend.counter.workaround().filter(|_| counted);
        if counted {
            fs::write(
                request.output.join("host-static.json"),
                serde_json::to_vec_pretty(&static_evidence)?,
            )?;
        }
        let (cpu, mut lease) =
            HostLease::acquire_checked(backend.cpu, workaround, request.output, |reservation| {
                if !counted {
                    return Ok(());
                }
                let mut observed = Vec::new();
                let admission = crate::admission::collect_admitted(
                    reservation.cpus.iter().copied(),
                    &mut observed,
                    |cpu| crate::admission::observe_counter(cpu, backend.counter),
                );
                crate::admission::finish_admission(admission, |admission| {
                    fs::write(
                        request.output.join("host-leased.json"),
                        serde_json::to_vec_pretty(&observed)?,
                    )?;
                    if let Err(error) = admission {
                        fs::write(
                            request.output.join("host-leased-error.txt"),
                            error.to_string(),
                        )?;
                    }
                    Ok(())
                })
            })?;
        let originals = lease.original_registers();
        if workaround.is_some() {
            fs::write(
                request.output.join("host-workaround.json"),
                serde_json::to_vec_pretty(&lease.enabled_registers()?)?,
            )?;
        }
        unsafe {
            launch.command.pre_exec(move || {
                let mut set: libc::cpu_set_t = std::mem::zeroed();
                libc::CPU_ZERO(&mut set);
                libc::CPU_SET(cpu as usize, &mut set);
                if libc::sched_setaffinity(0, std::mem::size_of_val(&set), &set) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        // Declared after the lease: every error path finishes process cleanup
        // before releasing the leased CPU or restoring MSRs.
        let child = launch.spawn()?;
        let completion = child.wait(request.cancellation, request.limits.wall_time_ns, |bytes| {
            report(bytes, &request)
        });
        lease.restore()?;
        if workaround.is_some() {
            fs::write(
                request.output.join("host-restored.json"),
                serde_json::to_vec_pretty(&originals)?,
            )?;
        }
        completion
    }
}

#[cfg(test)]
mod doctor_tests {
    use super::*;
    use crate::admission::{required_policy, CoreObservation};

    #[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
    #[test]
    fn internal_world_refusal_precedes_retention_or_input_reading() {
        let temporary = tempfile::tempdir().unwrap();
        let store_path = temporary.path().join("objects");
        let store = ContentStore::new(&store_path);
        let missing = temporary.path().join("missing");
        let expected = "built-in Compose world execution requires Linux x86-64; ARM world integration is not available yet";
        let result = VmmExecutor::new_internal(
            &missing,
            &store,
            None,
            512,
            WitnessMode::Exits,
            CounterProfile::Zen,
        );
        assert_eq!(result.unwrap_err().to_string(), expected);
        assert!(!store_path.exists());
        assert_eq!(
            VmmExecutor::doctor_internal(&missing, &missing, None)
                .unwrap_err()
                .to_string(),
            expected
        );
        assert!(!store_path.exists());
    }

    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    #[test]
    fn internal_doctor_reports_simulator_version_without_new_qualification() {
        let temporary = tempfile::tempdir().unwrap();
        let executable = temporary.path().join("worker");
        std::fs::write(&executable, b"combined executable").unwrap();
        let store = ContentStore::new(temporary.path().join("objects"));
        let kernel = ContentDigest::of(b"kernel");
        let executor = VmmExecutor::new_internal(
            &executable,
            &store,
            None,
            512,
            WitnessMode::Exits,
            CounterProfile::Zen,
        )
        .unwrap();
        let report = executor
            .doctor_report(
                kernel,
                AdmissionProbe {
                    cpu: Some(8),
                    observations: vec![observed(8)],
                    result: Ok(()),
                },
            )
            .unwrap();
        assert_eq!(
            report.simulator_version.as_deref(),
            Some(crate::engine::SIMULATOR_VERSION)
        );
        assert!(serde_json::to_value(report)
            .unwrap()
            .get("engine_qualification")
            .is_none());
    }

    #[test]
    fn replay_preserves_historical_policy_data_without_current_defaults() {
        let temporary = tempfile::tempdir().unwrap();
        let executable = temporary.path().join("retained-worker");
        std::fs::write(&executable, b"retained executable").unwrap();
        let store = ContentStore::new(temporary.path().join("objects"));
        let mut profile = VmmExecutor::from_engine_digest(
            ContentDigest::of(b"retained executable"),
            None,
            512,
            WitnessMode::Exits,
            CounterProfile::Zen,
        )
        .unwrap()
        .profile;
        profile.semantic_parameters.remove("host_policy");
        profile.semantic_parameters.insert(
            crate::admission::ENGINE_QUALIFICATION.into(),
            "dev-unpinned".into(),
        );
        let before = serde_json::to_vec(&profile).unwrap();
        let executor =
            VmmExecutor::from_retained(&executable, &store, profile, None, CounterProfile::Zen)
                .unwrap();
        assert_eq!(serde_json::to_vec(&executor.profile).unwrap(), before);
        crate::admission::validate_profile(&executor.profile).unwrap();
        assert_eq!(
            crate::engine::recorded_launch(&executor.profile).unwrap(),
            crate::engine::ExecutableKind::Standalone
        );
    }

    #[test]
    fn explicit_override_uses_replacement_contract_in_both_directions() {
        let _lock = crate::preparation_command::PROCESS_TEST_LOCK
            .lock()
            .unwrap();
        let directory = tempfile::tempdir().unwrap();
        let combined = directory.path().join("combined");
        let standalone = directory.path().join("standalone");
        std::fs::write(
            &combined,
            b"#!/bin/sh\nprintf 'seedbox 0.2.0-replacement\\n'\n",
        )
        .unwrap();
        std::fs::write(&standalone, b"#!/bin/sh\nexit 2\n").unwrap();
        let store = ContentStore::new(directory.path().join("store"));
        let original = VmmExecutor::new(
            &standalone,
            &store,
            None,
            512,
            WitnessMode::Exits,
            CounterProfile::Zen,
        )
        .unwrap()
        .with_timebase(Timebase::Pv)
        .with_page_timer(true)
        .with_deferred_tx(true)
        .with_torn_writes(true);
        let combined = VmmExecutor::from_override(
            &combined,
            &store,
            original.profile.clone(),
            None,
            CounterProfile::Zen,
        )
        .unwrap();
        assert_eq!(
            combined.profile.semantic_parameters["simulator_version"],
            "0.2.0-replacement"
        );
        assert_eq!(
            engine_command(Path::new("engine"), &combined.profile)
                .unwrap()
                .get_args()
                .collect::<Vec<_>>(),
            [
                "__engine",
                crate::engine::INTERNAL_WORKER_V1,
                "--timebase",
                "pv",
                "--page-timer",
                "--deferred-tx"
            ]
        );
        assert!(recorded_torn_writes(&combined.profile).unwrap());
        let standalone = VmmExecutor::from_override(
            &standalone,
            &store,
            combined.profile,
            None,
            CounterProfile::Zen,
        )
        .unwrap();
        assert_eq!(standalone.profile, original.profile);
        assert_eq!(
            engine_command(Path::new("engine"), &standalone.profile)
                .unwrap()
                .get_args()
                .collect::<Vec<_>>(),
            ["--timebase", "pv", "--page-timer", "--deferred-tx"]
        );
        // Explicit fresh runs perform the same detection; retained replay does not.
        let detected = VmmExecutor::new_detected(
            &directory.path().join("combined"),
            &store,
            None,
            512,
            WitnessMode::Exits,
            CounterProfile::Zen,
        )
        .unwrap();
        assert_eq!(
            detected.profile.semantic_parameters["engine_executable_kind"],
            crate::engine::INTERNAL_WORKER_V1
        );
    }

    #[test]
    fn retained_worker_launch_preserves_timer_and_deferred_tx_options() {
        let executor = VmmExecutor::from_engine_digest(
            ContentDigest::of(b"engine"),
            None,
            512,
            WitnessMode::Exits,
            CounterProfile::Zen,
        )
        .unwrap()
        .with_timebase(Timebase::Pv)
        .with_page_timer(true)
        .with_deferred_tx(true);
        for internal in [false, true] {
            let mut profile = executor.profile.clone();
            if internal {
                profile.semantic_parameters.extend(BTreeMap::from([
                    (
                        "engine_executable_kind".into(),
                        crate::engine::INTERNAL_WORKER_V1.into(),
                    ),
                    (
                        "simulator_version".into(),
                        crate::engine::SIMULATOR_VERSION.into(),
                    ),
                    ("host_os".into(), "linux".into()),
                    ("host_arch".into(), "x86_64".into()),
                    ("guest_arch".into(), "x86_64".into()),
                    ("engine_backend".into(), "kvm".into()),
                    ("engine_timebase".into(), "counter-pv-v1".into()),
                ]));
            }
            let retained: ExecutionProfile =
                serde_json::from_slice(&serde_json::to_vec(&profile).unwrap()).unwrap();
            let command = engine_command(Path::new("retained-engine"), &retained).unwrap();
            let mut expected = Vec::new();
            if internal {
                expected.extend(["__engine", crate::engine::INTERNAL_WORKER_V1]);
            }
            expected.extend(["--timebase", "pv", "--page-timer", "--deferred-tx"]);
            assert_eq!(command.get_args().collect::<Vec<_>>(), expected);
            profile
                .semantic_parameters
                .insert("deferred_tx".into(), "false".into());
            assert!(engine_command(Path::new("retained-engine"), &profile).is_err());
        }
    }

    #[test]
    fn counter_and_pv_retain_identity_and_replay_through_worker_contract() {
        use crate::engine::{recorded_launch, ExecutableKind, INTERNAL_WORKER_V1};
        let temporary = tempfile::tempdir().unwrap();
        let executable = temporary.path().join("retained-worker");
        std::fs::write(&executable, b"combined executable").unwrap();
        let store = ContentStore::new(temporary.path().join("objects"));
        let mut profiles = Vec::new();
        for timebase in [Timebase::Counter, Timebase::Pv] {
            #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
            let executor = VmmExecutor::new_internal(
                &executable,
                &store,
                None,
                512,
                WitnessMode::Exits,
                CounterProfile::Zen,
            )
            .unwrap();
            // A retained Linux profile is inspectable on a Mac without host admission.
            #[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
            let executor = {
                let mut executor = VmmExecutor::new(
                    &executable,
                    &store,
                    None,
                    512,
                    WitnessMode::Exits,
                    CounterProfile::Zen,
                )
                .unwrap();
                executor.profile.semantic_parameters.extend(BTreeMap::from([
                    ("engine_executable_kind".into(), INTERNAL_WORKER_V1.into()),
                    (
                        "simulator_version".into(),
                        crate::engine::SIMULATOR_VERSION.into(),
                    ),
                    ("host_os".into(), "linux".into()),
                    ("host_arch".into(), "x86_64".into()),
                    ("guest_arch".into(), "x86_64".into()),
                    ("engine_backend".into(), "kvm".into()),
                    ("engine_timebase".into(), "counter-pv-v1".into()),
                ]));
                executor
            };
            let executor = executor.with_timebase(timebase);
            assert_eq!(
                recorded_launch(&executor.profile).unwrap(),
                ExecutableKind::InternalWorker
            );
            assert_eq!(recorded_timebase(&executor.profile).unwrap(), timebase);
            assert_eq!(
                executor
                    .profile
                    .semantic_parameters
                    .contains_key("timebase"),
                timebase == Timebase::Pv
            );
            let bytes = serde_json::to_vec(&executor.profile).unwrap();
            let retained: ExecutionProfile = serde_json::from_slice(&bytes).unwrap();
            let replay = VmmExecutor::from_retained(
                &executable,
                &store,
                retained,
                Some(8),
                CounterProfile::Zen,
            )
            .unwrap();
            assert_eq!(serde_json::to_vec(&replay.profile).unwrap(), bytes);
            let expected = if timebase == Timebase::Pv {
                vec!["__engine", INTERNAL_WORKER_V1, "--timebase", "pv"]
            } else {
                vec!["__engine", INTERNAL_WORKER_V1]
            };
            for profile in [&executor.profile, &replay.profile] {
                let command = engine_command(&executable, profile).unwrap();
                assert_eq!(command.get_program(), executable.as_os_str());
                assert_eq!(command.get_args().collect::<Vec<_>>(), expected);
                #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
                {
                    let mut args = vec!["seedbox".to_owned()];
                    args.extend(
                        command
                            .get_args()
                            .skip(2)
                            .map(|arg| arg.to_str().unwrap().to_owned()),
                    );
                    args.extend(
                        ["--config", "world.json", "--seed", "7", "--out", "output"]
                            .map(str::to_owned),
                    );
                    let options = seedbox_vmm::WorkerOptions::from_args(
                        &args,
                        seedbox_vmm::PerformanceOptions::default(),
                    )
                    .unwrap();
                    assert_eq!(options.pv_clock, timebase == Timebase::Pv);
                }
            }
            profiles.push(executor.profile);
        }
        let mut pv_as_counter = profiles.pop().unwrap();
        pv_as_counter.semantic_parameters.remove("timebase");
        assert_eq!(pv_as_counter, profiles[0]);
        // #89 owns timebase even for historical standalone profiles.
        let legacy = VmmExecutor::new(
            &executable,
            &store,
            None,
            512,
            WitnessMode::Exits,
            CounterProfile::Zen,
        )
        .unwrap();
        for timebase in [Timebase::Counter, Timebase::Pv] {
            let executor = legacy.clone().with_timebase(timebase);
            assert_eq!(
                recorded_launch(&executor.profile).unwrap(),
                ExecutableKind::Standalone
            );
        }
    }

    #[test]
    fn recorded_descriptor_is_data_and_unknown_timebases_still_fail() {
        let temporary = tempfile::tempdir().unwrap();
        let executable = temporary.path().join("old-worker");
        std::fs::write(&executable, b"retained worker").unwrap();
        let store = ContentStore::new(temporary.path().join("objects"));
        let mut profile = VmmExecutor::new(
            &executable,
            &store,
            None,
            512,
            WitnessMode::Exits,
            CounterProfile::Zen,
        )
        .unwrap()
        .profile;
        profile.semantic_parameters.extend(BTreeMap::from([
            (
                "engine_executable_kind".into(),
                crate::engine::INTERNAL_WORKER_V1.into(),
            ),
            ("simulator_version".into(), "0.1.0-dev.1".into()),
            ("host_os".into(), "linux".into()),
            ("host_arch".into(), "x86_64".into()),
            ("guest_arch".into(), "x86_64".into()),
            ("engine_backend".into(), "kvm".into()),
            ("timebase".into(), "counted-branches-v1".into()),
        ]));
        let original = serde_json::to_vec(&profile).unwrap();
        let executor = VmmExecutor::from_retained(
            &executable,
            &store,
            profile.clone(),
            None,
            CounterProfile::Zen,
        )
        .unwrap();
        assert_eq!(serde_json::to_vec(&executor.profile).unwrap(), original);
        assert_eq!(
            crate::engine::recorded_launch(&executor.profile).unwrap(),
            crate::engine::ExecutableKind::InternalWorker
        );
        assert!(recorded_timebase(&executor.profile).is_err());
    }

    #[test]
    fn replay_override_changes_only_retained_executable_digest() {
        let temporary = tempfile::tempdir().unwrap();
        let executable = temporary.path().join("replacement-worker");
        std::fs::write(&executable, b"replacement executable").unwrap();
        let store = ContentStore::new(temporary.path().join("objects"));
        let profile = VmmExecutor::from_engine_digest(
            ContentDigest::of(b"original executable"),
            None,
            512,
            WitnessMode::Exits,
            CounterProfile::Zen,
        )
        .unwrap()
        .profile;
        let mut expected = profile.clone();
        expected.engine = ContentDigest::of(b"replacement executable");
        let executor =
            VmmExecutor::from_retained(&executable, &store, profile, None, CounterProfile::Zen)
                .unwrap();
        assert_eq!(executor.profile, expected);
    }

    #[test]
    fn capacity_admission_holds_all_reservations_and_releases_them_on_every_exit() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        struct Reservation<'a>(&'a AtomicUsize);
        impl Drop for Reservation<'_> {
            fn drop(&mut self) {
                self.0.fetch_sub(1, Ordering::SeqCst);
            }
        }
        for (available, refuse) in [(3, false), (2, false), (3, true)] {
            // An unrelated owner must survive both capacity and admission errors.
            let held = AtomicUsize::new(1);
            let mut checked = false;
            let result = with_capacity(
                3,
                || {
                    if held.load(Ordering::SeqCst) > available {
                        return Err(Error::Setup("fixture capacity unavailable".into()));
                    }
                    held.fetch_add(1, Ordering::SeqCst);
                    Ok(Reservation(&held))
                },
                |reservations| {
                    checked = true;
                    assert_eq!(reservations.len(), 3);
                    assert_eq!(held.load(Ordering::SeqCst), 4);
                    if refuse {
                        Err(Error::Setup("fixture core admission refused".into()))
                    } else {
                        Ok(())
                    }
                },
            );
            assert_eq!(held.load(Ordering::SeqCst), 1);
            assert_eq!(checked, available == 3);
            assert_eq!(result.is_ok(), available == 3 && !refuse);
            if available == 2 {
                assert!(result
                    .unwrap_err()
                    .to_string()
                    .contains("--workers 3: cannot reserve 3 distinct eligible physical cores"));
            }
        }
    }

    #[test]
    fn parallel_preflight_preserves_the_experimental_admission_gate() {
        let pinned = VmmExecutor::from_engine_digest(
            ContentDigest::of(b"engine"),
            None,
            512,
            WitnessMode::Exits,
            CounterProfile::Zen,
        )
        .unwrap();
        for workers in [0, 33] {
            assert!(pinned
                .preflight_workers(workers)
                .unwrap_err()
                .to_string()
                .contains("--workers: use 1..32"));
        }
    }

    fn observed(cpu: u32) -> CoreObservation {
        let p = required_policy();
        CoreObservation {
            cpu,
            architecture: p.architecture,
            vendor: "AuthenticAMD".into(),
            family: 25,
            model: 33,
            stepping: 0,
            microcode: 0,
            kvm_api: p.kvm_api,
            immediate_exit: p.immediate_exit,
            guest_debug: p.guest_debug,
            sync_regs: 7,
            perf_type: p.perf_type,
            perf_disabled_open: true,
            perf_error: None,
            workaround_msr: 17,
            raw_cpuid: Vec::new(),
        }
    }
    #[test]
    fn replay_reconstructs_checkpoint_profile_without_performance_changes() {
        let ordinary = VmmExecutor::from_engine_digest(
            ContentDigest::of(b"engine"),
            None,
            512,
            WitnessMode::Exits,
            CounterProfile::Zen,
        )
        .unwrap();
        let branch = ordinary.clone().with_checkpoint(500, 42);
        assert_eq!(
            branch.performance_parameters(),
            ordinary.performance_parameters()
        );
        assert_eq!(
            ordinary
                .clone()
                .with_recorded_checkpoint(&branch.profile())
                .unwrap()
                .profile(),
            branch.profile()
        );
        assert_eq!(
            ordinary
                .clone()
                .with_recorded_checkpoint(&ordinary.profile())
                .unwrap()
                .profile(),
            ordinary.profile()
        );
    }

    #[test]
    fn any_engine_and_kernel_pair_is_admitted() {
        let pinned_engine = ContentDigest::of(b"engine");
        let pinned_kernel = ContentDigest::of(b"kernel");
        for (engine, kernel) in [
            (pinned_engine, pinned_kernel),
            (
                ContentDigest::of(b"dev engine"),
                ContentDigest::of(b"kernel"),
            ),
            (
                ContentDigest::of(b"dev engine"),
                ContentDigest::of(b"dev kernel"),
            ),
        ] {
            let backend = VmmExecutor::from_engine_digest(
                engine.clone(),
                None,
                512,
                WitnessMode::Exits,
                CounterProfile::Zen,
            )
            .unwrap();
            let inputs = PreparedInputs {
                wall_clock_epoch: None,
                profile: backend.profile(),
                kernel: kernel.clone(),
                images: BTreeMap::new(),
                placement: BTreeMap::new(),
                guests: BTreeMap::new(),
                auxiliary: BTreeMap::new(),
                provenance: BTreeMap::new(),
            };
            assert_eq!(
                inputs.profile.engine, engine,
                "the run records the engine it used"
            );
            assert!(!inputs
                .profile
                .semantic_parameters
                .contains_key(crate::admission::ENGINE_QUALIFICATION));
            backend.validate_retained(&inputs).unwrap();
            // Older runs carry a dev marker and policy bindings; both are only data now.
            let mut marked = inputs.clone();
            for key in [crate::admission::ENGINE_QUALIFICATION, "host_policy"] {
                marked
                    .profile
                    .semantic_parameters
                    .insert(key.into(), "anything".into());
            }
            backend.validate_retained(&marked).unwrap();
            for key in ["guest_abi", "control_channel", "memory_mb"] {
                let mut wrong = inputs.clone();
                wrong
                    .profile
                    .semantic_parameters
                    .insert(key.into(), "wrong".into());
                assert!(backend.validate_retained(&wrong).is_err());
            }
            let report = backend
                .doctor_report(
                    kernel,
                    AdmissionProbe {
                        cpu: Some(8),
                        observations: vec![observed(8)],
                        result: Ok(()),
                    },
                )
                .unwrap();
            let json = serde_json::to_value(report).unwrap();
            assert!(json["engine_qualification"].is_null());
            assert!(json.get("simulator_version").is_none());
            assert!(json.get("qualified").is_none());
        }
    }

    #[test]
    fn pv_carries_no_counter_profile_and_needs_no_counter_admission() {
        let intel = VmmExecutor::from_engine_digest(
            ContentDigest::of(b"engine"),
            None,
            512,
            WitnessMode::Exits,
            CounterProfile::IntelIceLake,
        )
        .unwrap();
        let pv = intel.clone().with_timebase(Timebase::Pv);
        assert!(!pv.profile.semantic_parameters.contains_key(COUNTER));
        assert_eq!(recorded_counter(&pv.profile).unwrap(), CounterProfile::Zen);
        let zen_pv = VmmExecutor::from_engine_digest(
            ContentDigest::of(b"engine"),
            None,
            512,
            WitnessMode::Exits,
            CounterProfile::Zen,
        )
        .unwrap()
        .with_timebase(Timebase::Pv);
        assert_eq!(pv.profile, zen_pv.profile);
        let counted = pv.with_timebase(Timebase::Counter);
        assert_eq!(counted.profile, intel.profile);
    }

    #[test]
    fn experimental_mode_never_carries_amd_proof_and_cannot_relax_release_or_counter_checks() {
        let make = |counter| {
            VmmExecutor::from_engine_digest(
                ContentDigest::of(b"engine"),
                None,
                512,
                WitnessMode::Exits,
                counter,
            )
            .unwrap()
        };
        let experimental = make(CounterProfile::IntelIceLake);
        assert!(!experimental
            .profile
            .semantic_parameters
            .keys()
            .any(|key| key.starts_with("host_policy")));
        assert_eq!(
            recorded_counter(&experimental.profile).unwrap(),
            CounterProfile::IntelIceLake
        );
        assert!(crate::admission::validate_profile(&experimental.profile).is_ok());
        let mut wrong = experimental.profile.clone();
        wrong
            .semantic_parameters
            .insert("counter".into(), "unknown".into());
        assert!(crate::admission::validate_profile(&wrong).is_err());
        let inputs = PreparedInputs {
            wall_clock_epoch: None,
            kernel: ContentDigest::of(b"kernel"),
            images: BTreeMap::new(),
            placement: BTreeMap::new(),
            guests: BTreeMap::new(),
            profile: experimental.profile.clone(),
            auxiliary: BTreeMap::new(),
            provenance: BTreeMap::new(),
        };
        assert!(experimental.validate_retained(&inputs).is_ok());
        assert!(make(CounterProfile::IntelSkylake)
            .validate_retained(&inputs)
            .is_err());
    }

    #[test]
    fn doctor_report_preserves_actual_selected_core_and_exact_admitted_observations() {
        let executor = VmmExecutor::from_engine_digest(
            ContentDigest::of(b"engine"),
            None,
            1024,
            WitnessMode::Exits,
            CounterProfile::Zen,
        )
        .unwrap();
        let observations = vec![observed(2), observed(18)];
        let report = executor
            .doctor_report(
                ContentDigest::of(b"kernel"),
                AdmissionProbe {
                    cpu: Some(18),
                    observations: observations.clone(),
                    result: Ok(()),
                },
            )
            .unwrap();
        assert_eq!(report.cpu, 18); // Selected CPU need not be the first sorted sibling.
        assert_eq!(report.cpus, [2, 18]);
        assert_eq!(report.observations, observations);
        let json = serde_json::to_value(&report).unwrap();
        assert_eq!(
            json["observations"],
            serde_json::to_value(&observations).unwrap()
        );
        assert_eq!(json["cpu"], 18);
        assert!(executor.admission.lock().unwrap().is_empty()); // No cached/second observation supplied the receipt.
    }
    #[test]
    fn no_doctor_success_report_on_probe_cleanup_error_or_capability_drift() {
        let executor = VmmExecutor::from_engine_digest(
            ContentDigest::of(b"engine"),
            Some(2),
            1024,
            WitnessMode::Exits,
            CounterProfile::Zen,
        )
        .unwrap();
        for case in 0..3 {
            let mut observation = observed(2);
            if case == 1 {
                observation.perf_disabled_open = false;
            }
            if case == 2 {
                observation.immediate_exit = 0;
            }
            let result = if case == 0 {
                Err(Error::Execution(
                    "host preflight cleanup failed: affinity readback mismatch".into(),
                ))
            } else {
                Ok(())
            };
            assert!(executor
                .doctor_report(
                    ContentDigest::of(b"kernel"),
                    AdmissionProbe {
                        cpu: Some(2),
                        observations: vec![observation],
                        result
                    }
                )
                .is_err());
        }
    }
}
