//! Native HVF application admission and execution.
use crate::{
    execution::*,
    store::{ContentDigest, ContentStore},
    Error, Result,
};
use seedbox_arm64::{
    backend::{hvf::Hvf, Backend, SysReg},
    boot::BootSpec,
};
use seedbox_control::Action;
use seedbox_engine_protocol::report::{FaultRecord, RowStatus};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::Path,
};

const BOOT_POLICY: &str = "image-reservation/archive-aligned/final-dtb-v1";
const ENTROPY_POLICY: &str = "sha256-domain-u64-seed-v1";
const DEVICE_POLICY: &str = "net-console-optional-block/supervisor-pid1-v1";

#[derive(Clone, Debug)]
pub struct MacExecutor {
    profile: ExecutionProfile,
    performance: BTreeMap<String, String>,
}
#[derive(Debug, serde::Serialize)]
pub struct DoctorReport {
    pub engine: ContentDigest,
    pub kernel: ContentDigest,
    pub simulator_version: String,
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
            engine: Some(crate::engine::arm_descriptor()),
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
            profile: ExecutionProfile {
                backend: crate::execution::Backend::Hvf,
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
    pub fn doctor(engine: &Path, kernel: &Path) -> Result<DoctorReport> {
        validate_kernel(kernel, 3072)?;
        Ok(DoctorReport {
            engine: crate::admission::file_digest(engine)?,
            kernel: crate::admission::file_digest(kernel)?,
            simulator_version: crate::engine::SIMULATOR_VERSION.into(),
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
    if profile.backend != crate::execution::Backend::Hvf
        || p.launch() != crate::engine::ExecutableKind::ArmWorld
    {
        return Err(Error::Integrity(
            "retained HVF application profile is missing its world descriptor".into(),
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
        WorkerLaunch::prepare(&request)?.spawn()?.wait(
            request.cancellation,
            request.limits.wall_time_ns,
            |bytes| crate::engine_adapter::report_hvf(bytes, &request),
        )
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
    if ContentDigest::of(&kernel) != request.prepared.kernel {
        return Err(Error::Integrity("retained ARM kernel hash mismatch".into()));
    }
    let initial: BTreeSet<_> = request.prepared.guests.keys().map(String::as_str).collect();
    let controller = request.control.and_then(|control| {
        request
            .prepared
            .placement
            .get(&control.service)
            .map(String::as_str)
    });
    let mut boots = BTreeMap::new();
    for (name, digest) in &request.prepared.guests {
        let initrd = fs::read(
            request
                .artifacts
                .get(digest)
                .ok_or_else(|| Error::Integrity("missing retained ARM guest".into()))?,
        )?;
        if ContentDigest::of(&initrd) != *digest {
            return Err(Error::Integrity("retained ARM guest hash mismatch".into()));
        }
        let mut command_line = seedbox_arm64::boot::DEFAULT_COMMAND_LINE.to_owned();
        if controller == Some(name.as_str()) {
            command_line.push_str(" seedbox.control=1");
        }
        boots.insert(
            name.clone(),
            ApplicationBoot {
                initrd,
                command_line,
                memory_mb: request.prepared.memory_mb(name),
                booted_at_ns: None,
            },
        );
    }
    let faults: Vec<FaultRecord> = serde_json::from_value(report["faults"].clone())?;
    let mut remaining = spawned_boots(boots, &faults)?;
    for row in report["machines"]
        .as_array()
        .ok_or_else(|| Error::Integrity("missing application machine reports".into()))?
    {
        let name = row["name"]
            .as_str()
            .ok_or_else(|| Error::Integrity("invalid application machine name".into()))?;
        let boot = remaining
            .remove(name)
            .ok_or_else(|| Error::Integrity("duplicate or unknown application machine".into()))?;
        let memory = u64::from(boot.memory_mb) * 1024 * 1024;
        let initrd_digest = ContentDigest::of(&boot.initrd);
        let evidence: seedbox_arm64::application::Evidence =
            serde_json::from_value(row["application_evidence"].clone())?;
        let incarnation = row["incarnation"]
            .as_u64()
            .ok_or_else(|| Error::Integrity("missing application incarnation".into()))?;
        let domain = if incarnation > 0 {
            format!("reboot-entropy/{name}/{incarnation}")
        } else if initial.contains(name) {
            format!("boot-entropy/{name}")
        } else {
            format!("spawn-entropy/{name}")
        };
        let seed = seedbox_arm64::application::world::entropy_seed(request.seed, &domain);
        let mut cmdline = boot.command_line;
        if launch_skew.is_some() && incarnation == 0 && initial.contains(name) {
            cmdline.push_str(" seedbox.barrier=1");
        }
        let spec = BootSpec::for_application(&kernel, &boot.initrd, seed, memory, &cmdline)
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
            || (incarnation == 0
                && boot
                    .booted_at_ns
                    .is_some_and(|at| row["booted_at_ns"].as_u64() != Some(at)))
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
            "application report is missing a prepared or spawned machine".into(),
        ));
    }
    Ok(())
}

struct ApplicationBoot {
    initrd: Vec<u8>,
    command_line: String,
    /// A spawned machine has its source's memory.
    memory_mb: u32,
    booted_at_ns: Option<u64>,
}

/// Spawn faults are the only authority for new machines. Construct their boot
/// inputs in applied fault order: a child may inherit an earlier child's recipe.
fn spawned_boots(
    mut boots: BTreeMap<String, ApplicationBoot>,
    faults: &[FaultRecord],
) -> Result<BTreeMap<String, ApplicationBoot>> {
    let bad = || Error::Integrity("invalid application spawn evidence".into());
    let mut names: Vec<String> = boots.keys().cloned().collect();
    // Fault IDs are allocated in admission order; applied times can be equal,
    // but a source must already exist when its spawn applies.
    let mut applied: Vec<_> = faults
        .iter()
        .filter(|fault| {
            matches!(&fault.request.action, Action::Spawn { .. })
                && fault.status == RowStatus::Applied
        })
        .collect();
    applied.sort_by_key(|fault| (fault.applied_at_ns, fault.id));
    for fault in applied {
        let Action::Spawn {
            machine,
            from,
            env,
            argv,
        } = &fault.request.action
        else {
            return Err(bad());
        };
        let at = fault.applied_at_ns.ok_or_else(bad)?;
        let index = names.len();
        if fault.action != "spawn"
            || fault.machine.as_deref() != Some(machine)
            || at < fault.request.at_ns
            || index >= seedbox_engine_core::faults::MAX_MACHINES
            || boots.contains_key(machine)
        {
            return Err(bad());
        }
        let source = boots.get(from).ok_or_else(bad)?;
        let memory_mb = source.memory_mb;
        names.push(machine.clone());
        let mut mac = [2, 0, 0, 0, 0, 0];
        mac[5] = (index + 1) as u8;
        let address = seedbox_engine_core::world::machine_address(index);
        let spec = seedbox_engine_core::world::SpawnSpec {
            name: machine,
            index,
            mac,
            address,
            names: &names,
            env,
            argv: argv.as_deref(),
        };
        let initrd = seedbox_engine_core::spawn_recipe::spawned_initrd(&source.initrd, &spec)
            .map_err(Error::Integrity)?;
        let mut command_line = source
            .command_line
            .split_whitespace()
            .filter(|part| {
                *part != "seedbox.control=1"
                    && !part.starts_with("seedbox.index=")
                    && !part.starts_with("seedbox.name=")
            })
            .collect::<Vec<_>>()
            .join(" ");
        command_line.push_str(&format!(" seedbox.index={index} seedbox.name={machine}"));
        boots.insert(
            machine.clone(),
            ApplicationBoot {
                initrd,
                command_line,
                memory_mb,
                booted_at_ns: Some(at),
            },
        );
    }
    Ok(boots)
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
            engine: ContentDigest::of(b"engine"),
            witness_mode: WitnessMode::FullState,
            semantic_parameters: SemanticParameters {
                engine: Some(crate::engine::arm_descriptor()),
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
    fn initrd() -> Vec<u8> {
        use seedbox_engine_core::spawn_recipe::{entry, Recipe};
        let recipe = Recipe {
            ip: b"10.0.0.1/24".to_vec(),
            hosts: b"10.0.0.1 source\n".to_vec(),
            hostname: b"source".to_vec(),
            user: b"0:0".to_vec(),
            working_dir: b"/".to_vec(),
            max_map_count: 262_144,
            wall_clock_epoch: 1_700_000_000,
            instructions_per_progress_call: 50000,
            progress_points_per_block: 1,
            argv: vec![b"/bin/source".to_vec()],
            environment: vec![b"ROLE=source".to_vec()],
            volumes: vec![],
            disk_volumes: vec![],
            configs: vec![],
        };
        let mut archive = Vec::new();
        entry(
            &mut archive,
            "seedbox/launch.bin",
            1,
            0o100400,
            &recipe.encode(),
        );
        entry(&mut archive, "TRAILER!!!", 2, 0, &[]);
        archive
    }

    fn spawn(id: u32, at: u64, machine: &str, from: &str, env: &[(&str, &str)]) -> FaultRecord {
        let env: BTreeMap<String, String> = env
            .iter()
            .map(|(key, value)| ((*key).into(), (*value).into()))
            .collect();
        serde_json::from_value(serde_json::json!({
            "action": "spawn", "applied_at_ns": at,
            "detail": format!("machine {machine} spawned"), "id": id,
            "machine": machine,
            "request": {"id":id,"at_ns":at,"deadline_ns":at + 1,
                        "action":"spawn","machine":machine,"from":from,"env":env},
            "requested_at_ns":at,"status":"applied"
        }))
        .unwrap()
    }

    #[test]
    fn spawned_boots_inherit_recursive_recipe_and_reject_unapplied_or_unknown_sources() {
        use seedbox_engine_core::spawn_recipe::{find, Recipe};
        let base = initrd();
        let boots = || {
            BTreeMap::from([(
                "source".into(),
                ApplicationBoot {
                    initrd: base.clone(),
                    command_line: "console=ttyAMA0 seedbox.control=1".into(),
                    memory_mb: 128,
                    booted_at_ns: None,
                },
            )])
        };
        let child = spawn(1, 100, "child", "source", &[("ROLE", "child")]);
        let grandchild = spawn(2, 100, "grandchild", "child", &[("EXTRA", "yes")]);
        let inherited = spawned_boots(boots(), &[grandchild.clone(), child.clone()]).unwrap();
        assert_eq!(inherited["child"].booted_at_ns, Some(100));
        assert_eq!(inherited["grandchild"].booted_at_ns, Some(100));
        assert_eq!(
            inherited["grandchild"].command_line,
            "console=ttyAMA0 seedbox.index=2 seedbox.name=grandchild"
        );
        let recipe =
            Recipe::parse(find(&inherited["grandchild"].initrd, "seedbox/launch.bin").unwrap())
                .unwrap();
        assert_eq!(recipe.ip, b"10.0.0.3/24");
        assert_eq!(recipe.hostname, b"grandchild");
        assert_eq!(
            recipe.environment,
            [b"ROLE=child".to_vec(), b"EXTRA=yes".to_vec()]
        );
        assert_eq!(
            recipe.hosts,
            b"10.0.0.1 source\n10.0.0.2 child\n10.0.0.3 grandchild\n"
        );
        let mut unapplied = child.clone();
        unapplied.status = RowStatus::Unrequested;
        unapplied.applied_at_ns = None;
        assert_eq!(spawned_boots(boots(), &[unapplied]).unwrap().len(), 1);
        assert!(spawned_boots(boots(), &[grandchild]).is_err());
        assert!(spawned_boots(boots(), &[child.clone(), child]).is_err());
        assert!(spawned_boots(boots(), &[spawn(1, 100, "source", "source", &[])]).is_err());
    }

    #[test]
    fn application_report_validates_spawned_boot_identity_and_seed_domains() {
        let temp = tempfile::tempdir().unwrap();
        let mut kernel = vec![0; 4096];
        kernel[8..16].copy_from_slice(&0x80000u64.to_le_bytes());
        kernel[16..24].copy_from_slice(&8192u64.to_le_bytes());
        kernel[56..60].copy_from_slice(b"ARM\x64");
        let source_initrd = initrd();
        let kernel_hash = ContentDigest::of(&kernel);
        let source_hash = ContentDigest::of(&source_initrd);
        let kernel_path = temp.path().join("kernel");
        let source_path = temp.path().join("source");
        fs::write(&kernel_path, &kernel).unwrap();
        fs::write(&source_path, &source_initrd).unwrap();
        let artifacts = BTreeMap::from([
            (kernel_hash.clone(), kernel_path),
            (source_hash.clone(), source_path),
        ]);
        let mut profile = profile();
        profile.witness_mode = WitnessMode::Exits;
        let prepared = PreparedInputs {
            wall_clock_epoch: None,
            kernel: kernel_hash.clone(),
            images: BTreeMap::new(),
            placement: BTreeMap::from([("oracle".into(), "source".into())]),
            guests: BTreeMap::from([("source".into(), source_hash)]),
            machine_memory_mb: BTreeMap::new(),
            profile,
            auxiliary: BTreeMap::new(),
            provenance: BTreeMap::new(),
        };
        let limits = crate::config::Limits {
            simulated_time_ns: 10_000_000,
            wall_time_ns: 20_000_000,
        };
        let network = crate::config::Network {
            links: vec![],
            name: "private".into(),
            latency_ns: 100_000,
            jitter_ns: 20_000,
            bandwidth_bps: None,
            queue_bound_ns: None,
        };
        let schedule = crate::config::Schedule::default();
        let check = crate::config::Check {
            service: "oracle".into(),
            exit_code: 0,
        };
        let request = ExecutionRequest {
            start_paused: false,
            resume_checkpoint: None,
            intervention_history: None,
            replay_interventions: None,
            observations: None,
            seed: 1009,
            program: &[],
            limits: &limits,
            network: &network,
            cpus: &BTreeMap::new(),
            schedule: &schedule,
            control: None,
            check: &check,
            prepared: &prepared,
            artifacts: &artifacts,
            output: temp.path(),
            cancellation: &CancellationToken::default(),
            performance_parameters: &BTreeMap::new(),
        };
        let fault = spawn(1, 100, "child", "source", &[("ROLE", "child")]);
        let boots = spawned_boots(
            BTreeMap::from([(
                "source".into(),
                ApplicationBoot {
                    initrd: source_initrd,
                    command_line: seedbox_arm64::boot::DEFAULT_COMMAND_LINE.into(),
                    memory_mb: prepared.memory_mb("source"),
                    booted_at_ns: None,
                },
            )]),
            std::slice::from_ref(&fault),
        )
        .unwrap();
        let row = |name: &str, incarnation: u64| {
            let boot = &boots[name];
            let domain = if incarnation > 0 {
                format!("reboot-entropy/{name}/{incarnation}")
            } else if name == "source" {
                format!("boot-entropy/{name}")
            } else {
                format!("spawn-entropy/{name}")
            };
            let seed = seedbox_arm64::application::world::entropy_seed(request.seed, &domain);
            let mut command_line = boot.command_line.clone();
            if name == "source" && incarnation == 0 {
                command_line.push_str(" seedbox.barrier=1");
            }
            let spec =
                BootSpec::for_application(&kernel, &boot.initrd, seed, 512 << 20, &command_line)
                    .unwrap();
            let digest = ContentDigest::of(b"test-evidence").hex().to_owned();
            let evidence = seedbox_arm64::application::Evidence {
                format: 1,
                boot: seedbox_arm64::application::BootIdentity::from_spec(spec).unwrap(),
                kernel_sha256: kernel_hash.hex().to_owned(),
                initrd_sha256: ContentDigest::of(&boot.initrd).hex().to_owned(),
                timer_policy: Some(seedbox_arm64::application::TimerPolicy {
                    seed: request.seed,
                    domain: if incarnation == 0 {
                        format!("timer-jitter/{name}")
                    } else {
                        format!("timer-jitter/{name}/{incarnation}")
                    },
                    jitter_ns: 0,
                }),
                disk_digest: None,
                control_channel: None,
                time_ns: 1000,
                exits: 10,
                trace_hash: digest.clone(),
                console_hash: digest.clone(),
                witness: digest,
                state_digest: None,
                state_digest_parts: None,
            };
            serde_json::json!({
                "name":name,"incarnation":incarnation,
                "booted_at_ns":boot.booted_at_ns.unwrap_or(0),
                "application_evidence":evidence,
                "time_ns":1000,"witness":evidence.witness,
                "state_digest":null,"state_digest_parts":null,
                "control_channel":null,
            })
        };
        let skew = crate::engine::arm_launch_skew(&request.prepared.profile, 80_000).unwrap();
        let mut rng = seedbox_engine_core::rng::Rng::new(request.seed, "release/source");
        let mut report = serde_json::json!({
            "timebase":crate::engine::ARM_REPORT_TIMEBASE,
            "faults":[fault],
            "machines":[row("source", 0), row("child", 0)],
            "initial_launch": {
                "max_skew_ns":skew, "released":true,
                "barrier_ns":100, "release_ns":100 + skew,
                "release_times":{"source":100 + rng.below(skew + 1)}
            },
        });
        let valid = |report: &serde_json::Value| {
            validate_report(&serde_json::to_vec(report).unwrap(), &request)
        };
        valid(&report).unwrap();
        report["machines"][1] = row("child", 1);
        report["machines"][1]["booted_at_ns"] = 101.into();
        valid(&report).unwrap();
        report["machines"][1]["application_evidence"]["initrd_sha256"] =
            ContentDigest::of(b"unrelated archive").hex().into();
        assert!(valid(&report).is_err());
        report["machines"][1] = row("child", 0);
        report["machines"][1]["application_evidence"]["boot"]["seed"] = 0.into();
        assert!(valid(&report).is_err());
        report["machines"][1] = row("child", 0);
        report["machines"][1]["booted_at_ns"] = 101.into();
        assert!(valid(&report).is_err());
        report["machines"][1] = row("source", 0);
        assert!(valid(&report).is_err());
        report["machines"].as_array_mut().unwrap().pop();
        assert!(valid(&report).is_err());
        report["machines"]
            .as_array_mut()
            .unwrap()
            .push(row("child", 0));
        report["faults"][0]["status"] = "unrequested".into();
        report["faults"][0]["applied_at_ns"] = serde_json::Value::Null;
        assert!(valid(&report).is_err());
    }

    #[test]
    fn preflight_workers_admits_the_linux_range_without_core_reservation() {
        let executor = MacExecutor {
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
    #[test]
    fn retained_constructor_preserves_descriptive_metadata() {
        let temp = tempfile::tempdir().expect("temp");
        let engine = temp.path().join("engine");
        fs::write(&engine, b"new executable").expect("engine");
        let store = ContentStore::new(temp.path().join("objects"));
        let previous = profile();
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
        validate_profile(&executor.profile).unwrap();
    }
}
