//! Named suite admission and Cargo execution. Portable tests never lease a VM.
use clap::ValueEnum;
use std::{
    collections::{BTreeMap, BTreeSet},
    env, fs, io,
    os::unix::fs::FileTypeExt,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    time::Instant,
};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, ValueEnum)]
pub enum Suite {
    /// Portable unit and integration tests; no hypervisor or container engine.
    Portable,
    /// Linux native KVM register, entry and restore contracts.
    LinuxKvm,
    /// Linux KVM execution with explicitly supplied fixture kernel and initrd.
    LinuxGuests,
    /// Actual container-engine pull, build and export behavior on Linux x86-64.
    LinuxOci,
    /// Linux x86-64 application determinism (KVM, root and container engine).
    LinuxApps,
    /// Linux x86-64 pause/resume determinism (KVM, root and container engine).
    LinuxPause,
    /// Apple Silicon native Hypervisor.framework contracts (macOS 15+).
    MacHvf,
    /// Apple Silicon execution with explicitly supplied fixture guest inputs.
    MacGuests,
    /// Apple Silicon application determinism with a container engine.
    MacApps,
    /// Linux native, container-engine and application suites.
    Linux,
    /// Apple Silicon native and application suites.
    Mac,
}

impl Suite {
    fn name(self) -> String {
        self.to_possible_value().unwrap().get_name().to_owned()
    }
    fn applications(self) -> bool {
        matches!(self, Self::LinuxApps | Self::LinuxPause | Self::MacApps)
    }
    fn filter(self, arch: &str) -> Option<&'static str> {
        match self {
            Self::LinuxKvm | Self::MacHvf => Some(if arch == "x86_64" {
                "x86::native"
            } else {
                "arm64::native"
            }),
            Self::LinuxGuests | Self::MacGuests => Some(if arch == "x86_64" {
                "x86::guests"
            } else {
                "arm64::guests"
            }),
            Self::LinuxOci => Some("oci::container_engine"),
            Self::LinuxApps | Self::MacApps => Some("applications::applications"),
            Self::LinuxPause => Some("applications::applications"),
            _ => None,
        }
    }
}

#[derive(clap::Args)]
pub struct Args {
    /// Suites or platform groups to run [default: portable].
    #[arg(value_enum)]
    pub suites: Vec<Suite>,
    /// Check selected suite requirements without building tests or guest assets.
    #[arg(long)]
    pub list: bool,
}

fn expand(names: &[Suite]) -> Vec<Suite> {
    let mut chosen = BTreeSet::from([Suite::Portable]);
    let mut pending = names.to_vec();
    while let Some(suite) = pending.pop() {
        if !chosen.insert(suite) {
            continue;
        }
        pending.extend_from_slice(match suite {
            Suite::Linux => &[Suite::LinuxKvm, Suite::LinuxOci, Suite::LinuxApps],
            Suite::Mac => &[Suite::MacHvf, Suite::MacApps],
            Suite::LinuxGuests => &[Suite::LinuxKvm],
            Suite::MacGuests | Suite::MacApps => &[Suite::MacHvf],
            _ => &[],
        });
    }
    chosen.remove(&Suite::Linux);
    chosen.remove(&Suite::Mac);
    chosen.into_iter().collect()
}

#[derive(Default)]
struct Host {
    os: String,
    arch: String,
    kvm: bool,
    kvm_writable: bool,
    hvf: bool,
    codesign: bool,
    root: bool,
    engine: bool,
    engine_writable: bool,
    builder: bool,
    files: BTreeSet<String>,
}

impl Host {
    fn missing(&self, suite: Suite) -> Vec<String> {
        let mut missing = Vec::new();
        let mut need = |condition: bool, reason: &str| {
            if !condition {
                missing.push(reason.to_owned());
            }
        };
        match suite {
            Suite::Portable => return missing,
            Suite::LinuxKvm | Suite::LinuxGuests => {
                need(self.os == "linux", "requires Linux");
                need(
                    matches!(self.arch.as_str(), "x86_64" | "aarch64"),
                    "requires Linux x86-64 or ARM64",
                );
                need(self.kvm, "requires access to /dev/kvm");
            }
            Suite::LinuxOci | Suite::LinuxApps | Suite::LinuxPause => {
                need(
                    self.os == "linux" && self.arch == "x86_64",
                    "requires Linux x86-64",
                );
                if suite != Suite::LinuxOci {
                    need(self.kvm, "requires access to /dev/kvm");
                    need(self.root, "requires root or passwordless sudo");
                }
            }
            Suite::MacHvf | Suite::MacGuests | Suite::MacApps => {
                need(
                    self.os == "macos" && self.arch == "aarch64",
                    "requires macOS on Apple Silicon",
                );
                need(self.hvf, "requires macOS 15+ and Hypervisor.framework");
                need(self.codesign, "codesign not found");
            }
            Suite::Linux | Suite::Mac => unreachable!("groups are expanded"),
        }
        if suite.applications() || suite == Suite::LinuxOci {
            need(
                self.engine,
                "no Docker-compatible engine; start Docker or Podman, or set DOCKER_HOST",
            );
            need(
                self.engine_writable || self.root,
                "engine socket is not writable; join its group or allow passwordless sudo",
            );
        }
        if suite.applications() {
            need(self.builder, "guest asset builder missing; enter nix develop and start the container engine on Mac");
        }
        if matches!(suite, Suite::LinuxGuests | Suite::MacGuests) {
            let keys = if self.arch == "x86_64" {
                ["SEEDBOX_TEST_KERNEL", "SEEDBOX_TEST_INITRD"]
            } else {
                ["SEEDBOX_ARM64_KERNEL", "SEEDBOX_ARM64_INITRD"]
            };
            for key in keys {
                need(
                    self.files.contains(key),
                    &format!("set {key} to an existing guest input"),
                );
            }
        }
        missing
    }
}

fn which(name: &str) -> bool {
    env::var_os("PATH")
        .is_some_and(|paths| env::split_paths(&paths).any(|p| p.join(name).is_file()))
}

const INPUTS: [(&str, &str); 4] = [
    ("SEEDBOX_TEST_KERNEL", "artifacts/bzImage"),
    ("SEEDBOX_TEST_INITRD", "artifacts/entropy-fork.cpio"),
    ("SEEDBOX_ARM64_KERNEL", "artifacts/arm64-hres/Image"),
    ("SEEDBOX_ARM64_INITRD", "artifacts/arm64-hres/init.cpio"),
];

/// Check the real KVM API without creating a VM. Also used under sudo when
/// only the privileged system-test process has device access.
pub fn probe_kvm() -> bool {
    #[cfg(target_os = "linux")]
    {
        use std::os::fd::AsRawFd;
        fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/kvm")
            .is_ok_and(|file| {
                // SAFETY: KVM_GET_API_VERSION takes no argument; file is owned.
                (unsafe { libc::ioctl(file.as_raw_fd(), 0xae00) }) == 12
            })
    }
    #[cfg(not(target_os = "linux"))]
    {
        false
    }
}

fn detect(root: &Path, environment: &mut BTreeMap<String, String>, hardware: bool) -> (Host, bool) {
    // SAFETY: geteuid has no preconditions.
    let is_root = unsafe { libc::geteuid() } == 0;
    let sudo = hardware
        && !is_root
        && which("sudo")
        && Command::new("sudo")
            .args(["-n", "true"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success());
    let mut host = Host {
        os: env::consts::OS.into(),
        arch: env::consts::ARCH.into(),
        codesign: which("codesign"),
        root: is_root || sudo,
        builder: if cfg!(target_os = "linux") {
            which("nix")
        } else {
            environment
                .get("SEEDBOX_ASSET_BUILDER")
                .map_or_else(|| which("docker") || which("podman"), |p| which(p))
        },
        ..Host::default()
    };
    if !hardware {
        return (host, false);
    }
    for (key, default) in INPUTS {
        let path = environment
            .get(key)
            .map(PathBuf::from)
            .unwrap_or_else(|| root.join(default));
        if let Some(path) = path.canonicalize().ok().filter(|p| p.is_file()) {
            host.files.insert(key.into());
            environment.insert(key.into(), path.to_string_lossy().into_owned());
        }
    }
    #[cfg(target_os = "linux")]
    {
        host.kvm_writable = probe_kvm();
        host.kvm = host.kvm_writable;
        // Probe as root only when the selected work needs it and sudo is available.
        if !host.kvm && sudo {
            host.kvm = env::current_exe().is_ok_and(|exe| {
                Command::new("sudo")
                    .arg("-n")
                    .arg(exe)
                    .arg("probe-kvm")
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status()
                    .is_ok_and(|s| s.success())
            });
        }
    }
    #[cfg(target_os = "macos")]
    {
        let version = Command::new("sw_vers")
            .arg("-productVersion")
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .and_then(|s| s.split('.').next()?.parse::<u32>().ok())
            .unwrap_or(0);
        host.hvf = version >= 15
            && Command::new("sysctl")
                .args(["-n", "kern.hv_support"])
                .output()
                .is_ok_and(|o| o.status.success() && o.stdout == b"1\n");
    }
    let mut sockets = vec![
        PathBuf::from("/var/run/docker.sock"),
        PathBuf::from("/run/podman/podman.sock"),
    ];
    if let Some(home) = environment.get("HOME") {
        sockets.push(Path::new(home).join(".docker/run/docker.sock"));
    }
    if let Some(runtime) = environment.get("XDG_RUNTIME_DIR") {
        sockets.push(Path::new(runtime).join("podman/podman.sock"));
    }
    #[cfg(target_os = "macos")]
    if let Ok(output) = Command::new("getconf").arg("DARWIN_USER_TEMP_DIR").output() {
        let directory =
            PathBuf::from(String::from_utf8_lossy(&output.stdout).trim()).join("podman");
        if let Ok(entries) = fs::read_dir(directory) {
            sockets.extend(
                entries
                    .flatten()
                    .map(|e| e.path())
                    .filter(|p| p.extension().is_some_and(|e| e == "sock")),
            );
        }
    }
    if let Some(endpoint) = environment.get("DOCKER_HOST") {
        sockets = endpoint
            .strip_prefix("unix://")
            .map(PathBuf::from)
            .into_iter()
            .collect();
    }
    sockets.retain(|p| fs::metadata(p).is_ok_and(|m| m.file_type().is_socket()));
    host.engine = environment.contains_key("DOCKER_HOST")
        || !sockets.is_empty()
        || which("docker")
        || which("podman");
    host.engine_writable = sockets.is_empty()
        || sockets
            .iter()
            .any(|p| std::os::unix::net::UnixStream::connect(p).is_ok());
    (host, sudo)
}

fn command_line(command: &Command) -> String {
    std::iter::once(command.get_program())
        .chain(command.get_args())
        .map(|argument| format!("{argument:?}"))
        .collect::<Vec<_>>()
        .join(" ")
}

fn invoke(root: &Path, command: &mut Command) -> Result<()> {
    let started = Instant::now();
    let description = command_line(command);
    eprintln!("running: {description}");
    let status = command.current_dir(root).status()?;
    eprintln!("check: {status} ({:.1}s)", started.elapsed().as_secs_f64());
    if !status.success() {
        return Err(format!("{description}: {status}").into());
    }
    Ok(())
}

fn capture(root: &Path, command: &mut Command) -> Result<Output> {
    let description = command_line(command);
    eprintln!("building: {description}");
    let output = command
        .current_dir(root)
        .stderr(Stdio::inherit())
        .output()?;
    if !output.status.success() {
        io::Write::write_all(&mut io::stderr(), &output.stdout)?;
        return Err(format!("{description}: {}", output.status).into());
    }
    Ok(output)
}

fn executables(bytes: &[u8], name: &str, test: bool) -> Result<Vec<PathBuf>> {
    let mut paths = BTreeSet::new();
    for line in bytes.split(|b| *b == b'\n').filter(|l| !l.is_empty()) {
        let message: serde_json::Value = serde_json::from_slice(line)?;
        if message["reason"] == "compiler-artifact"
            && message["target"]["name"] == name
            && message["profile"]["test"] == test
        {
            if let Some(path) = message["executable"].as_str() {
                paths.insert(PathBuf::from(path));
            }
        }
    }
    Ok(paths.into_iter().collect())
}

fn portable(root: &Path) -> Result<()> {
    invoke(
        root,
        Command::new("cargo").args(["fmt", "--all", "--check"]),
    )?;
    invoke(
        root,
        Command::new("cargo").args([
            "clippy",
            "--workspace",
            "--all-targets",
            "--locked",
            "--",
            "-D",
            "warnings",
        ]),
    )?;
    // Includes (ignored) system-test bodies. Nothing opens KVM/HVF here.
    invoke(
        root,
        Command::new("cargo").args(["test", "--workspace", "--locked"]),
    )?;
    invoke(
        root,
        Command::new("cargo")
            .args(["test-arm64"])
            .env_remove("CARGO_TARGET_DIR"),
    )?;
    if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        invoke(
            root,
            Command::new("cargo")
                .args(["test-vmm"])
                .env_remove("CARGO_TARGET_DIR"),
        )?;
    }
    let output = capture(
        root,
        Command::new("git").args([
            "ls-files",
            "--cached",
            "--others",
            "--exclude-standard",
            "-z",
            "*.sh",
            ":!:research/**",
        ]),
    )?;
    use std::os::unix::ffi::OsStrExt;
    for name in output.stdout.split(|b| *b == 0).filter(|n| !n.is_empty()) {
        let path = Path::new(std::ffi::OsStr::from_bytes(name));
        if root.join(path).is_file() {
            invoke(root, Command::new("bash").arg("-n").arg(path))?;
        }
    }
    Ok(())
}

pub fn check(root: &Path, args: Args) -> Result<i32> {
    let suites = expand(&args.suites);
    let hardware = suites.iter().any(|s| *s != Suite::Portable);
    let mut environment: BTreeMap<_, _> = env::vars().collect();
    let (host, sudo) = detect(root, &mut environment, hardware);
    eprintln!("platform: {}/{}", host.os, host.arch);
    let mut unavailable = false;
    for suite in &suites {
        let missing = host.missing(*suite);
        eprintln!(
            "suite {}: {}",
            suite.name(),
            if missing.is_empty() {
                "ready".into()
            } else {
                unavailable = true;
                format!("unavailable: {}", missing.join("; "))
            }
        );
    }
    if unavailable {
        return Ok(2);
    }
    if args.list {
        return Ok(0);
    }
    portable(root)?;
    if !hardware {
        return Ok(0);
    }

    if suites.contains(&Suite::LinuxPause) && !suites.contains(&Suite::LinuxApps) {
        environment.insert("SEEDBOX_TEST_PAUSE_ONLY".into(), "1".into());
    } else {
        environment.remove("SEEDBOX_TEST_PAUSE_ONLY");
    }
    let reports = root.join("target/check-reports");
    fs::create_dir_all(&reports)?;
    let reports = tempfile::Builder::new()
        .prefix("run-")
        .tempdir_in(reports)?
        .keep();
    eprintln!("reports: {}", reports.display());
    environment.insert(
        "SEEDBOX_TEST_REPORT_DIR".into(),
        reports.to_string_lossy().into_owned(),
    );
    environment.insert(
        "SEEDBOX_ARM64_REPORT_DIR".into(),
        reports.join("arm64").to_string_lossy().into_owned(),
    );
    // Deliberately independent of the checkout/run: concurrent binaries and
    // sudo executions must contend for the same kernel lease.
    let leases = seedbox_system_tests::KernelLease::directory();
    fs::create_dir_all(&leases)?;
    environment.insert(
        "SEEDBOX_TEST_RESOURCE_DIR".into(),
        leases.to_string_lossy().into_owned(),
    );
    if suites.iter().any(|s| s.applications()) {
        let assets = crate::assets::build(root)?;
        let output = capture(
            root,
            Command::new("cargo")
                .args([
                    "build",
                    "--release",
                    "--locked",
                    "-p",
                    "seedbox",
                    "--message-format=json",
                ])
                .env("SEEDBOX_BUILD_KERNEL", assets.join("kernel"))
                .env("SEEDBOX_BUILD_HELPERS", assets.join("runtime.tar.gz")),
        )?;
        let binaries = executables(&output.stdout, "seedbox", false)?;
        let [binary] = binaries.as_slice() else {
            return Err("Cargo did not produce one Seedbox executable".into());
        };
        let frozen = reports.join("application-build");
        fs::create_dir(&frozen)?;
        for name in ["kernel", "runtime.tar.gz"] {
            fs::copy(assets.join(name), frozen.join(name))?;
        }
        fs::copy(binary, frozen.join("seedbox"))?;
        for key in [
            "SEEDBOX_ASSET_DIR",
            "SEEDBOX_BUILD_HELPERS",
            "SEEDBOX_GUEST_INIT",
            "SEEDBOX_MKE2FS",
            "SEEDBOX_VMM",
        ] {
            environment.remove(key);
        }
        for (key, name) in [
            ("SEEDBOX_TEST_CLI", "seedbox"),
            ("SEEDBOX_KERNEL", "kernel"),
            ("SEEDBOX_RUNTIME_ASSETS", "runtime.tar.gz"),
        ] {
            environment.insert(key.into(), frozen.join(name).to_string_lossy().into_owned());
        }
    }
    let output = capture(
        root,
        Command::new("cargo").args([
            "test",
            "--release",
            "--locked",
            "-p",
            "seedbox-system-tests",
            "--test",
            "system",
            "--no-run",
            "--message-format=json",
        ]),
    )?;
    let binaries = executables(&output.stdout, "system", true)?;
    let [binary] = binaries.as_slice() else {
        return Err("Cargo did not produce one system test binary".into());
    };
    let binary = if cfg!(target_os = "macos") {
        let signed = reports.join("system");
        fs::copy(binary, &signed)?;
        invoke(
            root,
            Command::new("bash")
                .arg("crates/seedbox-arm64/codesign.sh")
                .arg(&signed),
        )?;
        signed
    } else {
        binary.clone()
    };
    let needs_kvm = suites.iter().any(|s| {
        matches!(
            s,
            Suite::LinuxKvm | Suite::LinuxGuests | Suite::LinuxApps | Suite::LinuxPause
        )
    });
    let needs_engine = suites
        .iter()
        .any(|s| s.applications() || *s == Suite::LinuxOci);
    let privileged = sudo
        && (suites
            .iter()
            .any(|s| matches!(s, Suite::LinuxApps | Suite::LinuxPause))
            || (needs_kvm && !host.kvm_writable)
            || (needs_engine && !host.engine_writable));
    let mut command = if privileged {
        let mut c = Command::new("sudo");
        c.args(["-n", "env"]);
        // Forward explicit test inputs and engine connection, never credentials.
        for (key, value) in &environment {
            if key.starts_with("SEEDBOX_TEST_")
                || key.starts_with("SEEDBOX_ARM64_")
                || matches!(
                    key.as_str(),
                    "PATH"
                        | "SEEDBOX_KERNEL"
                        | "SEEDBOX_RUNTIME_ASSETS"
                        | "SEEDBOX_BUSYBOX"
                        | "SEEDBOX_MKE2FS"
                        | "SEEDBOX_STATIC_CC"
                        | "DOCKER_HOST"
                )
            {
                c.arg(format!("{key}={value}"));
            }
        }
        c.arg(&binary);
        c
    } else {
        Command::new(&binary)
    };
    command.envs(environment).args(["--ignored", "--nocapture"]);
    for filter in suites.iter().filter_map(|s| s.filter(&host.arch)) {
        command.arg(filter);
    }
    invoke(root, &mut command)?;
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selection_is_explicit_ordered_and_deduplicated() {
        assert_eq!(expand(&[]), [Suite::Portable]);
        assert_eq!(
            expand(&[Suite::MacApps, Suite::MacGuests, Suite::Mac]),
            [
                Suite::Portable,
                Suite::MacHvf,
                Suite::MacGuests,
                Suite::MacApps
            ]
        );
        assert_eq!(
            expand(&[Suite::Linux]),
            [
                Suite::Portable,
                Suite::LinuxKvm,
                Suite::LinuxOci,
                Suite::LinuxApps
            ]
        );
        assert_eq!(
            expand(&[Suite::LinuxGuests]),
            [Suite::Portable, Suite::LinuxKvm, Suite::LinuxGuests]
        );
    }

    #[test]
    fn requirements_keep_portable_and_engine_checks_independent_of_kvm() {
        let mut host = Host {
            os: "linux".into(),
            arch: "x86_64".into(),
            engine: true,
            engine_writable: true,
            ..Host::default()
        };
        assert!(host.missing(Suite::Portable).is_empty());
        assert!(host.missing(Suite::LinuxOci).is_empty());
        assert!(!host.missing(Suite::LinuxKvm).is_empty());
        host.kvm = true;
        assert!(host.missing(Suite::LinuxKvm).is_empty());
        assert_eq!(host.missing(Suite::LinuxApps).len(), 2);
        host.root = true;
        host.builder = true;
        assert!(host.missing(Suite::LinuxApps).is_empty());
        assert_eq!(host.missing(Suite::LinuxGuests).len(), 2);
        host.files
            .extend(["SEEDBOX_TEST_KERNEL".into(), "SEEDBOX_TEST_INITRD".into()]);
        assert!(host.missing(Suite::LinuxGuests).is_empty());
    }

    #[test]
    fn incompatible_platforms_and_missing_mac_inputs_fail_admission() {
        let host = Host {
            os: "macos".into(),
            arch: "aarch64".into(),
            hvf: true,
            codesign: true,
            ..Host::default()
        };
        assert!(host.missing(Suite::MacHvf).is_empty());
        assert!(!host.missing(Suite::LinuxKvm).is_empty());
        assert_eq!(host.missing(Suite::MacGuests).len(), 2);
        assert_eq!(Suite::MacHvf.filter("aarch64"), Some("arm64::native"));
        assert_eq!(Suite::LinuxKvm.filter("x86_64"), Some("x86::native"));
    }

    #[test]
    fn cargo_discovery_uses_the_exact_target_and_test_profile() {
        let bytes = br#"{"reason":"build-script-executed","executable":"/wrong"}
{"reason":"compiler-artifact","target":{"name":"system"},"profile":{"test":false},"executable":"/not-test"}
{"reason":"compiler-artifact","target":{"name":"other"},"profile":{"test":true},"executable":"/other"}
{"reason":"compiler-artifact","target":{"name":"system"},"profile":{"test":true},"executable":"/test"}
{"reason":"compiler-artifact","target":{"name":"system"},"profile":{"test":true},"executable":"/test"}
"#;
        assert_eq!(
            executables(bytes, "system", true).unwrap(),
            [PathBuf::from("/test")]
        );
        assert!(executables(b"not JSON", "system", true).is_err());
    }
}
