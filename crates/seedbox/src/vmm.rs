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

#[allow(dead_code)]
#[rustfmt::skip]
#[path = "../../seedbox-vmm/src/checkpoint.rs"]
mod checkpoint;
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
    allow_unqualified: bool,
    profile: ExecutionProfile,
    performance: BTreeMap<String, String>,
    admission: std::sync::Arc<std::sync::Mutex<Vec<Vec<crate::admission::CoreObservation>>>>,
}

/// Successful compatibility observation; it is not empirical replay qualification.
#[derive(Debug, Serialize)]
pub struct DoctorReport {
    pub cpu: u32,
    pub cpus: Vec<u32>,
    pub engine: ContentDigest,
    pub kernel: ContentDigest,
    pub observations: Vec<crate::admission::CoreObservation>,
}

struct AdmissionProbe {
    cpu: Option<u32>,
    observations: Vec<crate::admission::CoreObservation>,
    result: Result<()>,
}

impl VmmExecutor {
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
            allow_unqualified: false,
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
    pub fn with_timebase(mut self, timebase: Timebase) -> Self {
        if timebase == Timebase::Counter {
            self.profile.semantic_parameters.remove("timebase");
        } else {
            self.profile
                .semantic_parameters
                .insert("timebase".into(), timebase.name().into());
        }
        self
    }

    /// Check the existing execution policy without storing inputs or starting guests.
    /// Host observations are the same bounded probe used by execution preflight.
    pub fn doctor(engine: &Path, kernel: &Path, cpu: Option<u32>) -> Result<DoctorReport> {
        let executor = Self::from_engine_digest(
            crate::admission::file_digest(engine)?,
            cpu,
            1024,
            WitnessMode::Exits,
            CounterProfile::Zen,
        )?;
        crate::admission::validate_profile(&executor.profile)?;
        let kernel = crate::admission::file_digest(kernel)?;
        Self::doctor_counter(crate::counter::CounterProfile::host()?)?;
        let probe = executor.probe();
        executor.doctor_report(kernel, probe)
    }

    fn doctor_counter(counter: CounterProfile) -> Result<()> {
        if !counter.qualified() {
            return Err(Error::Setup(format!("doctor cannot qualify the unqualified {} counter; --allow-unqualified-cpu permits experimental execution only", counter.name())));
        }
        Ok(())
    }

    /// Explicitly permit an experimental counter; release and input checks still apply.
    pub fn with_unqualified_cpu(mut self, allow: bool) -> Self {
        self.allow_unqualified = allow;
        self
    }

    fn validate_execution_mode(&self) -> Result<()> {
        if !self.counter.qualified() && !self.allow_unqualified {
            return Err(Error::Setup(format!(
                "{} is unqualified; explicitly allow unqualified CPU execution",
                self.counter.name()
            )));
        }
        Ok(())
    }

    fn doctor_report(&self, kernel: ContentDigest, probe: AdmissionProbe) -> Result<DoctorReport> {
        probe.result?;
        crate::admission::validate_profile(&self.profile)?;
        for observation in &probe.observations {
            crate::admission::validate_observation(observation)?;
        }
        Ok(DoctorReport {
            cpu: probe
                .cpu
                .ok_or_else(|| Error::Execution("host probe did not select a CPU".into()))?,
            cpus: probe
                .observations
                .iter()
                .map(|observation| observation.cpu)
                .collect(),
            engine: self.profile.engine.clone(),
            kernel,
            observations: probe.observations,
        })
    }

    fn unqualified_preflight(&self, output: Option<&Path>) -> Result<()> {
        #[cfg(target_os = "linux")]
        {
            let reservation = crate::host::CpuReservation::acquire(self.cpu)?;
            let result =
                crate::admission::check_unqualified(reservation.cpus.iter().copied(), self.counter);
            if let Some(output) = output {
                crate::admission::finish_admission(result, |_| {
                    std::fs::write(
                        output.join("host-unqualified.json"),
                        serde_json::to_vec_pretty(
                            &serde_json::json!({"counter":self.counter.name(),"qualified":false,"cpu":reservation.cpu,"cpus":reservation.cpus}),
                        )?,
                    )?;
                    Ok(())
                })
            } else {
                result
            }
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = output;
            Err(Error::Setup(
                "experimental execution requires a Linux KVM host".into(),
            ))
        }
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
                                crate::admission::observe,
                            )?;
                        }
                        Ok(())
                    },
                )?; // All reservations/perf/affinity close before publication or OCI work.
            }
            #[cfg(not(target_os = "linux"))]
            {
                let _ = (&mut selected, workers);
                observations.push(crate::admission::observe(0)?);
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

    /// Host CPU time a PV run may go without an exit before the VMM checks
    /// it for a spin loop. Changes only how soon a spin is noticed.
    pub fn with_pv_watchdog_ms(mut self, ms: u64) -> Self {
        self.performance
            .insert("SEEDBOX_PV_WATCHDOG_MS".into(), ms.to_string());
        self
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
        self.validate_execution_mode()?;
        if !self.counter.qualified() {
            if workers == 1 {
                return self.unqualified_preflight(output);
            }
            #[cfg(target_os = "linux")]
            {
                return with_capacity(
                    workers,
                    || crate::host::CpuReservation::acquire(self.cpu),
                    |reservations| {
                        for reservation in reservations {
                            crate::admission::check_unqualified(
                                reservation.cpus.iter().copied(),
                                self.counter,
                            )?;
                        }
                        Ok(())
                    },
                );
            }
            #[cfg(not(target_os = "linux"))]
            {
                return self.unqualified_preflight(output);
            }
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
        self.validate_execution_mode()?;
        if recorded_counter(&inputs.profile)? != self.counter {
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
                "KVM execution requires the qualified Linux amd64 host".into(),
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
        Ok(json!({"from":machine(&link.from)?,"to":machine(&link.to)?,"latency_ns":link.latency_ns,"jitter_ns":link.jitter_ns,"loss_ppm":link.loss_ppm}))
    }).collect::<Result<Vec<_>>>()?;
    let mut network =
        json!({"latency_ns":request.network.latency_ns,"jitter_ns":request.network.jitter_ns});
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
    if recorded_torn_writes(&request.prepared.profile)? {
        world[DISK_LOSS] = json!({
            "sector_bytes": 512,
            "survival_probability": 0.5,
        });
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
            if !matches!(plan.action, Action::Stop { .. } | Action::Restart { .. })
                || e.is_none_or(|e| e > plan.deadline_ns)
            {
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
            "unqualified VMM report is missing its counter profile".into(),
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
    use std::{
        fs,
        os::unix::{fs::PermissionsExt, process::CommandExt},
        process::{Child, Command},
        time::{Duration, Instant},
    };

    // VMM is a single-process engine. Keep its unreaped leader owned until the
    // final process-group signal, including when it has already exited.
    struct OwnedChild(Option<Child>);
    impl OwnedChild {
        fn stop(&mut self) -> Result<()> {
            if let Some(child) = &mut self.0 {
                let killed = unsafe { libc::kill(-(child.id() as i32), libc::SIGKILL) };
                if killed != 0
                    && std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
                {
                    return Err(std::io::Error::last_os_error().into());
                }
                child.wait()?;
                self.0 = None;
            }
            Ok(())
        }
        fn exited(&self) -> Result<Option<bool>> {
            let child = self.0.as_ref().unwrap();
            let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
            if unsafe {
                libc::waitid(
                    libc::P_PID,
                    child.id(),
                    &mut info,
                    libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
                )
            } != 0
            {
                let error = std::io::Error::last_os_error();
                if error.kind() == std::io::ErrorKind::Interrupted {
                    return Ok(None);
                }
                return Err(error.into());
            }
            Ok(unsafe {
                (info.si_pid() != 0)
                    .then(|| info.si_code == libc::CLD_EXITED && info.si_status() == 0)
            })
        }
    }
    impl Drop for OwnedChild {
        fn drop(&mut self) {
            // An owned live child must never outlive its host-state guard.
            // Retry interrupted waits before the guard can restore MSRs.
            while self.0.is_some() {
                if let Err(error) = self.stop() {
                    eprintln!("seedbox: waiting for VMM cleanup: {error}");
                    std::thread::sleep(Duration::from_millis(10));
                }
            }
        }
    }

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
        let world = world(&request)?;
        let world_path = request.output.join("world.json");
        fs::write(&world_path, serde_json::to_vec_pretty(&world)?)?;
        let engine = request
            .artifacts
            .get(&backend.profile.engine)
            .ok_or_else(|| Error::Integrity("missing retained VMM".into()))?;
        fs::set_permissions(engine, fs::Permissions::from_mode(0o500))?;
        let report_path = request.output.join("vmm.json");
        let endpoint = crate::host_control::Endpoint::new(request.output)?;
        let mut command = Command::new(engine);
        command
            .arg("--config")
            .arg(&world_path)
            .arg("--seed")
            .arg(request.seed.to_string())
            .arg("--out")
            .arg(request.output.join(MACHINE_OUTPUT_DIR));
        if timebase != Timebase::Counter {
            command.arg("--timebase").arg(timebase.name());
        }
        if recorded_deferred_tx(&backend.profile)? {
            command.arg("--deferred-tx");
        }
        command.arg("--host-control").arg(endpoint.socket());
        command
            .arg("--stream-console")
            // Bound observation metadata to at most 40 MiB across all streams.
            .arg("--log-chunk-limit")
            .arg(
                (1_048_576 / (2 * request.prepared.guests.len()))
                    .min(65_536)
                    .to_string(),
            )
            .arg("--console-limit")
            .arg((1024 * 1024 * 1024 / (2 * request.prepared.guests.len())).to_string());
        if backend.profile.witness_mode == WitnessMode::FullState {
            command.arg("--digest");
        }
        // Do not inherit hidden performance/semantic VMM environment settings.
        command.env_clear().envs(request.performance_parameters);
        command
            .stdout(fs::File::create(&report_path)?)
            .stderr(fs::File::create(request.output.join("vmm.stderr"))?);
        let incomplete = |reason| ExecutionReport {
            timebase: Some(timebase),
            reason,
            workload_exit: None,
            witness: None,
            full_state: None,
            faults: vec![],
            detail: None,
            engine_qualification: None,
        };
        if request.cancellation.is_cancelled() {
            return Ok(incomplete(StopReason::Cancelled));
        }
        let static_evidence = backend
            .admission
            .lock()
            .map_err(|_| Error::Execution("host admission evidence lock poisoned".into()))?
            .clone();
        if backend.counter.qualified() {
            fs::write(
                request.output.join("host-static.json"),
                serde_json::to_vec_pretty(&static_evidence)?,
            )?;
        }
        let (cpu, mut lease) = HostLease::acquire_checked(
            backend.cpu,
            backend.counter.workaround(),
            request.output,
            |reservation| {
                if !backend.counter.qualified() {
                    let check = crate::admission::check_unqualified(
                        reservation.cpus.iter().copied(),
                        backend.counter,
                    );
                    return crate::admission::finish_admission(check, |result| {
                        fs::write(
                            request.output.join("host-leased-unqualified.json"),
                            serde_json::to_vec_pretty(
                                &serde_json::json!({"counter":backend.counter.name(),"qualified":false,"cpu":reservation.cpu,"cpus":reservation.cpus,"applicable_checks_passed":result.is_ok(),"error":result.as_ref().err().map(ToString::to_string)}),
                            )?,
                        )?;
                        Ok(())
                    });
                }
                let mut observed = Vec::new();
                let admission = crate::admission::collect_admitted(
                    reservation.cpus.iter().copied(),
                    &mut observed,
                    crate::admission::observe,
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
            },
        )?;
        let originals = lease.original_registers();
        if backend.counter.workaround().is_some() {
            fs::write(
                request.output.join("host-workaround.json"),
                serde_json::to_vec_pretty(&lease.enabled_registers()?)?,
            )?;
        }
        unsafe {
            command.pre_exec(move || {
                if libc::setpgid(0, 0) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                let mut set: libc::cpu_set_t = std::mem::zeroed();
                libc::CPU_ZERO(&mut set);
                libc::CPU_SET(cpu as usize, &mut set);
                if libc::sched_setaffinity(0, std::mem::size_of_val(&set), &set) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let started = Instant::now();
        let mut child = OwnedChild(Some(command.spawn()?));
        let completion = loop {
            if request.cancellation.is_cancelled() {
                break Ok(incomplete(StopReason::Cancelled));
            }
            if let Some(success) = child.exited()? {
                break if success {
                    report(&fs::read(&report_path)?, &request)
                } else {
                    Err(Error::Execution(VMM_FAILURE_MESSAGE.into()))
                };
            }
            if started.elapsed() >= Duration::from_nanos(request.limits.wall_time_ns) {
                break Ok(incomplete(StopReason::WallTimeLimit));
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        child.stop()?;
        lease.restore()?;
        if backend.counter.workaround().is_some() {
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
        let experimental = VmmExecutor::from_engine_digest(
            pinned.profile.engine,
            None,
            512,
            WitnessMode::Exits,
            CounterProfile::IntelIceLake,
        )
        .unwrap();
        assert!(experimental
            .preflight_workers(2)
            .unwrap_err()
            .to_string()
            .contains("explicitly allow unqualified CPU execution"));
    }

    fn observed(cpu: u32) -> CoreObservation {
        let p = required_policy();
        CoreObservation {
            cpu,
            architecture: p.architecture,
            vendor: p.vendor,
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
        }
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
        assert!(experimental.validate_execution_mode().is_err());
        assert!(experimental
            .clone()
            .with_unqualified_cpu(true)
            .validate_execution_mode()
            .is_ok());
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
        for counter in [CounterProfile::IntelIceLake, CounterProfile::IntelSkylake] {
            assert!(VmmExecutor::doctor_counter(counter).is_err());
        }
        assert!(VmmExecutor::doctor_counter(CounterProfile::Zen).is_ok());
        let mut wrong = experimental.profile.clone();
        wrong
            .semantic_parameters
            .insert("counter".into(), "unknown".into());
        assert!(crate::admission::validate_profile(&wrong).is_err());
        let inputs = PreparedInputs {
            kernel: ContentDigest::of(b"kernel"),
            images: BTreeMap::new(),
            placement: BTreeMap::new(),
            guests: BTreeMap::new(),
            profile: experimental.profile.clone(),
            auxiliary: BTreeMap::new(),
            provenance: BTreeMap::new(),
        };
        assert!(experimental.validate_retained(&inputs).is_err());
        assert!(experimental
            .clone()
            .with_unqualified_cpu(true)
            .validate_retained(&inputs)
            .is_ok());
        assert!(make(CounterProfile::IntelSkylake)
            .with_unqualified_cpu(true)
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
