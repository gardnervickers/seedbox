use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};

pub type Environment = BTreeMap<String, String>;
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Suite {
    Portable,
    Dynamorio,
    DynamorioIsolated,
    MachineTcg,
    MachinePv,
    LinuxOci,
    LinuxApps,
    LinuxPause,
    MacApps,
}
pub const SUITES: [Suite; 9] = [
    Suite::Portable,
    Suite::Dynamorio,
    Suite::DynamorioIsolated,
    Suite::MachineTcg,
    Suite::MachinePv,
    Suite::LinuxOci,
    Suite::LinuxApps,
    Suite::LinuxPause,
    Suite::MacApps,
];
impl Suite {
    pub fn name(self) -> &'static str {
        match self {
            Self::Portable => "portable",
            Self::Dynamorio => "dynamorio",
            Self::DynamorioIsolated => "dynamorio-isolated",
            Self::MachineTcg => "machine-tcg",
            Self::MachinePv => "machine-pv-x86",
            Self::LinuxOci => "linux-oci",
            Self::LinuxApps => "linux-apps",
            Self::LinuxPause => "linux-pause",
            Self::MacApps => "mac-apps",
        }
    }
    pub fn description(self) -> &'static str {
        match self {
        Self::DynamorioIsolated=>"Linux x86-64/ARM64, prepared DR fixtures, user/PID namespaces and owned cgroup-v2 pids delegation.",
        Self::Dynamorio=>"Linux x86-64/ARM64, explicit release/debug DR fixtures; no KVM, root or engine.",
        Self::Portable=>"Any host: fmt, clippy, discovered portable tests and workload validation.",
        Self::MachineTcg=>"Linux x86-64, explicit machineTcgAssets/test fixtures and patched QEMU; no KVM.",
        Self::MachinePv=>"Linux x86-64, KVM, native PV worker and explicit machinePvAssets/test fixtures.",
        Self::LinuxOci=>"Linux x86-64 with a Docker-compatible engine; no KVM required.",
        Self::LinuxApps=>"Linux x86-64 KVM, root, engine and Nix guest builds; server application acceptance.",
        Self::LinuxPause=>"Linux x86-64 KVM, root, engine and guest builds; server pause/resume rerun.",
        Self::MacApps=>"Apple Silicon macOS 15+, Hypervisor.framework, codesign, engine and guest builds; server application acceptance."
    }
    }
}
pub fn expand(names: &[String]) -> Result<Vec<Suite>, String> {
    let mut chosen = BTreeSet::from([Suite::Portable]);
    for name in names {
        match name.as_str() {
            "linux" => chosen.extend([Suite::LinuxOci, Suite::LinuxApps]),
            "mac" => chosen.extend([Suite::MacApps]),
            _ => {
                chosen.insert(
                    SUITES
                        .iter()
                        .copied()
                        .find(|s| s.name() == name)
                        .ok_or_else(|| format!("unknown suite: {name}; see --help"))?,
                );
            }
        }
    }
    Ok(SUITES.into_iter().filter(|s| chosen.contains(s)).collect())
}
#[derive(Clone, Debug)]
pub struct Host {
    pub system: String,
    pub machine: String,
    pub tools: BTreeSet<String>,
    pub sockets: Vec<(PathBuf, bool)>,
    pub kvm: Option<String>,
    pub root_prefix: Option<Vec<String>>,
    pub mac_major: u32,
    pub hvf: bool,
}
impl Host {
    pub fn local() -> Self {
        Self {
            system: if cfg!(target_os = "macos") {
                "Darwin"
            } else if cfg!(target_os = "linux") {
                "Linux"
            } else {
                std::env::consts::OS
            }
            .into(),
            machine: if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
                "arm64"
            } else {
                std::env::consts::ARCH
            }
            .into(),
            tools: BTreeSet::new(),
            sockets: Vec::new(),
            kvm: None,
            root_prefix: None,
            mac_major: 0,
            hvf: false,
        }
    }
    pub fn arm(&self) -> bool {
        matches!(self.machine.as_str(), "arm64" | "aarch64")
    }
    pub fn engine(&self, env: &Environment) -> bool {
        env.get("DOCKER_HOST").is_some_and(|v| !v.is_empty())
            || self.tools.contains("docker")
            || self.tools.contains("podman")
            || !self.sockets.is_empty()
    }
    pub fn engine_needs_root(&self, env: &Environment) -> bool {
        let sockets: Vec<_> = if let Some(host) = env.get("DOCKER_HOST").filter(|v| !v.is_empty()) {
            if let Some(path) = host.strip_prefix("unix://") {
                self.sockets
                    .iter()
                    .filter(|(p, _)| p == Path::new(path))
                    .collect()
            } else {
                Vec::new()
            }
        } else {
            self.sockets.iter().collect()
        };
        !sockets.is_empty() && !sockets.iter().any(|(_, access)| *access)
    }
}
pub fn socket_candidates(env: &Environment) -> Vec<PathBuf> {
    let mut paths = vec![
        PathBuf::from("/var/run/docker.sock"),
        PathBuf::from(env.get("HOME").map(String::as_str).unwrap_or("~"))
            .join(".docker/run/docker.sock"),
        PathBuf::from("/run/podman/podman.sock"),
    ];
    if let Some(dir) = env.get("XDG_RUNTIME_DIR") {
        paths.push(Path::new(dir).join("podman/podman.sock"));
    }
    paths
}
pub fn executable(path: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt;
    path.is_file()
        && std::ffi::CString::new(path.as_os_str().as_bytes())
            .is_ok_and(|p| unsafe { libc::access(p.as_ptr(), libc::X_OK) == 0 })
}
pub fn build_requirements(host: &Host, env: &Environment) -> Vec<String> {
    if host.system == "Linux" {
        if host.tools.contains("nix") {
            vec![]
        } else {
            vec!["nix is required to build current guest inputs".into()]
        }
    } else {
        let found = env
            .get("SEEDBOX_ASSET_BUILDER")
            .filter(|value| !value.is_empty())
            .map(|s| host.tools.contains(s))
            .unwrap_or_else(|| host.tools.contains("podman") || host.tools.contains("docker"));
        if found {
            vec![]
        } else {
            vec!["Podman or Docker CLI is required to build Mac guest inputs".into()]
        }
    }
}
/// The static x86-64 cpu-loop ELF that both named machine suites run through
/// the public service (`machinePvTestFixtures/bin/cpu-loop`; building it needs
/// no KVM). Backend-neutral: it selects no backend.
pub const CPU_LOOP_FIXTURE: &str = "SEEDBOX_CPU_LOOP_FIXTURE";
/// The normal runtime package the public-service tests submit through, from
/// `cargo xtask build-assets`.
pub const SERVICE_RUNTIME: &str = "build/assets/driver-runtime.tar.gz";
/// The feature and `seedbox-server` integration targets of a machine suite's
/// public-service acceptance. These tests are feature-gated, not ignored.
/// The controller target runs the prepared fixture's oracle socket calls on
/// either backend.
/// Other suites have none.
pub fn service_targets(suite: Suite) -> (&'static str, &'static [&'static str]) {
    match suite {
        Suite::MachineTcg => (
            "tcg-tests",
            &["native_tcg_service", "native_controller_service"],
        ),
        Suite::MachinePv => (
            "kvm-tests",
            &["native_pv_service", "native_controller_service"],
        ),
        _ => ("", &[]),
    }
}
pub fn machine_inputs(suite: Suite) -> Vec<String> {
    let backend = if suite == Suite::MachineTcg {
        "TCG"
    } else {
        "PV"
    };
    let mut suffixes = vec![
        "ASSETS",
        "SERVICE_INITRD",
        "CONTROL_INITRD",
        "DRIVER_CONTROL_APPLICATION",
        "PREPARED_FIXTURE",
        "CLOCK_APPLICATION",
        "TERMINAL_APPLICATION",
        "NETWORK_APPLICATION",
    ];
    suffixes.extend(if suite == Suite::MachineTcg {
        vec![
            "QEMU",
            "FIRMWARE",
            "KERNEL",
            "INIT",
            "DISK_PROBE",
            "DISK_IMAGE",
        ]
    } else {
        vec![
            "WORKER",
            "KERNEL",
            "INIT",
            "PROGRESS_RUNTIME",
            "PROGRESS_FIXTURE",
            "CLANG",
            "LD",
            "DISK_PROBE",
            "DISK_IMAGE",
        ]
    });
    suffixes
        .into_iter()
        .map(|s| format!("SEEDBOX_{backend}_{s}"))
        .chain([CPU_LOOP_FIXTURE.to_string()])
        .collect()
}
pub fn requirements(suite: Suite, host: &Host, env: &Environment, root: &Path) -> Vec<String> {
    let mut missing = Vec::new();
    let mut need = |condition: bool, reason: String| {
        if !condition {
            missing.push(reason);
        }
    };
    let linux = host.system == "Linux";
    let x86 = linux && host.machine == "x86_64";
    let kvm = host.kvm.as_deref() == Some("");
    let kvm_reason = format!(
        "/dev/kvm: {}",
        host.kvm
            .as_deref()
            .filter(|s| !s.is_empty())
            .unwrap_or("not probed")
    );
    if suite == Suite::Portable {
        return missing;
    }
    if matches!(suite, Suite::Dynamorio | Suite::DynamorioIsolated) {
        need(
            linux && (x86 || host.arm()),
            "requires Linux x86-64 or ARM64".into(),
        );
        let fixtures = env.get("SEEDBOX_DYNAMORIO_FIXTURES").map(Path::new);
        for file in [
            "runtime/bin64/drrun",
            "bin/application",
            "lib/libpreserve.so",
        ] {
            need(
                fixtures.is_some_and(|p| {
                    let file = p.join(file);
                    if file.extension().is_some_and(|ext| ext == "so") {
                        file.is_file()
                    } else {
                        executable(&file)
                    }
                }),
                format!(
                    "set SEEDBOX_DYNAMORIO_FIXTURES to dynamorioTestFixtures containing {file}"
                ),
            );
        }
        if suite == Suite::Dynamorio {
            let debug = env.get("SEEDBOX_DYNAMORIO_DEBUG_FIXTURES").map(Path::new);
            for file in [
                "runtime/bin64/drrun",
                "runtime/lib64/debug/libdynamorio.so",
                "bin/unix-supervisor",
                "bin/unix-signal-models-itimer",
            ] {
                need(
                    debug.is_some_and(|root| {
                        let path = root.join(file);
                        if path.extension().is_some_and(|ext| ext == "so") {
                            path.is_file()
                        } else {
                            executable(&path)
                        }
                    }),
                    format!(
                        "set SEEDBOX_DYNAMORIO_DEBUG_FIXTURES to dynamorioDebugTestFixtures containing {file}"
                    ),
                );
            }
            if host.arm() {
                need(
                    fixtures.is_some_and(|root| executable(&root.join("bin/unix-lifecycle_model"))),
                    "prepared ARM bin/unix-lifecycle_model is required".into(),
                );
                need(
                    debug.is_some_and(|root| executable(&root.join("bin/unix-lifecycle_model"))),
                    "prepared ARM Debug bin/unix-lifecycle_model is required".into(),
                );
            }
        }
        if suite == Suite::DynamorioIsolated {
            for name in ["bin/unix-pid1", "bin/unix-eagain", "bin/unix-supervisor"] {
                need(
                    fixtures.is_some_and(|p| executable(&p.join(name))),
                    format!("prepared {name} is required for isolated fixtures"),
                );
            }
            need(
                fixtures.is_some_and(|p| executable(&p.join("bin/seedbox-dr-isolate"))),
                "prepared bin/seedbox-dr-isolate is required".into(),
            );
            need(
                env.get("SEEDBOX_DYNAMORIO_CGROUP_ROOT")
                    .is_some_and(|p| Path::new(p).is_dir()),
                "set SEEDBOX_DYNAMORIO_CGROUP_ROOT to an empty owned cgroup-v2 pids delegation"
                    .into(),
            );
            for mode in ["pid1", "pids"] {
                need(
                    host.tools.contains(&format!("dynamorio-isolate-{mode}")),
                    format!("isolate: {mode} capability preflight did not succeed"),
                );
            }
        }
        return missing;
    }
    if matches!(suite, Suite::MachineTcg | Suite::MachinePv) {
        need(x86, "requires Linux x86-64".into());
        if suite == Suite::MachinePv {
            need(kvm, kvm_reason);
        }
        for key in machine_inputs(suite) {
            let path = env.get(&key).map(Path::new);
            let dir = key.ends_with("_ASSETS");
            let valid = path.is_some_and(|p| if dir { p.is_dir() } else { p.is_file() });
            need(
                valid,
                format!(
                    "set {key} to an existing {}",
                    if dir { "directory" } else { "file" }
                ),
            );
            if valid
                && ["_QEMU", "_WORKER", "_CLANG", "_LD"]
                    .iter()
                    .any(|s| key.ends_with(s))
            {
                need(
                    path.is_some_and(executable),
                    format!("{key} is not executable"),
                );
            }
        }
        need(
            root.join(SERVICE_RUNTIME).is_file(),
            format!("{SERVICE_RUNTIME} is missing; run cargo xtask build-assets"),
        );
        return missing;
    }
    if suite.name().starts_with("linux-") {
        if !linux {
            return vec!["requires Linux".into()];
        }
        need(x86, "requires Linux x86-64".into());
        if suite != Suite::LinuxOci {
            need(kvm, kvm_reason);
        }
        if matches!(suite, Suite::LinuxApps | Suite::LinuxPause) {
            need(
                host.root_prefix.is_some(),
                "requires root or passwordless sudo".into(),
            );
        }
        if suite == Suite::LinuxOci {
            need(
                host.engine(env),
                "no Docker-compatible engine (set DOCKER_HOST, or install Docker or Podman)".into(),
            );
            need(host.root_prefix.is_some()||!host.engine_needs_root(env),"the engine socket is not writable by this user; join its group, or allow passwordless sudo".into());
        }
        if matches!(suite, Suite::LinuxApps | Suite::LinuxPause) {
            need(
                host.engine(env),
                "no Docker-compatible engine (set DOCKER_HOST, or install Docker or Podman)".into(),
            );
        }
    } else {
        if host.system != "Darwin" || !host.arm() {
            return vec!["requires macOS on Apple Silicon".into()];
        }
        need(host.mac_major >= 15, "requires macOS 15+".into());
        need(
            host.hvf,
            "Hypervisor.framework unavailable (sysctl kern.hv_support)".into(),
        );
        need(host.tools.contains("codesign"), "codesign not found".into());
        need(
            host.engine(env),
            "no Docker-compatible engine (set DOCKER_HOST, or install Docker or Podman)".into(),
        );
    }
    if matches!(suite, Suite::LinuxApps | Suite::LinuxPause | Suite::MacApps) {
        missing.extend(build_requirements(host, env));
    }
    missing
}
#[derive(Debug, PartialEq, Eq)]
pub struct SuiteTests {
    pub package: String,
    pub features: Vec<String>,
    pub targets: Vec<String>,
}

// Packages declare suite -> Cargo features in package.metadata.seedbox-check.suites.
// Targets remain Cargo-owned: required-features determines which tests to run.
pub fn suite_test_targets(metadata: &Value, suites: &[Suite]) -> Result<Vec<SuiteTests>, String> {
    let packages = metadata["packages"]
        .as_array()
        .ok_or("Cargo metadata has no packages")?;
    let mut result = Vec::new();
    for package in packages {
        let declarations = &package["metadata"]["seedbox-check"]["suites"];
        if declarations.is_null() {
            continue;
        }
        let name = package["name"]
            .as_str()
            .ok_or("Cargo package has no name")?;
        let declarations = declarations.as_object().ok_or_else(|| {
            format!("{name}: seedbox-check.suites must map suites to feature lists")
        })?;
        let mut features = BTreeSet::new();
        for (suite, declared) in declarations {
            let suite = SUITES
                .iter()
                .find(|s| s.name() == suite)
                .ok_or_else(|| format!("{name}: unknown seedbox-check suite {suite}"))?;
            let declared = declared.as_array().ok_or_else(|| {
                format!("{name}: suite {} must contain a feature list", suite.name())
            })?;
            for feature in declared {
                let feature = feature.as_str().ok_or_else(|| {
                    format!("{name}: suite {} feature must be a string", suite.name())
                })?;
                if package["features"].get(feature).is_none() {
                    return Err(format!(
                        "{name}: suite {} has unknown feature {feature}",
                        suite.name()
                    ));
                }
                if suites.contains(suite) {
                    features.insert(feature.to_owned());
                }
            }
        }
        if features.is_empty() {
            continue;
        }
        let features: Vec<_> = features.into_iter().collect();
        let targets = feature_test_targets(metadata, name, &features)?;
        if targets.is_empty() {
            return Err(format!(
                "{name}: no integration targets require {features:?}"
            ));
        }
        result.push(SuiteTests {
            package: name.into(),
            features,
            targets,
        });
    }
    result.sort_by(|a, b| a.package.cmp(&b.package));
    Ok(result)
}
pub fn feature_test_targets(
    metadata: &Value,
    package: &str,
    features: &[String],
) -> Result<Vec<String>, String> {
    let packages = metadata["packages"]
        .as_array()
        .ok_or("Cargo metadata has no packages")?;
    let targets = packages
        .iter()
        .find(|p| p["name"] == package)
        .and_then(|p| p["targets"].as_array())
        .ok_or_else(|| format!("Cargo metadata has no targets for {package}"))?;
    let mut result = Vec::new();
    for target in targets {
        let needed = target["required-features"].as_array();
        if target["kind"]
            .as_array()
            .is_some_and(|k| k.iter().any(|v| v == "test"))
            && needed.is_some_and(|n| {
                !n.is_empty()
                    && n.iter()
                        .all(|f| features.iter().any(|v| Some(v.as_str()) == f.as_str()))
            })
        {
            result.push(
                target["name"]
                    .as_str()
                    .ok_or("test target has no name")?
                    .into(),
            );
        }
    }
    result.sort();
    result.dedup();
    Ok(result)
}
pub fn test_artifacts(output: &str) -> Result<Vec<(PathBuf, String)>, String> {
    let mut result = BTreeMap::new();
    for line in output.lines().filter(|l| !l.trim().is_empty()) {
        let item: Value =
            serde_json::from_str(line).map_err(|e| format!("Cargo artifact JSON: {e}"))?;
        if item["reason"] == "compiler-artifact" && item["profile"]["test"] == true {
            if let Some(path) = item["executable"].as_str() {
                result.insert(
                    PathBuf::from(path),
                    item["target"]["name"]
                        .as_str()
                        .ok_or("artifact target has no name")?
                        .into(),
                );
            }
        }
    }
    Ok(result.into_iter().collect())
}
pub fn privileged(
    prefix: Option<&[String]>,
    command: Vec<String>,
    env: &Environment,
) -> Vec<String> {
    let Some(prefix) = prefix.filter(|p| !p.is_empty()) else {
        return command;
    };
    let mut result = prefix.to_vec();
    for (key, value) in env {
        if key.starts_with("SEEDBOX_TEST_")
            || [
                "SEEDBOX_MKE2FS",
                "DOCKER_HOST",
                "SEEDBOX_ASSET_DIR",
                "TMPDIR",
            ]
            .contains(&key.as_str())
        {
            result.push(format!("{key}={value}"));
        }
    }
    result.extend(command);
    result
}
pub fn select_build(env: &mut Environment, directory: &Path) -> Result<(), String> {
    if !directory.is_absolute()
        || !["seedbox", "driver-runtime.tar.gz"]
            .iter()
            .all(|n| directory.join(n).is_file())
    {
        return Err("complete build did not return its executable and Driver runtime".into());
    }
    // The complete build embeds its runtime; ambient overrides must not replace it.
    for key in ["SEEDBOX_ASSET_DIR", "SEEDBOX_MKE2FS"] {
        env.remove(key);
    }
    env.insert(
        "SEEDBOX_TEST_CLI".into(),
        directory.join("seedbox").to_string_lossy().into(),
    );
    Ok(())
}
