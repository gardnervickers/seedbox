//! Native HVF application admission and execution.
use crate::{execution::*, store::ContentDigest, Error, Result};
use seedbox_arm64::{
    backend::{hvf::Hvf, Backend, SysReg},
    boot::BootSpec,
};
use std::{collections::BTreeMap, fs, path::Path};

const BOOT_POLICY: &str = "image-reservation/archive-aligned/final-dtb-v1";
const ENTROPY_POLICY: &str = "sha256-domain-u64-seed-v1";
const DEVICE_POLICY: &str = "net-console-optional-block/supervisor-pid1-v1";

#[derive(Clone, Debug)]
pub struct MacExecutor {
    engine: Engine,
    profile: ExecutionProfile,
    performance: BTreeMap<String, String>,
}
#[derive(Debug, serde::Serialize)]
pub struct DoctorReport {
    pub engine: EngineDescriptor,
    pub kernel: ContentDigest,
    pub observations: BTreeMap<String, String>,
}

/// Hypervisor.framework holds one VM per process, so concurrent exploration
/// attempts must take turns probing from the CLI; each engine worker process
/// then owns its own VM.
static PROBE: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn observe() -> Result<BTreeMap<String, String>> {
    let _probe = PROBE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let mut backend = Hvf::create().map_err(Error::Setup)?;
    backend.create_vcpu().map_err(Error::Setup)?;
    seedbox_arm64::machine::configure_cpu_features(&mut backend).map_err(Error::Setup)?;
    let mut features = BTreeMap::new();
    for reg in [
        0xc000, 0xc020, 0xc021, 0xc028, 0xc029, 0xc030, 0xc031, 0xc038, 0xc039, 0xc03a,
    ] {
        features.insert(
            format!("{reg:04x}"),
            backend.get_sys_reg(SysReg(reg)).map_err(Error::Setup)?,
        );
    }
    // Also require complete state support while no guest or retained run exists.
    backend.state().map_err(Error::Setup)?;
    let mut uname: libc::utsname = unsafe { std::mem::zeroed() };
    if unsafe { libc::uname(&mut uname) } != 0 {
        return Err(Error::Setup(format!(
            "HVF host inventory: {}",
            std::io::Error::last_os_error()
        )));
    }
    let release = unsafe { std::ffi::CStr::from_ptr(uname.release.as_ptr()) }
        .to_str()
        .map_err(|_| Error::Setup("HVF host release is not UTF-8".into()))?;
    Ok(BTreeMap::from([
        ("hvf_host_release".into(), release.into()),
        (
            "hvf_cpu_features".into(),
            ContentDigest::of(&serde_json::to_vec(&features)?)
                .hex()
                .into(),
        ),
    ]))
}
/// Probe HVF before downloading immutable guest inputs; retain no VM or vCPU.
pub fn check_host_access() -> Result<()> {
    observe().map(|_| ())
}
impl MacExecutor {
    /// Run worlds in the ARM worker of `engine`, the running signed executable.
    pub fn new(
        engine: Engine,
        kernel: &Path,
        memory_mb: u32,
        witness_mode: WitnessMode,
        torn_writes: bool,
    ) -> Result<Self> {
        if !(64..=3072).contains(&memory_mb) {
            return Err(Error::Setup(
                "x-seedbox.machine.memory_mb: ARM guests require 64..3072 MiB".into(),
            ));
        }
        validate_kernel(kernel, memory_mb)?;
        let kernel = crate::admission::file_digest(kernel)?;
        let mut host = observe()?;
        let mut inventory = |key: &str| {
            host.remove(key)
                .ok_or_else(|| Error::Setup(format!("HVF host inventory: missing {key}")))
        };
        let parameters = SemanticParameters {
            torn_writes,
            hvf: Some(HvfParameters {
                boot_policy: BOOT_POLICY.into(),
                entropy_policy: ENTROPY_POLICY.into(),
                device_policy: DEVICE_POLICY.into(),
                host_release: inventory("hvf_host_release")?,
                cpu_features: inventory("hvf_cpu_features")?,
                guest_kernel: kernel,
                launch_max_skew_ns: crate::engine::ARM_LAUNCH_MAX_SKEW_NS,
            }),
            ..SemanticParameters::new(memory_mb)
        };
        Ok(Self {
            engine,
            profile: ExecutionProfile {
                backend: crate::execution::Backend::Hvf,
                witness_mode,
                semantic_parameters: parameters,
            },
            performance: BTreeMap::new(),
        })
    }
    /// Execute previously resolved inputs with the current engine.
    pub fn for_profile(engine: Engine, profile: ExecutionProfile) -> Self {
        Self {
            engine,
            profile,
            performance: BTreeMap::new(),
        }
    }
    pub fn doctor(engine: &Engine, kernel: &Path) -> Result<DoctorReport> {
        validate_kernel(kernel, 3072)?;
        Ok(DoctorReport {
            engine: crate::engine::arm_descriptor(engine.revision.clone()),
            kernel: crate::admission::file_digest(kernel)?,
            observations: observe()?,
        })
    }
}
fn validate_kernel(kernel: &Path, memory_mb: u32) -> Result<()> {
    let image = fs::read(kernel).map_err(|e| {
        Error::Setup(format!(
            "--kernel {}: {e}; provide an ARM64 Image",
            kernel.display()
        ))
    })?;
    BootSpec::for_application(
        &image,
        b"validation",
        0,
        u64::from(memory_mb) * 1024 * 1024,
        seedbox_arm64::boot::DEFAULT_COMMAND_LINE,
    )
    .map_err(|e| {
        Error::Setup(format!(
            "--kernel {}: {e}; provide an ARM64 Image that fits x-seedbox.machine.memory_mb",
            kernel.display()
        ))
    })?;
    Ok(())
}
fn validate_profile(profile: &ExecutionProfile) -> Result<()> {
    let p = &profile.semantic_parameters;
    if profile.backend != crate::execution::Backend::Hvf {
        return Err(Error::Integrity(
            "retained profile does not use the HVF backend".into(),
        ));
    }
    if p.hvf.is_none() {
        return Err(Error::Integrity(
            "retained HVF profile missing its HVF parameters".into(),
        ));
    }
    if !(64..=3072).contains(&p.memory_mb) {
        return Err(Error::Integrity(
            "retained HVF profile has invalid memory_mb".into(),
        ));
    }
    Ok(())
}
impl ExecutionBackend for MacExecutor {
    fn profile(&self) -> ExecutionProfile {
        self.profile.clone()
    }
    fn engine(&self) -> Option<EngineDescriptor> {
        Some(crate::engine::arm_descriptor(self.engine.revision.clone()))
    }
    fn performance_parameters(&self) -> BTreeMap<String, String> {
        self.performance.clone()
    }
    /// Every attempt is its own signed engine process with its own HVF VM, so
    /// the Mac needs no core reservation: admit the range, then probe HVF once.
    fn preflight_workers(&self, workers: u32) -> Result<()> {
        if !(1..=32).contains(&workers) {
            return Err(Error::Setup("--workers: use 1..32".into()));
        }
        self.preflight(None)
    }
    fn preflight(&self, output: Option<&Path>) -> Result<()> {
        validate_profile(&self.profile)?;
        let observations = observe()?;
        if let Some(output) = output {
            fs::write(
                output.join("host-hvf.json"),
                serde_json::to_vec_pretty(&observations)?,
            )?;
        }
        Ok(())
    }
    fn validate_retained(&self, inputs: &PreparedInputs) -> Result<()> {
        validate_profile(&inputs.profile)?;
        if inputs
            .profile
            .semantic_parameters
            .hvf
            .as_ref()
            .map(|hvf| &hvf.guest_kernel)
            != Some(&inputs.kernel)
        {
            return Err(Error::Integrity("retained HVF kernel hash mismatch".into()));
        }
        Ok(())
    }
    fn execute(&self, request: ExecutionRequest<'_>) -> Result<ExecutionReport> {
        self.validate_retained(request.prepared)?;
        if request.prepared.profile != self.profile {
            return Err(Error::Integrity("execution profile mismatch".into()));
        }
        self.preflight(Some(request.output))?;
        use crate::worker_process::{incomplete, WorkerLaunch};
        if request.cancellation.is_cancelled() {
            return Ok(incomplete(StopReason::Cancelled));
        }
        WorkerLaunch::prepare(
            &request,
            &self.engine.executable,
            crate::engine::ExecutableKind::ArmWorld,
        )?
        .spawn()?
        .wait(request.cancellation, request.limits.wall_time_ns, |bytes| {
            crate::engine_adapter::report_hvf(bytes, &request)
        })
    }
}

pub(crate) fn validate_report(bytes: &[u8], request: &ExecutionRequest<'_>) -> Result<()> {
    let report: serde_json::Value = serde_json::from_slice(bytes)?;
    if report["timebase"] != crate::engine::ARM_REPORT_TIMEBASE {
        return Err(Error::Integrity(
            "HVF report missing application timebase".into(),
        ));
    }
    let launch_skew = crate::engine::arm_launch_skew(
        &request.prepared.profile,
        request
            .network
            .latency_ns
            .saturating_sub(request.network.jitter_ns),
    );
    validate_initial_launch(
        &report,
        launch_skew,
        request.seed,
        request.limits.simulated_time_ns,
        request.prepared.guests.keys().map(String::as_str),
    )?;
    let kernel_path = request
        .artifacts
        .get(&request.prepared.kernel)
        .ok_or_else(|| Error::Integrity("missing retained kernel".into()))?;
    let kernel = fs::read(kernel_path)?;
    // A reseed input moves later draws to the branch's streams: timers of
    // every incarnation, and boot entropy of incarnations a later crash made.
    let reseed: Option<(u64, u64, String)> = serde_json::from_value::<
        Vec<seedbox_engine_protocol::input_program::BranchReceipt>,
    >(report["input_branches"].clone())
    .unwrap_or_default()
    .into_iter()
    .rev()
    .find_map(|receipt| match receipt.action {
        seedbox_engine_protocol::input_program::BranchAction::Reseed { seed, identity } => {
            Some((receipt.at_ns, seed, identity))
        }
        _ => None,
    });
    let mut remaining: std::collections::BTreeSet<_> =
        request.prepared.guests.keys().map(String::as_str).collect();
    for row in report["machines"]
        .as_array()
        .ok_or_else(|| Error::Integrity("missing application machine reports".into()))?
    {
        let name = row["name"]
            .as_str()
            .ok_or_else(|| Error::Integrity("invalid application machine name".into()))?;
        if !remaining.remove(name) {
            return Err(Error::Integrity(
                "duplicate or unknown application machine".into(),
            ));
        }
        let memory = u64::from(request.prepared.memory_mb(name)) * 1024 * 1024;
        let initrd_digest = request
            .prepared
            .guests
            .get(name)
            .ok_or_else(|| Error::Integrity("unknown application machine".into()))?;
        let initrd = fs::read(
            request
                .artifacts
                .get(initrd_digest)
                .ok_or_else(|| Error::Integrity("missing retained ARM guest".into()))?,
        )?;
        let evidence: seedbox_arm64::application::Evidence =
            serde_json::from_value(row["application_evidence"].clone())?;
        let incarnation = row["incarnation"]
            .as_u64()
            .ok_or_else(|| Error::Integrity("missing application incarnation".into()))?;
        let domain = if incarnation == 0 {
            format!("boot-entropy/{name}")
        } else {
            format!("reboot-entropy/{name}/{incarnation}")
        };
        let booted_after_reseed = incarnation > 0
            && reseed.as_ref().is_some_and(|(at, _, _)| {
                row["crashed_incarnations"]
                    .as_array()
                    .and_then(|crashes| crashes.last())
                    .and_then(|crash| crash["crashed_at_ns"].as_u64())
                    .is_some_and(|crashed| crashed >= *at)
            });
        let seed = match &reseed {
            Some((_, seed, identity)) if booted_after_reseed => {
                seedbox_arm64::application::world::entropy_seed(
                    *seed,
                    &format!("checkpoint/{identity}/{domain}"),
                )
            }
            _ => seedbox_arm64::application::world::entropy_seed(request.seed, &domain),
        };
        let mut cmdline = seedbox_arm64::boot::DEFAULT_COMMAND_LINE.to_owned();
        if request.control.is_some_and(|control| {
            request
                .prepared
                .placement
                .get(&control.service)
                .map(String::as_str)
                == Some(name)
        }) {
            cmdline.push_str(" seedbox.control=1");
        }
        if launch_skew.is_some() && incarnation == 0 {
            cmdline.push_str(" seedbox.barrier=1");
        }
        let spec = BootSpec::for_application(&kernel, &initrd, seed, memory, &cmdline)
            .map_err(Error::Integrity)?;
        let timer_domain = if incarnation == 0 {
            format!("timer-jitter/{name}")
        } else {
            format!("timer-jitter/{name}/{incarnation}")
        };
        let timer_policy = match &reseed {
            Some((_, seed, identity)) => seedbox_arm64::application::TimerPolicy {
                seed: *seed,
                domain: format!("checkpoint/{identity}/timer-jitter/{name}/{incarnation}"),
                jitter_ns: request.schedule.timer_jitter_ns,
            },
            None => seedbox_arm64::application::TimerPolicy {
                seed: request.seed,
                domain: timer_domain,
                jitter_ns: request.schedule.timer_jitter_ns,
            },
        };
        for digest in [
            &evidence.witness,
            &evidence.trace_hash,
            &evidence.console_hash,
        ]
        .into_iter()
        .chain(evidence.disk_digest.as_ref())
        {
            ContentDigest::try_from(digest.clone())?;
        }
        match (
            &evidence.state_digest,
            &evidence.state_digest_parts,
            &request.prepared.profile.witness_mode,
        ) {
            (Some(digest), Some(parts), WitnessMode::FullState)
                if parts
                    .keys()
                    .map(String::as_str)
                    .collect::<std::collections::BTreeSet<_>>()
                    == ["boot", "clock", "cpu", "devices", "ram", "uart"]
                        .into_iter()
                        .collect() =>
            {
                for value in parts.values() {
                    ContentDigest::try_from(value.clone())?;
                }
                if ContentDigest::of(&serde_json::to_vec(parts)?).hex() != digest {
                    return Err(Error::Integrity(
                        "application full-state parts hash mismatch".into(),
                    ));
                }
            }
            (None, None, WitnessMode::Exits) => {}
            _ => {
                return Err(Error::Integrity(
                    "application state evidence does not match witness mode".into(),
                ))
            }
        }
        if evidence.format != 1
            || evidence.kernel_sha256 != request.prepared.kernel.hex()
            || evidence.initrd_sha256 != initrd_digest.hex()
            || evidence.boot
                != seedbox_arm64::application::BootIdentity::from_spec(spec)
                    .map_err(Error::Integrity)?
            || evidence.timer_policy != Some(timer_policy)
            || row["time_ns"].as_u64() != Some(evidence.time_ns)
            || row["witness"].as_str() != Some(&evidence.witness)
            || row["state_digest"] != serde_json::to_value(&evidence.state_digest)?
            || row["state_digest_parts"] != serde_json::to_value(&evidence.state_digest_parts)?
            || (evidence.control_channel.is_some()
                && row["control_channel"] != serde_json::to_value(&evidence.control_channel)?)
        {
            return Err(Error::Integrity(
                "HVF application boot/state evidence mismatch".into(),
            ));
        }
    }
    if !remaining.is_empty() {
        return Err(Error::Integrity(
            "application report is missing a prepared machine".into(),
        ));
    }
    Ok(())
}

fn validate_initial_launch<'a>(
    report: &serde_json::Value,
    skew: Option<u64>,
    seed: u64,
    duration: u64,
    names: impl Iterator<Item = &'a str>,
) -> Result<()> {
    let bad = || Error::Integrity("HVF initial launch evidence mismatch".into());
    let Some(skew) = skew else {
        return if report.get("initial_launch").is_none() {
            Ok(())
        } else {
            Err(bad())
        };
    };
    let launch = &report["initial_launch"];
    if launch["max_skew_ns"].as_u64() != Some(skew) {
        return Err(bad());
    }
    let times = launch["release_times"].as_object().ok_or_else(bad)?;
    if launch["released"].as_bool() == Some(false) {
        if !launch["release_ns"].is_null()
            || !times.is_empty()
            || (!launch["barrier_ns"].is_null()
                && launch["barrier_ns"].as_u64().is_none_or(|at| at > duration))
            || !matches!(
                report["outcome"].as_str(),
                Some("duration" | "end_machine_exited" | "fault_failed" | "quiescent")
            )
        {
            return Err(bad());
        }
        return Ok(());
    }
    if launch["released"].as_bool() != Some(true) {
        return Err(bad());
    }
    let barrier = launch["barrier_ns"].as_u64().ok_or_else(bad)?;
    let release = barrier
        .checked_add(skew)
        .filter(|at| *at <= duration)
        .ok_or_else(bad)?;
    if launch["release_ns"].as_u64() != Some(release) {
        return Err(bad());
    }
    let mut count = 0;
    for name in names {
        let mut rng = seedbox_engine_core::rng::Rng::new(seed, &format!("release/{name}"));
        if times.get(name).and_then(serde_json::Value::as_u64)
            != Some(barrier + rng.below(skew + 1))
        {
            return Err(bad());
        }
        count += 1;
    }
    if times.len() != count {
        return Err(bad());
    }
    Ok(())
}

#[cfg(test)]
mod process_tests {
    use super::*;
    fn profile() -> ExecutionProfile {
        ExecutionProfile {
            backend: crate::execution::Backend::Hvf,
            witness_mode: WitnessMode::FullState,
            semantic_parameters: SemanticParameters {
                hvf: Some(HvfParameters {
                    boot_policy: BOOT_POLICY.into(),
                    entropy_policy: ENTROPY_POLICY.into(),
                    device_policy: DEVICE_POLICY.into(),
                    host_release: "test-release".into(),
                    cpu_features: ContentDigest::of(b"hvf_cpu_features").hex().into(),
                    guest_kernel: ContentDigest::of(b"hvf_guest_kernel"),
                    launch_max_skew_ns: crate::engine::ARM_LAUNCH_MAX_SKEW_NS,
                }),
                ..SemanticParameters::new(512)
            },
        }
    }
    #[test]
    fn preflight_workers_admits_the_linux_range_without_core_reservation() {
        let executor = MacExecutor {
            engine: Engine {
                executable: "seedbox".into(),
                revision: None,
            },
            profile: profile(),
            performance: BTreeMap::new(),
        };
        for workers in [0, 33] {
            assert_eq!(
                executor.preflight_workers(workers).unwrap_err().to_string(),
                "--workers: use 1..32"
            );
        }
        // In range, admission is the ordinary HVF probe; a host without HVF
        // access reports that, never a worker-count refusal.
        for workers in [1, 2, 32] {
            match executor.preflight_workers(workers) {
                Ok(()) => {}
                Err(error) => assert!(!error.to_string().contains("--workers"), "{error}"),
            }
        }
    }
    #[test]
    fn launch_report_binds_seeded_offsets_and_preserves_short_timeouts() {
        assert_eq!(
            crate::engine::ARM_LAUNCH_MAX_SKEW_NS,
            seedbox_arm64::application::world::MAX_LAUNCH_SKEW_NS
        );
        let names = ["client", "server"];
        let mut times = serde_json::Map::new();
        for name in names {
            let mut rng = seedbox_engine_core::rng::Rng::new(1009, &format!("release/{name}"));
            times.insert(name.into(), serde_json::json!(100 + rng.below(801)));
        }
        let mut report = serde_json::json!({"initial_launch": {
            "policy": seedbox_arm64::application::world::LAUNCH_POLICY, "max_skew_ns": 800,
            "released": true, "barrier_ns":100, "release_ns":900, "release_times":times,
        }});
        report["initial_launch"]["policy"] = serde_json::json!("descriptive-development-marker");
        validate_initial_launch(&report, Some(800), 1009, 1000, names.into_iter()).unwrap();
        assert!(
            validate_initial_launch(&report, Some(800), 1010, 1000, names.into_iter()).is_err()
        );
        assert!(validate_initial_launch(&report, Some(800), 1009, 899, names.into_iter()).is_err());
        assert!(validate_initial_launch(&report, None, 1009, 1000, names.into_iter()).is_err());
        report["initial_launch"]["release_times"]["client"] = serde_json::json!(0);
        assert!(
            validate_initial_launch(&report, Some(800), 1009, 1000, names.into_iter()).is_err()
        );
        let mut timeout = serde_json::json!({"outcome":"duration", "initial_launch": {
            "policy": seedbox_arm64::application::world::LAUNCH_POLICY, "max_skew_ns": 800,
            "released": false, "barrier_ns":null, "release_ns":null, "release_times":{},
        }});
        validate_initial_launch(&timeout, Some(800), 1009, 1, names.into_iter()).unwrap();
        timeout["initial_launch"]["release_ns"] = serde_json::json!(800);
        assert!(validate_initial_launch(&timeout, Some(800), 1009, 1, names.into_iter()).is_err());
        validate_initial_launch(&serde_json::json!({}), None, 1009, 1, names.into_iter()).unwrap();
    }
    #[test]
    fn retained_application_validates_required_inputs_and_ignores_host_metadata() {
        let complete = profile();
        validate_profile(&complete).expect("complete profile");
        let mut descriptive = complete.clone();
        let hvf = descriptive.semantic_parameters.hvf.as_mut().unwrap();
        for value in [
            &mut hvf.host_release,
            &mut hvf.cpu_features,
            &mut hvf.boot_policy,
            &mut hvf.entropy_policy,
            &mut hvf.device_policy,
        ] {
            *value = "unknown-development-value".into();
        }
        validate_profile(&descriptive).unwrap();
        let mut large = complete.clone();
        large.semantic_parameters.memory_mb = 4096;
        let mut kvm = complete.clone();
        kvm.backend = crate::execution::Backend::Kvm;
        let mut missing = complete.clone();
        missing.semantic_parameters.hvf = None;
        for wrong in [large, kvm, missing] {
            assert!(validate_profile(&wrong).is_err(), "{wrong:?}");
        }
        let mut json = serde_json::to_value(&complete).unwrap();
        for (key, value) in [
            ("memory_mb", serde_json::json!("NaN")),
            ("guest_abi", serde_json::json!("unknown")),
        ] {
            json["semantic_parameters"][key] = value;
            assert!(serde_json::from_value::<ExecutionProfile>(json.clone()).is_err());
        }
        let mut json = serde_json::to_value(&complete).unwrap();
        json["semantic_parameters"]["hvf"]["guest_kernel"] = "not-a-hash".into();
        assert!(serde_json::from_value::<ExecutionProfile>(json).is_err());
    }
}
