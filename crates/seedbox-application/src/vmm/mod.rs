//! Adapter for the controlled Seedbox KVM engine. World JSON is an internal
//! execution artifact generated from verified inputs, not the authoring format.

use crate::{
    execution::*,
    store::{ContentDigest, ContentStore},
    Error, Result,
};
use serde::Serialize;
use std::{collections::BTreeMap, path::Path};

#[derive(Clone, Debug)]
pub struct VmmExecutor {
    profile: ExecutionProfile,
    performance: BTreeMap<String, String>,
    admission: std::sync::Arc<std::sync::Mutex<Vec<Vec<crate::admission::CoreObservation>>>>,
}

/// Successful compatibility observation; it is not empirical replay qualification.
#[derive(Debug, Serialize)]
pub struct DoctorReport {
    pub engine: ContentDigest,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub simulator_version: Option<String>,
    pub kernel: ContentDigest,
    pub observations: Vec<crate::admission::CoreObservation>,
}

struct AdmissionProbe {
    observations: Vec<crate::admission::CoreObservation>,
    result: Result<()>,
}

impl VmmExecutor {
    /// Detect the launch contract of an explicitly supplied executable.
    pub fn new_detected(
        engine: &Path,
        store: &ContentStore,
        memory_mb: u32,
        witness_mode: WitnessMode,
    ) -> Result<Self> {
        Self::new(engine, store, memory_mb, witness_mode)?.with_detected_contract(store, engine)
    }

    /// Preserve simulation settings and use the replacement's own argv contract.
    /// Normal replay uses `from_retained` without probing or rewriting metadata.
    pub fn from_override(
        engine: &Path,
        store: &ContentStore,
        profile: ExecutionProfile,
    ) -> Result<Self> {
        Self::from_retained(engine, store, profile)?.with_detected_contract(store, engine)
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
        let engine = store.materialize_executable(&self.profile.engine, scratch.path())?;
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&engine, std::fs::Permissions::from_mode(0o500))?;
        let version = crate::worker_process::replacement_version(&engine, supplied_path)?;
        self.profile.semantic_parameters.engine = version.map(|version| {
            crate::engine::kvm_descriptor(crate::engine::ExecutableKind::InternalWorker, version)
        });
        Ok(self)
    }

    /// Retain this Seedbox executable and explicitly record its worker contract.
    pub fn new_internal(
        engine: &Path,
        store: &ContentStore,
        memory_mb: u32,
        witness_mode: WitnessMode,
    ) -> Result<Self> {
        let descriptor = crate::engine::internal_descriptor()?;
        let mut executor = Self::new(engine, store, memory_mb, witness_mode)?;
        executor.profile.semantic_parameters.engine = Some(descriptor);
        Ok(executor)
    }

    /// Preserve retained metadata exactly, without detecting a new launch contract.
    /// A replacement executable changes only
    /// its digest, so the orchestrator can retain profile/admission rejection.
    pub fn from_retained(
        engine: &Path,
        store: &ContentStore,
        mut profile: ExecutionProfile,
    ) -> Result<Self> {
        profile.engine = store.put_file(engine)?;
        Ok(Self {
            profile,
            performance: BTreeMap::new(),
            admission: Default::default(),
        })
    }

    pub fn new(
        engine: &Path,
        store: &ContentStore,
        memory_mb: u32,
        witness_mode: WitnessMode,
    ) -> Result<Self> {
        if !(64..=65536).contains(&memory_mb) {
            return Err(Error::Execution(
                "guest memory must be 64..65536 MiB".into(),
            ));
        }
        Self::from_engine_digest(store.put_file(engine)?, memory_mb, witness_mode)
    }

    fn from_engine_digest(
        engine: ContentDigest,
        memory_mb: u32,
        witness_mode: WitnessMode,
    ) -> Result<Self> {
        if !(64..=65536).contains(&memory_mb) {
            return Err(Error::Execution(
                "guest memory must be 64..65536 MiB".into(),
            ));
        }
        let semantic_parameters = SemanticParameters::new(memory_mb);
        Ok(Self {
            admission: Default::default(),
            profile: ExecutionProfile {
                backend: Backend::Kvm,
                engine,
                witness_mode,
                semantic_parameters,
            },
            performance: BTreeMap::new(),
        })
    }

    /// Defer virtio-net TX kicks to deterministic polls (semantic, off by default).
    pub fn with_deferred_tx(mut self, enabled: bool) -> Self {
        self.profile.semantic_parameters.deferred_tx = enabled;
        self
    }

    /// Read guest timer deadlines from the time page (semantic, off by default).
    pub fn with_page_timer(mut self, enabled: bool) -> Self {
        self.profile.semantic_parameters.page_timer = enabled;
        self
    }

    /// Check the existing execution policy without storing inputs or starting guests.
    /// Host observations are the same bounded probe used by execution preflight.
    pub fn doctor(engine: &Path, kernel: &Path) -> Result<DoctorReport> {
        let executor = Self::from_engine_digest(
            crate::admission::engine_digest(engine)?,
            1024,
            WitnessMode::Exits,
        )?;
        Self::doctor_executor(executor, kernel)
    }

    /// Inspect the built-in worker without storing bytes or invoking it.
    pub fn doctor_internal(engine: &Path, kernel: &Path) -> Result<DoctorReport> {
        let descriptor = crate::engine::internal_descriptor()?;
        let mut executor = Self::from_engine_digest(
            crate::admission::engine_digest(engine)?,
            1024,
            WitnessMode::Exits,
        )?;
        executor.profile.semantic_parameters.engine = Some(descriptor);
        Self::doctor_executor(executor, kernel)
    }

    fn doctor_executor(executor: Self, kernel: &Path) -> Result<DoctorReport> {
        crate::admission::validate_profile(&executor.profile)?;
        let kernel = crate::admission::file_digest(kernel)?;
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
            engine: self.profile.engine.clone(),
            simulator_version: self.simulator_version(),
            kernel,
            observations: probe.observations,
        })
    }

    fn probe(&self) -> AdmissionProbe {
        let mut observations = Vec::new();
        #[cfg(target_os = "linux")]
        let access = crate::host::check_kvm_access();
        #[cfg(not(target_os = "linux"))]
        let access: Result<()> = Ok(());
        let result = access.and_then(|()| {
            crate::admission::collect_admitted(&mut observations, crate::admission::observe)
        });
        AdmissionProbe {
            observations,
            result,
        }
    }

    /// Bind a runtime checkpoint and its branch seed into retained semantics.
    pub fn with_checkpoint(mut self, at_ns: u64, branch_seed: u64) -> Self {
        self.profile.semantic_parameters.checkpoint = Some(Checkpoint { at_ns, branch_seed });
        self
    }

    /// Seeded survival of the latest unflushed 512-byte sectors on crash.
    pub fn with_torn_writes(mut self, enabled: bool) -> Self {
        self.profile.semantic_parameters.torn_writes = enabled;
        self
    }

    fn simulator_version(&self) -> Option<String> {
        self.profile
            .semantic_parameters
            .engine
            .as_ref()
            .map(|engine| engine.simulator_version.clone())
    }

    /// Host CPU interval between watchdog observations; does not change guest time.
    pub fn with_watchdog_ms(mut self, milliseconds: u64) -> Result<Self> {
        if !(1..=60000).contains(&milliseconds) {
            return Err(Error::Setup("--watchdog-ms must be 1..60000".into()));
        }
        self.performance
            .insert("SEEDBOX_WATCHDOG_MS".into(), milliseconds.to_string());
        Ok(self)
    }
}

/// Checkpoints are optional, but partial or malformed retained settings fail closed.
impl VmmExecutor {
    fn preflight_host(&self, output: Option<&Path>) -> Result<()> {
        crate::admission::validate_profile(&self.profile)?;
        let probe = self.probe();
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
}

/// Missing page timer means the legacy port transport.
impl ExecutionBackend for VmmExecutor {
    fn preflight_workers(&self, workers: u32) -> Result<()> {
        if !(1..=32).contains(&workers) {
            return Err(Error::Setup("--workers: use 1..32".into()));
        }
        self.preflight(None)
    }

    fn preflight(&self, output: Option<&Path>) -> Result<()> {
        self.preflight_host(output)
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
        crate::admission::validate_profile(&inputs.profile)
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

#[cfg(target_os = "linux")]
mod linux;
#[cfg(test)]
mod tests;
