//! Application adapter for retained Driver executables. The server's backend
//! provider selects this only for its explicit recorded executable contract.
#[cfg(test)]
mod tests;
use crate::{
    execution::{
        ExecutionBackend, ExecutionProfile, ExecutionReport, ExecutionRequest, PreparedInputs,
        StopReason,
    },
    worker_process::{incomplete, WorkerLaunch},
    Error, Result,
};
use std::{collections::BTreeMap, ffi::OsString, path::PathBuf};

/// Native implementation selected by the retained factory inventory, not by
/// an ambient environment variable or an executor-side backend override.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DriverBackend {
    Pv,
    Tcg,
}
/// Retained process adapter sharing the ordinary worker ownership, cancellation,
/// observation and journal boundaries. Construction performs no guest work.
// TODO: Re-evaluate: the execution process owns Driver and its nested native
// workers; putting Driver into the catalogue process breaks owner-death cleanup.
pub struct DriverExecutor {
    profile: ExecutionProfile,
    backend: DriverBackend,
    workers: usize,
    runtime: Option<super::binding::Binding>,
    environment: Vec<(OsString, OsString)>,
    session_lease: Option<PathBuf>,
}
impl DriverExecutor {
    /// Use an explicit retained profile and server-owned worker count. Factory
    /// inventory must name the same backend; no fallback or profile rewriting.
    pub fn new(profile: ExecutionProfile, backend: DriverBackend, workers: usize) -> Result<Self> {
        super::retained::validate_contract(&profile)?;
        if !(1..=64).contains(&workers) {
            return Err(Error::Setup("Driver machine workers: use 1..64".into()));
        }
        Ok(Self {
            profile,
            backend,
            workers,
            runtime: None,
            environment: vec![],
            session_lease: None,
        })
    }
    /// Set this host launch's exact environment and optional existing session
    /// lease. Server callers supply the path from their actual session owner;
    /// `None` is only for sessionless/offline callers. No path is inferred or
    /// environment discovered by this library. These are transient host inputs,
    /// never retained simulation metadata. Without this builder the environment
    /// is empty and no session is borrowed.
    // TODO: Re-evaluate: a monitor crosses a process boundary; implicit ambient
    // inheritance and output-ancestor searches lose explicit cleanup authority.
    pub fn with_launch_context(
        mut self,
        environment: Vec<(OsString, OsString)>,
        session_lease: Option<PathBuf>,
    ) -> Self {
        self.environment = environment;
        self.session_lease = session_lease;
        self
    }
    /// Borrow an explicitly selected immutable runtime across worker processes.
    /// The archive must match the retained factory inventory exactly; this never
    /// substitutes a startup/default archive for a different retained archive.
    /// This executor keeps the complete Inputs guard through worker shutdown.
    /// Callers may share cloned Inputs acquired in their owned session.
    // TODO: Re-evaluate: direct standalone executors may instead own extraction
    // inside their single worker; service callers already holding Inputs reuse it.
    pub fn with_runtime(mut self, inputs: crate::runtime_assets::Inputs) -> Result<Self> {
        if inputs.qemu.is_some() != (self.backend == DriverBackend::Tcg) {
            return Err(Error::Setup("Driver runtime Inputs select a different backend; select this executor backend from the retained archive".into()));
        }
        self.runtime = Some(super::binding::Binding::new(inputs)?);
        Ok(self)
    }
    // Controller mapping and continuation are scoped to Linux x86 PV.
    // Other PV hosts and TCG guest control remain explicit unproven paths.
    fn admit_execution(&self, controller: bool) -> Result<()> {
        if controller
            && !(self.backend == DriverBackend::Pv
                && cfg!(all(target_os = "linux", target_arch = "x86_64")))
        {
            return Err(Error::Setup(super::policy::CONTROLLER_INCOMPLETE.into()));
        }
        if self.backend == DriverBackend::Pv
            && !cfg!(all(target_os = "linux", target_arch = "x86_64"))
        {
            return Err(Error::Setup(super::policy::PV_INCOMPLETE.into()));
        }
        Ok(())
    }
    /// Descriptive current-host metadata and explicit owned launch contract. No
    /// revision, source/build hash, qualification or provenance admission gate.
    pub fn descriptor(
        backend: DriverBackend,
        guest_arch: &str,
    ) -> crate::execution::EngineDescriptor {
        crate::execution::EngineDescriptor {
            kind: crate::engine::ExecutableKind::DriverWorld,
            owner_pipe: true,
            live_observations: true,
            simulator_version: crate::engine::SIMULATOR_VERSION.into(),
            host_os: std::env::consts::OS.into(),
            host_arch: std::env::consts::ARCH.into(),
            guest_arch: guest_arch.into(),
            engine_backend: match backend {
                DriverBackend::Pv => "driver-pv",
                DriverBackend::Tcg => "driver-tcg",
            }
            .into(),
            engine_timebase: "driver-world-v1".into(),
        }
    }
}
impl ExecutionBackend for DriverExecutor {
    fn profile(&self) -> ExecutionProfile {
        self.profile.clone()
    }
    fn performance_parameters(&self) -> BTreeMap<String, String> {
        BTreeMap::from([("SEEDBOX_MACHINE_WORKERS".into(), self.workers.to_string())])
    }
    fn preflight_workers(&self, workers: u32) -> Result<()> {
        if !(1..=64).contains(&workers) {
            return Err(Error::Setup("--workers: use 1..64".into()));
        }
        self.preflight(None)
    }
    fn preflight(&self, _: Option<&std::path::Path>) -> Result<()> {
        // Fail before opening a native admission device or preparing workers.
        // TODO: Re-evaluate: remove only after real PV policy/evidence wiring
        // and acceptance, not by restoring an unconfigured None policy path.
        self.admit_execution(false)
    }
    fn validate_retained(&self, inputs: &PreparedInputs) -> Result<()> {
        if inputs.profile != self.profile {
            return Err(Error::Integrity(
                "retained Driver profile differs from executor".into(),
            ));
        }
        super::retained::Closure::from_inputs(inputs)?;
        Ok(())
    }
    fn runtime_inputs<'a>(
        &self,
        inputs: &'a PreparedInputs,
    ) -> Vec<&'a crate::store::ContentDigest> {
        inputs
            .digests()
            .filter(|digest| {
                *digest == &inputs.profile.engine
                    || self
                        .runtime
                        .as_ref()
                        .is_none_or(|runtime| !runtime.files.contains_key(*digest))
            })
            .collect()
    }
    fn execute(&self, request: ExecutionRequest<'_>) -> Result<ExecutionReport> {
        self.validate_retained(request.prepared)?;
        if request.cancellation.is_cancelled() {
            return Ok(incomplete(StopReason::Cancelled));
        }
        self.admit_execution(request.control.is_some())?;
        let mut launch = WorkerLaunch::prepare_driver_with_runtime(
            &request,
            self.backend,
            self.runtime.as_ref(),
        )?;
        let overrides: Vec<_> = launch
            .command
            .get_envs()
            .map(|(key, value)| (key.to_owned(), value.map(std::ffi::OsStr::to_owned)))
            .collect();
        launch
            .command
            .env_clear()
            .envs(self.environment.iter().cloned());
        for (key, value) in overrides {
            match value {
                Some(value) => {
                    launch.command.env(key, value);
                }
                None => {
                    launch.command.env_remove(key);
                }
            }
        }
        let monitor = request
            .artifacts
            .get(&self.profile.engine)
            .ok_or_else(|| Error::Integrity("missing retained Driver monitor executable".into()))?;
        let mut worker = launch.spawn_with_monitor(monitor, self.session_lease.as_deref())?;
        if let Some(runtime) = &self.runtime {
            worker = worker.retain_until_join(runtime.retained_inputs());
        }
        worker.wait(
            request.cancellation,
            request.limits.wall_time_ns,
            decode_report,
        )
    }
}

// Decode the report from an exited owned worker. WorkerProcess::wait still owns
// final cleanup. Actual native witness production belongs to the worker/backend;
// this boundary cannot promote publication/policy hashes or erase oracle results.
fn decode_report(bytes: &[u8]) -> Result<ExecutionReport> {
    let mut report: ExecutionReport = serde_json::from_slice(bytes)?;
    if report.reason != StopReason::Error && report.witness.is_none() {
        report.reason = StopReason::Error;
        let missing = "retained Driver worker omitted its required native execution witness";
        report.detail = Some(match report.detail {
            Some(previous) => format!("{previous}; {missing}"),
            None => missing.into(),
        });
    }
    Ok(report)
}
