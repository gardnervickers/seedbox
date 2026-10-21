use super::{
    policy::*,
    process::{timed, words, Checks, Commands, Job, Real, Temporary},
};
use std::{
    fs,
    path::{Path, PathBuf},
    time::Duration,
};

#[derive(Debug)]
pub struct Options {
    pub suites: Vec<Suite>,
    pub list: bool,
    pub test_time_policy: bool,
    pub probe: bool,
    pub help: bool,
}
pub fn options(args: Vec<String>) -> Result<Options, String> {
    let mut names = Vec::new();
    let mut list = false;
    let mut skip_portable = false;
    let mut test_time_policy = false;
    let mut probe = false;
    let mut help = false;
    for arg in args {
        match arg.as_str() {
            "--list" => list = true,
            "--skip-portable" => skip_portable = true,
            "--test-time-policy" => test_time_policy = true,
            "--probe-linux" => probe = true,
            "--help" | "-h" => help = true,
            _ if arg.starts_with('-') => return Err(format!("unknown option: {arg}; see --help")),
            _ => names.push(arg),
        }
    }
    if test_time_policy && (!names.is_empty() || list || skip_portable || probe) {
        return Err("--test-time-policy must be used without suites or other modes".into());
    }
    let mut suites = expand(&names)?;
    if skip_portable {
        if names.is_empty() || names.iter().any(|name| name == "portable") {
            return Err("--skip-portable requires native suites and cannot select portable".into());
        }
        suites.retain(|suite| *suite != Suite::Portable);
    }
    Ok(Options {
        suites,
        list,
        test_time_policy,
        probe,
        help,
    })
}
fn help() {
    println!("Run explicitly named test suites; Cargo discovers tests.\n\nUsage: seedbox-check [--list] [SUITE ...]\n\n  --list             Check requirements without building or running tests.\n  --skip-portable    Run native suites after portable passed earlier in this job.\n  --test-time-policy Scan test code for host sleeps without running other checks.\n  --help             Show this help.\n\nsuites:");
    for s in SUITES {
        println!("  {:16} {}", s.name(), s.description());
    }
    println!("groups:\n  linux             linux-oci linux-apps\n  mac               mac-apps\n\nUse inside the pinned nix develop environment. Cold shell realization provisions\ntools; checker invocation never builds its own executable.");
}
pub fn arguments(args: Vec<std::ffi::OsString>) -> Result<Vec<String>, String> {
    args.into_iter()
        .enumerate()
        .map(|(index, value)| {
            value.into_string().map_err(|_| {
                format!(
                    "argument {} contains non-UTF-8 bytes; use a suite listed by --help",
                    index + 1
                )
            })
        })
        .collect()
}
pub fn environment_values(
    values: impl IntoIterator<Item = (std::ffi::OsString, std::ffi::OsString)>,
) -> Result<Environment, String> {
    values
        .into_iter()
        .map(|(key, value)| {
            let key = key
                .into_string()
                .map_err(|_| "environment variable name contains non-UTF-8 bytes".to_string())?;
            let value = value.into_string().map_err(|_| {
                format!("environment variable {key} contains non-UTF-8 bytes; use a UTF-8 value")
            })?;
            Ok((key, value))
        })
        .collect()
}
pub fn entry(args: Vec<std::ffi::OsString>) -> Result<u8, String> {
    let options = options(arguments(args)?)?;
    if options.help {
        help();
        return Ok(0);
    }
    let root = std::env::current_dir().map_err(|e| e.to_string())?;
    if !root.join("harness").is_dir() || !root.join("Cargo.toml").is_file() {
        return Err(
            "run from the checkout root, or use bash harness/check.sh inside nix develop".into(),
        );
    }
    if options.test_time_policy {
        return test_time_policy_stage(&root);
    }
    let env = environment_values(std::env::vars_os())?;
    let mut checks = Checks::new(
        Real::install_signals().map_err(|e| e.to_string())?,
        root,
        env,
    );
    if options.probe {
        println!("{}", serde_json::json!({"kvm":probe_linux()}));
        return Ok(0);
    }
    let host = observe(&mut checks, &options.suites)?;
    coordinate(&mut checks, &host, &options)
}
pub fn test_time_policy_stage(root: &Path) -> Result<u8, String> {
    let report = super::test_time_policy::check(root)?;
    for finding in &report.findings {
        eprintln!("{finding}");
    }
    if report.findings.is_empty() {
        println!("test-time-policy: PASS");
        Ok(0)
    } else {
        eprintln!(
            "test-time-policy: FAIL ({} findings)",
            report.findings.len()
        );
        Ok(1)
    }
}
pub fn probe_linux() -> String {
    use std::os::fd::AsRawFd;
    #[cfg(target_os = "linux")]
    {
        let file = match fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/kvm")
        {
            Ok(f) => f,
            Err(e) => return e.to_string(),
        };
        let api = unsafe { libc::ioctl(file.as_raw_fd(), 0xAE00, 0) };
        if api == 12 {
            String::new()
        } else if api < 0 {
            std::io::Error::last_os_error().to_string()
        } else {
            "unsupported KVM API".into()
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = std::mem::size_of::<Option<&dyn AsRawFd>>();
        "requires Linux".into()
    }
}
fn find_tool(env: &Environment, tool: &str) -> bool {
    if tool.contains('/') {
        return executable(Path::new(tool));
    }
    std::env::split_paths(env.get("PATH").map(String::as_str).unwrap_or(""))
        .any(|dir| executable(&dir.join(tool)))
}
fn observation<C: Commands>(
    checks: &mut Checks<C>,
    name: &str,
    args: Vec<String>,
) -> Option<String> {
    let mut job = checks.job(name, args);
    job.capture = true;
    checks
        .commands
        .execute(&job)
        .ok()
        .filter(|o| o.code == 0)
        .map(|o| o.stdout.trim().into())
}
pub fn observe<C: Commands>(checks: &mut Checks<C>, suites: &[Suite]) -> Result<Host, String> {
    use std::os::unix::{ffi::OsStrExt, fs::FileTypeExt};
    let mut host = Host::local();
    // Portable/list needs no command, capability probe, privilege or build.
    if suites == [Suite::Portable] {
        return Ok(host);
    }
    let mut tools = words(&["sudo", "nix", "podman", "docker", "codesign"]);
    if let Some(builder) = checks.environment.get("SEEDBOX_ASSET_BUILDER") {
        tools.push(builder.clone());
    }
    host.tools.extend(
        tools
            .into_iter()
            .filter(|tool| find_tool(&checks.environment, tool)),
    );
    // No root capability is needed at all by the TCG-only fixture suite.
    if host.system == "Linux"
        && suites.iter().any(|s| {
            !matches!(
                s,
                Suite::Portable | Suite::MachineTcg | Suite::Dynamorio | Suite::DynamorioIsolated
            )
        })
    {
        if unsafe { libc::geteuid() } == 0 {
            host.root_prefix = Some(vec![]);
        } else if host.tools.contains("sudo")
            && observation(checks, "root-capability", words(&["sudo", "-n", "true"])).is_some()
        {
            host.root_prefix = Some(vec![
                "sudo".into(),
                "-n".into(),
                "env".into(),
                format!(
                    "PATH={}",
                    checks
                        .environment
                        .get("PATH")
                        .map(String::as_str)
                        .unwrap_or("")
                ),
            ]);
        }
        let executable = std::env::current_exe().map_err(|e| e.to_string())?;
        let command = privileged(
            host.root_prefix.as_deref(),
            vec![executable.to_string_lossy().into(), "--probe-linux".into()],
            &checks.environment,
        );
        let output = observation(checks, "kvm-capability", command)
            .ok_or("KVM capability probe failed before builds")?;
        host.kvm = Some(
            serde_json::from_str::<serde_json::Value>(&output).map_err(|e| e.to_string())?["kvm"]
                .as_str()
                .ok_or("KVM probe has no result")?
                .into(),
        );
    }
    if host.system == "Linux" && suites.contains(&Suite::DynamorioIsolated) {
        if let Some(fixtures) = checks.environment.get("SEEDBOX_DYNAMORIO_FIXTURES") {
            let launcher = Path::new(fixtures).join("bin/seedbox-dr-isolate");
            if executable(&launcher) {
                for mode in ["pid1", "pids"] {
                    let selected = if mode == "pid1" {
                        checks
                            .environment
                            .get("SEEDBOX_DYNAMORIO_PID1_LAUNCHER")
                            .map(PathBuf::from)
                            .unwrap_or_else(|| launcher.clone())
                    } else {
                        launcher.clone()
                    };
                    let mut job = checks.job(
                        &format!("isolate-{mode}-preflight"),
                        vec![
                            selected.to_string_lossy().into(),
                            "preflight".into(),
                            mode.into(),
                        ],
                    );
                    job.timeout = Some(Duration::from_secs(150));
                    if checks.commands.execute(&job).is_ok_and(|o| o.code == 0) {
                        host.tools.insert(format!("dynamorio-isolate-{mode}"));
                    }
                }
            }
        }
    }
    let mut sockets = socket_candidates(&checks.environment);
    if let Some(path) = checks
        .environment
        .get("DOCKER_HOST")
        .and_then(|s| s.strip_prefix("unix://"))
    {
        sockets.push(path.into());
    }
    if host.system == "Darwin" && suites.contains(&Suite::MacApps) {
        host.mac_major = observation(
            checks,
            "mac-version",
            words(&["sw_vers", "-productVersion"]),
        )
        .and_then(|v| v.split('.').next()?.parse().ok())
        .unwrap_or(0);
        host.hvf = observation(
            checks,
            "hvf-capability",
            words(&["sysctl", "-n", "kern.hv_support"]),
        )
        .as_deref()
            == Some("1");
    }
    if host.system == "Darwin" && suites.contains(&Suite::MacApps) {
        if let Some(dir) = observation(
            checks,
            "mac-temporary",
            words(&["getconf", "DARWIN_USER_TEMP_DIR"]),
        ) {
            if let Ok(entries) = fs::read_dir(Path::new(&dir).join("podman")) {
                for entry in entries.flatten() {
                    if entry.file_name().to_string_lossy().ends_with("-api.sock") {
                        sockets.push(entry.path());
                    }
                }
            }
        }
    }
    sockets.sort();
    sockets.dedup();
    for path in sockets {
        if fs::metadata(&path).is_ok_and(|m| m.file_type().is_socket()) {
            let access = std::ffi::CString::new(path.as_os_str().as_bytes())
                .is_ok_and(|p| unsafe { libc::access(p.as_ptr(), libc::R_OK | libc::W_OK) == 0 });
            host.sockets.push((path, access));
        }
    }
    Ok(host)
}
pub fn coordinate<C: Commands>(
    checks: &mut Checks<C>,
    host: &Host,
    options: &Options,
) -> Result<u8, String> {
    println!("platform: {}/{}", host.system, host.machine);
    let mut unavailable = Vec::new();
    for suite in &options.suites {
        let missing = requirements(*suite, host, &checks.environment, &checks.root);
        println!(
            "suite {}: {}",
            suite.name(),
            if missing.is_empty() {
                "ready".into()
            } else {
                format!("unavailable: {}", missing.join("; "))
            }
        );
        if !missing.is_empty() {
            unavailable.push(suite.name());
        }
    }
    if !unavailable.is_empty() {
        eprintln!(
            "error: selected suites cannot run on this host: {}",
            unavailable.join(", ")
        );
        return Ok(2);
    }
    if options.list {
        return Ok(0);
    }
    if options.suites.contains(&Suite::Portable) {
        checks.step("fmt", words(&["cargo", "fmt", "--all", "--check"]));
        // --all-features type-checks every feature-gated target (native service
        // suites, worker-failure hooks) without a second full test build.
        checks.step(
            "clippy",
            words(&[
                "cargo",
                "clippy",
                "--workspace",
                "--all-targets",
                "--all-features",
                "--locked",
                "--",
                "-D",
                "warnings",
            ]),
        );
        portable(checks)?;
    }
    if checks.failed || options.suites == [Suite::Portable] {
        return Ok(u8::from(checks.failed));
    }
    let mut runs = machine_suite_runs(&options.suites, &checks.environment, &checks.root);
    if runs
        .iter()
        .any(|job| job.args.iter().any(|a| a == "seedbox-server"))
    {
        // The public-service tests submit through the checkout's normal
        // release CLI; build it from these sources rather than trust a
        // possibly stale target/release/seedbox. Without it, skip only them.
        if checks
            .run(service_release_build(&checks.environment, &checks.root))
            .is_none()
        {
            runs.retain(|job| !job.args.iter().any(|a| a == "seedbox-server"));
        }
    }
    for job in runs {
        run_machine(checks, job)?;
    }
    native(checks, host, &options.suites)?;
    Ok(u8::from(checks.failed))
}
pub fn portable<C: Commands>(checks: &mut Checks<C>) -> Result<(), String> {
    // One logical span includes Git discovery, every shell check and failures.
    if !timed(checks, "shell-syntax", |checks| {
        let mut job = checks.job(
            "shell-discovery",
            words(&[
                "git",
                "ls-files",
                "-z",
                "--cached",
                "--others",
                "--exclude-standard",
                "*.sh",
                ":!:research/**",
            ]),
        );
        job.capture = true;
        let Some(output) = checks.run(job) else {
            return Ok(false);
        };
        for script in output.split('\0').filter(|s| !s.is_empty()) {
            if checks.root.join(script).is_file()
                && !checks.step(
                    "shell-syntax",
                    vec!["bash".into(), "-n".into(), script.into()],
                )
            {
                return Ok(false);
            }
        }
        Ok(true)
    })? {
        return Ok(());
    }
    if !timed(checks, "test-time-policy", |checks| {
        let passed = test_time_policy_stage(&checks.root)? == 0;
        checks.failed |= !passed;
        Ok(passed)
    })? {
        return Ok(());
    }
    if !timed(checks, "workspace-tests", |checks| {
        Ok(checks.step(
            "workspace-tests",
            words(&["cargo", "test", "--workspace", "--locked"]),
        ))
    })? {
        return Ok(());
    }
    // seedbox-cli's native_acceptance_projects_round_trip_through_submission_json
    // discovers and validates every workload in the workspace tests above.
    // The public `check` command also probes the host and is not a portable check.
    Ok(())
}
pub fn machine_suite_runs(suites: &[Suite], environment: &Environment, root: &Path) -> Vec<Job> {
    let mut environment = environment.clone();
    environment
        .entry("RUST_TEST_THREADS".into())
        .or_insert("2".into());
    let mut result = Vec::new();
    for (suite, selector, backend, terminal) in [
        (
            Suite::MachineTcg,
            "tcg",
            "seedbox-machine-tcg",
            "tcg_shutdown",
        ),
        (Suite::MachinePv, "pv", "seedbox-machine-pv", "pv_progress"),
    ] {
        if !suites.contains(&suite) {
            continue;
        }
        let mut selections = vec![
            (backend, "--lib", "", None),
            (backend, "--test", "native_machine", None),
            ("seedbox-driver", "--test", "native_driver", Some(selector)),
            (
                if suite == Suite::MachineTcg {
                    "seedbox-driver"
                } else {
                    backend
                },
                "--test",
                terminal,
                None,
            ),
        ];
        if suite == Suite::MachinePv {
            selections.push(("seedbox-driver", "--test", "native_pv_driver", None));
            selections.push(("seedbox-driver", "--test", "pv_instructions", None));
        }
        selections.push((
            "seedbox-driver",
            "--test",
            "driver_guest_services",
            Some(selector),
        ));
        for target in [
            "guest_services",
            "prepared_application",
            "simulation_control",
        ] {
            selections.push((backend, "--test", target, Some(selector)));
        }
        for (package, kind, target, filter) in selections {
            let mut args = words(&["cargo", "test", "-p", package, "--locked", kind]);
            if !target.is_empty() {
                args.push(target.into());
            }
            args.extend(words(&["--", "--ignored"]));
            if let Some(filter) = filter {
                args.push(filter.into());
            }
            result.push(Job::new(
                &format!("{}/{package}/{kind}/{target}", suite.name()),
                args,
                &environment,
                root,
            ));
        }
        // Public-service acceptance is feature-gated rather than ignored:
        // build with the suite's feature and run the whole target unfiltered.
        for &(target, features) in service_targets(suite) {
            result.push(Job::new(
                &format!("{}/seedbox-server/--test/{target}", suite.name()),
                words(&[
                    "cargo",
                    "test",
                    "-p",
                    "seedbox-server",
                    "--locked",
                    "--features",
                    features,
                    "--test",
                    target,
                    "--",
                ]),
                &environment,
                root,
            ));
        }
        // native_tcg_driver retains unignored raw cases, so it was not part
        // of the old --ignored native selection. Keep its existing workspace
        // compile/execution classification unchanged; do not add an ignored
        // filter or independently schedule refused fixtures here.
    }
    result
}
/// `cargo build --release` of the CLI, embedding `build/assets`. It keeps the
/// suite's `CARGO_TARGET_DIR`, because the public-service tests look for
/// `release/seedbox` in that same target (the checkout's `target` when unset).
pub fn service_release_build(environment: &Environment, root: &Path) -> Job {
    Job::new(
        "service-release-cli",
        words(&["cargo", "build", "--release", "--locked", "-p", "seedbox"]),
        environment,
        root,
    )
}
fn list_selection(output: &str) -> usize {
    output.lines().filter(|l| l.ends_with(": test")).count()
}
pub fn run_machine<C: Commands>(checks: &mut Checks<C>, job: Job) -> Result<(), String> {
    // Build/list uses the same Cargo target and libtest filters. No individual
    // testcase registry; a stale backend filter cannot silently pass zero cases.
    let mut list = job.clone();
    list.name.push_str("-list");
    list.args.push("--list".into());
    list.capture = true;
    if let Some(output) = checks.run(list) {
        if list_selection(&output) == 0 {
            checks.failed = true;
            return Err(format!("{}: selected no native tests", job.name));
        }
        checks.run(job);
    }
    Ok(())
}
pub fn cargo_tests<C: Commands>(
    checks: &mut Checks<C>,
    package: &str,
    mut command: Vec<String>,
    features: &[String],
    prefix: Option<&[String]>,
    engine_requires_root: bool,
) -> Result<(), String> {
    // Respect the caller's private target across discovered package builds too.
    let mut env = checks.environment.clone();
    if !features.is_empty() {
        command.extend(["--features".into(), features.join(",")]);
    }
    command.extend(words(&["--no-run", "--message-format=json"]));
    let mut build = Job::new(&format!("{package}-build"), command, &env, &checks.root);
    build.capture = true;
    let Some(output) = checks.run(build) else {
        return Ok(());
    };
    let artifacts = test_artifacts(&output)?;
    if artifacts.is_empty() {
        return Err(format!("{package}: Cargo produced no test executables"));
    }
    for (binary, target) in artifacts {
        // Native fixtures create Unix sockets beneath TMPDIR. Own a short
        // scratch root rather than inheriting a potentially long Nix build path;
        // keep it alive until the test process (including sudo) has exited.
        let scratch = Temporary::in_directory(Path::new("/tmp"), "sb-check-")
            .map_err(|e| format!("native test scratch directory: {e}"))?;
        env.insert("TMPDIR".into(), scratch.0.to_string_lossy().into());
        let mut command = vec![binary.to_string_lossy().into(), "--test-threads=1".into()];
        if matches!(
            target.as_str(),
            "applications" | "dynamorio" | "dynamorio_isolated"
        ) {
            command.push("--nocapture".into());
        }
        if (package == "seedbox"
            && features
                .iter()
                .any(|f| matches!(f.as_str(), "application-tests" | "retained-run-tests")))
            || (package == "seedbox-application"
                && features.iter().any(|f| f == "oci-tests")
                && engine_requires_root)
        {
            command = privileged(prefix, command, &env);
        }
        let mut job = Job::new(&format!("{package}/{target}"), command, &env, &checks.root);
        job.timeout = match (package, target.as_str()) {
            (_, "applications") => None,
            // The complete ordinary DR corpus includes managed runtimes and
            // many native/plain/client phases. Allow a diagnostic run to reach
            // every case; each phase keeps its own shorter completion guard.
            ("seedbox-machine", "dynamorio") => Some(Duration::from_secs(5400)),
            _ => Some(Duration::from_secs(1800)),
        };
        checks.run(job);
    }
    Ok(())
}
pub fn release_cli_artifact(output: &str) -> Result<PathBuf, String> {
    let mut paths = std::collections::BTreeSet::new();
    for line in output.lines().filter(|line| !line.trim().is_empty()) {
        let value: serde_json::Value =
            serde_json::from_str(line).map_err(|e| format!("release artifact JSON: {e}"))?;
        if value["reason"] == "compiler-artifact"
            && value["target"]["name"] == "seedbox"
            && value["profile"]["test"] == false
        {
            if let Some(path) = value["executable"].as_str() {
                paths.insert(PathBuf::from(path));
            }
        }
    }
    if paths.len() != 1 {
        return Err("release build did not return exactly one seedbox executable".into());
    }
    let path = paths
        .into_iter()
        .next()
        .ok_or("missing release executable")?;
    if !path.is_absolute() {
        return Err("release build returned a nonabsolute executable path".into());
    }
    Ok(path)
}
fn native<C: Commands>(
    checks: &mut Checks<C>,
    host: &Host,
    suites: &[Suite],
) -> Result<(), String> {
    let reports_root = checks.root.join("target/check-reports");
    fs::create_dir_all(&reports_root).map_err(|e| e.to_string())?;
    // Retain reports intentionally, even on failure; only disposable test copies
    // use the Temporary Drop lifetime.
    let reports = Temporary::in_directory(&reports_root, "run-").map_err(|e| e.to_string())?;
    let report_path = reports.0.clone();
    std::mem::forget(reports);
    println!("reports: {}", report_path.display());
    checks.environment.insert(
        "SEEDBOX_TEST_REPORT_DIR".into(),
        report_path.to_string_lossy().into(),
    );
    if suites
        .iter()
        .any(|s| matches!(s, Suite::LinuxApps | Suite::LinuxPause | Suite::MacApps))
    {
        let mut job = checks.job(
            "application-assets",
            words(&["cargo", "xtask", "build-assets"]),
        );
        job.capture = true;
        let Some(output) = checks.run(job) else {
            return Ok(());
        };
        let assets = PathBuf::from(output.trim());
        if !assets.is_absolute() || !assets.join("driver-runtime.tar.gz").is_file() {
            return Err("asset build did not return its absolute complete output directory".into());
        }
        let mut build = checks.job(
            "application-build",
            words(&[
                "cargo",
                "build",
                "--release",
                "--locked",
                "-p",
                "seedbox",
                "--message-format=json",
            ]),
        );
        if suites.contains(&Suite::LinuxApps) {
            // The smoke test launches this CLI as its worker too. Enable its
            // private page capture here, not only in the libtest executable.
            build.args.extend(words(&["--features", "etcd-prep-tests"]));
        }
        build.environment.insert(
            "SEEDBOX_BUILD_DRIVER_RUNTIME".into(),
            assets
                .join("driver-runtime.tar.gz")
                .to_string_lossy()
                .into(),
        );
        // Honor the workspace cache setting, but freeze the exact executable
        // Cargo actually built, not a potentially stale target/release path.
        build.capture = true;
        let Some(compiled) = checks.run(build) else {
            return Ok(());
        };
        let executable = release_cli_artifact(&compiled)?;
        let complete = report_path.join("application-build");
        fs::create_dir(&complete).map_err(|e| e.to_string())?;
        fs::copy(
            assets.join("driver-runtime.tar.gz"),
            complete.join("driver-runtime.tar.gz"),
        )
        .map_err(|e| e.to_string())?;
        fs::copy(executable, complete.join("seedbox")).map_err(|e| e.to_string())?;
        select_build(&mut checks.environment, &complete)?;
    }
    let mut job = checks.job(
        "test-targets",
        words(&[
            "cargo",
            "metadata",
            "--no-deps",
            "--format-version=1",
            "--locked",
        ]),
    );
    job.capture = true;
    let Some(output) = checks.run(job) else {
        return Ok(());
    };
    let metadata = serde_json::from_str(&output).map_err(|e| e.to_string())?;
    for tests in suite_test_targets(&metadata, suites)? {
        let package = tests.package.as_str();
        let mut command = words(&["cargo", "test", "-p", package, "--locked"]);
        for target in tests.targets {
            command.extend(["--test".into(), target]);
        }
        let engine_requires_root = host.engine_needs_root(&checks.environment);
        cargo_tests(
            checks,
            package,
            command,
            &tests.features,
            host.root_prefix.as_deref(),
            engine_requires_root,
        )?;
    }
    Ok(())
}
