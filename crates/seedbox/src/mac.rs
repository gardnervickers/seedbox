//! Native HVF application admission and execution. This profile has its own PV
//! identity and never inherits the x86 host policy or fixture qualifications.
use crate::{
    execution::*,
    store::{ContentDigest, ContentStore},
    Error, Result,
};
use seedbox_arm64::{
    backend::{hvf::Hvf, Backend, SysReg},
    boot::BootSpec,
};
use std::{collections::BTreeMap, fs, path::Path};

const POLICY: &str = "seedbox-hvf-application-v1";
const TIMEBASE: &str = "arm64-pv-world-v1";

#[derive(Clone, Debug)]
pub struct MacExecutor {
    profile: ExecutionProfile,
    performance: BTreeMap<String, String>,
}
#[derive(Debug, serde::Serialize)]
pub struct DoctorReport {
    pub policy_version: String,
    pub engine: ContentDigest,
    pub kernel: ContentDigest,
    pub simulator_version: String,
    pub observations: BTreeMap<String, String>,
    pub engine_qualification: String,
}

fn observe() -> Result<BTreeMap<String, String>> {
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
        ("hvf_host_policy".into(), POLICY.into()),
        ("hvf_host_release".into(), release.into()),
        (
            "hvf_cpu_features".into(),
            ContentDigest::of(&serde_json::to_vec(&features)?)
                .hex()
                .into(),
        ),
    ]))
}
impl MacExecutor {
    pub fn new(
        engine: &Path,
        kernel: &Path,
        store: &ContentStore,
        memory_mb: u32,
        witness_mode: WitnessMode,
        torn_writes: bool,
    ) -> Result<Self> {
        if !(64..=3072).contains(&memory_mb) {
            return Err(Error::Setup(
                "--memory-mb: ARM guests require 64..3072 MiB".into(),
            ));
        }
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
                "--kernel {}: {e}; provide an ARM64 Image that fits --memory-mb",
                kernel.display()
            ))
        })?;
        let kernel = crate::admission::file_digest(kernel)?;
        let mut parameters = crate::engine::arm_parameters();
        parameters.extend(observe()?);
        parameters.extend(BTreeMap::from([
            ("guest_abi".into(), "oci-init-v1".into()),
            ("control_channel".into(), "virtio-console-v1".into()),
            ("memory_mb".into(), memory_mb.to_string()),
            (
                "arm_boot_policy".into(),
                "image-reservation/archive-aligned/final-dtb-v1".into(),
            ),
            (
                "arm_entropy_policy".into(),
                "sha256-domain-u64-seed-v1".into(),
            ),
            (
                "arm_device_policy".into(),
                "net-console-optional-block/supervisor-pid1-v1".into(),
            ),
            (
                "engine_qualification".into(),
                crate::admission::DEV_UNPINNED.into(),
            ),
            ("hvf_guest_kernel".into(), kernel.hex().into()),
        ]));
        if torn_writes {
            parameters.insert("disk_loss_sector_bytes".into(), "512".into());
            parameters.insert("disk_loss_survival_probability".into(), "0.5".into());
        }
        Ok(Self {
            profile: ExecutionProfile {
                backend: "seedbox-hvf".into(),
                engine: store.put_file(engine)?,
                witness_mode,
                semantic_parameters: parameters,
            },
            performance: BTreeMap::new(),
        })
    }
    pub fn from_retained(
        engine: &Path,
        store: &ContentStore,
        mut profile: ExecutionProfile,
    ) -> Result<Self> {
        profile.engine = store.put_file(engine)?;
        Ok(Self {
            profile,
            performance: BTreeMap::new(),
        })
    }
    pub fn with_performance(mut self, margin: &str, undo: u64) -> Self {
        self.performance = BTreeMap::from([
            ("SEEDBOX_MARGIN".into(), margin.into()),
            ("SEEDBOX_UNDO".into(), undo.to_string()),
        ]);
        self
    }
    pub fn doctor(engine: &Path, kernel: &Path) -> Result<DoctorReport> {
        Ok(DoctorReport {
            policy_version: POLICY.into(),
            engine: crate::admission::file_digest(engine)?,
            kernel: crate::admission::file_digest(kernel)?,
            simulator_version: crate::engine::SIMULATOR_VERSION.into(),
            observations: observe()?,
            engine_qualification: crate::admission::DEV_UNPINNED.into(),
        })
    }
}
fn validate_profile(profile: &ExecutionProfile) -> Result<()> {
    if profile.backend != "seedbox-hvf"
        || crate::engine::recorded_launch(profile)? != crate::engine::ExecutableKind::ArmWorld
    {
        return Err(Error::Integrity(
            "retained HVF application profile is missing its world descriptor".into(),
        ));
    }
    let p = &profile.semantic_parameters;
    for (key, value) in [
        ("hvf_host_policy", POLICY),
        ("guest_abi", "oci-init-v1"),
        ("control_channel", "virtio-console-v1"),
        (
            "arm_boot_policy",
            "image-reservation/archive-aligned/final-dtb-v1",
        ),
        ("arm_entropy_policy", "sha256-domain-u64-seed-v1"),
        (
            "arm_device_policy",
            "net-console-optional-block/supervisor-pid1-v1",
        ),
        ("engine_qualification", crate::admission::DEV_UNPINNED),
    ] {
        if p.get(key).map(String::as_str) != Some(value) {
            return Err(Error::Integrity(format!(
                "retained HVF profile missing or unsupported {key}"
            )));
        }
    }
    let memory = p
        .get("memory_mb")
        .and_then(|v| v.parse::<u32>().ok())
        .filter(|m| (64..=3072).contains(m));
    if memory.is_none() {
        return Err(Error::Integrity(
            "retained HVF profile has invalid memory_mb".into(),
        ));
    }
    for key in ["hvf_guest_kernel", "hvf_cpu_features"] {
        ContentDigest::try_from(
            p.get(key)
                .ok_or_else(|| Error::Integrity(format!("retained HVF profile missing {key}")))?
                .clone(),
        )?;
    }
    if p.get("hvf_host_release").is_none_or(|v| v.is_empty()) {
        return Err(Error::Integrity(
            "retained HVF profile missing host release".into(),
        ));
    }
    crate::engine::validate_host(profile)
}
impl ExecutionBackend for MacExecutor {
    fn profile(&self) -> ExecutionProfile {
        self.profile.clone()
    }
    fn performance_parameters(&self) -> BTreeMap<String, String> {
        self.performance.clone()
    }
    fn preflight(&self, output: Option<&Path>) -> Result<()> {
        validate_profile(&self.profile)?;
        let observations = observe()?;
        for (key, value) in &observations {
            if self.profile.semantic_parameters.get(key) != Some(value) {
                return Err(Error::Integrity(format!(
                    "HVF host differs from retained {key}"
                )));
            }
        }
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
            .get("hvf_guest_kernel")
            .map(String::as_str)
            != Some(inputs.kernel.hex())
        {
            return Err(Error::Integrity(
                "retained HVF kernel does not match its application policy".into(),
            ));
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
            return Ok(incomplete(StopReason::Cancelled, Timebase::Pv));
        }
        WorkerLaunch::prepare(&request)?.spawn()?.wait(
            request.cancellation,
            request.limits.wall_time_ns,
            |bytes| crate::vmm::report_hvf(bytes, &request),
        )
    }
}

pub(crate) fn validate_report(bytes: &[u8], request: &ExecutionRequest<'_>) -> Result<()> {
    let report: serde_json::Value = serde_json::from_slice(bytes)?;
    if report["timebase"] != TIMEBASE {
        return Err(Error::Integrity(
            "HVF report missing application timebase".into(),
        ));
    }
    let kernel_path = request
        .artifacts
        .get(&request.prepared.kernel)
        .ok_or_else(|| Error::Integrity("missing retained kernel".into()))?;
    let kernel = fs::read(kernel_path)?;
    let memory = request.prepared.profile.semantic_parameters["memory_mb"]
        .parse::<u64>()
        .map_err(|_| Error::Integrity("invalid retained ARM memory".into()))?
        * 1024
        * 1024;
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
        let seed = seedbox_arm64::application::world::entropy_seed(request.seed, &domain);
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
        let spec = BootSpec::for_application(&kernel, &initrd, seed, memory, &cmdline)
            .map_err(Error::Integrity)?;
        let timer_domain = if incarnation == 0 {
            format!("timer-jitter/{name}")
        } else {
            format!("timer-jitter/{name}/{incarnation}")
        };
        let timer_policy = seedbox_arm64::application::TimerPolicy {
            seed: request.seed,
            domain: timer_domain,
            jitter_ns: request.schedule.timer_jitter_ns,
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

#[cfg(test)]
mod process_tests {
    use super::*;
    fn profile() -> ExecutionProfile {
        let mut parameters = crate::engine::arm_parameters();
        for (name, value) in [
            ("hvf_host_policy", POLICY),
            ("hvf_host_release", "test-release"),
            ("guest_abi", "oci-init-v1"),
            ("control_channel", "virtio-console-v1"),
            ("memory_mb", "512"),
            (
                "arm_boot_policy",
                "image-reservation/archive-aligned/final-dtb-v1",
            ),
            ("arm_entropy_policy", "sha256-domain-u64-seed-v1"),
            (
                "arm_device_policy",
                "net-console-optional-block/supervisor-pid1-v1",
            ),
            ("engine_qualification", crate::admission::DEV_UNPINNED),
        ] {
            parameters.insert(name.into(), value.into());
        }
        for name in ["hvf_cpu_features", "hvf_guest_kernel"] {
            parameters.insert(name.into(), ContentDigest::of(name.as_bytes()).hex().into());
        }
        ExecutionProfile {
            backend: "seedbox-hvf".into(),
            engine: ContentDigest::of(b"engine"),
            witness_mode: WitnessMode::FullState,
            semantic_parameters: parameters,
        }
    }
    #[test]
    fn retained_application_requires_complete_proof_and_cannot_use_fixture_identity() {
        let complete = profile();
        validate_profile(&complete).expect("complete profile");
        for key in [
            "hvf_host_policy",
            "hvf_guest_kernel",
            "hvf_host_release",
            "hvf_cpu_features",
            "arm_boot_policy",
            "arm_entropy_policy",
            "arm_device_policy",
            "engine_qualification",
        ] {
            let mut partial = complete.clone();
            partial.semantic_parameters.remove(key);
            assert!(validate_profile(&partial).is_err(), "{key}");
        }
        for (key, value) in [
            ("timebase", "arm64-pv-fixture-v1"),
            ("guest_arch", "x86_64"),
            ("memory_mb", "4096"),
            ("arm_boot_policy", "historical-fixture"),
        ] {
            let mut wrong = complete.clone();
            wrong.semantic_parameters.insert(key.into(), value.into());
            assert!(validate_profile(&wrong).is_err(), "{key}");
        }
    }
    #[test]
    fn retained_constructor_preserves_missing_proof_for_not_comparable_gate() {
        let temp = tempfile::tempdir().expect("temp");
        let engine = temp.path().join("engine");
        fs::write(&engine, b"new executable").expect("engine");
        let store = ContentStore::new(temp.path().join("objects"));
        let mut previous = profile();
        previous.semantic_parameters.remove("hvf_host_policy");
        let executor =
            MacExecutor::from_retained(&engine, &store, previous.clone()).expect("retain");
        assert_eq!(
            executor.profile.semantic_parameters,
            previous.semantic_parameters
        );
        assert_eq!(
            executor.profile.engine,
            ContentDigest::of(b"new executable")
        );
        assert!(validate_profile(&executor.profile).is_err());
    }
}
