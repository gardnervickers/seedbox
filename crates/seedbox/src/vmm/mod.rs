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

/// Recorded performance parameters before `--margin`/`--undo` apply.
fn default_performance() -> BTreeMap<String, String> {
    use seedbox_engine_core::launch::{ADAPTIVE_MARGIN, DEFAULT_UNDO_MARGIN};
    BTreeMap::from([
        ("SEEDBOX_MARGIN".into(), ADAPTIVE_MARGIN.into()),
        ("SEEDBOX_UNDO".into(), DEFAULT_UNDO_MARGIN.to_string()),
    ])
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
            performance: default_performance(),
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
            performance: default_performance(),
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

/// The retained executable and its recorded dispatch, without consulting the host.
/// Run settings follow in its launch request (`worker_process`).
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
        }
    }
    Ok(command)
}

/// The run-shaping machine features a profile records, as the x86 worker takes them.
pub(crate) fn recorded_features(
    profile: &ExecutionProfile,
) -> Result<seedbox_engine_core::launch::MachineFeatures> {
    Ok(seedbox_engine_core::launch::MachineFeatures {
        timebase: recorded_timebase(profile)?,
        page_timer: recorded_page_timer(profile),
        deferred_tx: recorded_deferred_tx(profile)?,
    })
}

/// Missing page timer means the legacy port transport.
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
    /// The engine runs the retained executable and reads only the kernel and
    /// each machine's guest image; image layers stay in the store.
    fn runtime_inputs<'a>(&self, inputs: &'a PreparedInputs) -> Vec<&'a ContentDigest> {
        [&inputs.profile.engine, &inputs.kernel]
            .into_iter()
            .chain(inputs.guests.values())
            .collect()
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

mod profile;
pub use profile::*;
mod report;
pub use report::*;
mod world;
pub use world::*;
#[cfg(target_os = "linux")]
mod linux;
#[cfg(test)]
mod tests;
