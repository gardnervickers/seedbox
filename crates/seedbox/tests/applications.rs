//! Each native workload is independently selectable; CLI/guest checks own its verdict.
#![cfg(any(
    all(target_os = "linux", target_arch = "x86_64"),
    all(target_os = "macos", target_arch = "aarch64")
))]
use seedbox_test::process::Guarded;
use std::{
    collections::HashSet,
    fs,
    io::{BufRead, BufReader},
    path::Path,
    process::{Child, Command, Stdio},
};

pub mod support;
use support::{failure_capture, json_diff};

// Every discovered workload proves a PASS. These four retain the expensive
// independent same-seed and retained-input comparisons, covering dynamic
// network faults, volume durability across crashes, clock state across reboot,
// and concurrent application behavior.
const REPEAT_WORKLOADS: [&str; 4] = ["network-faults", "volume-crash", "clock-reboot", "race"];

fn invoke(
    cli: &Path,
    directory: &Path,
    state: &Path,
    name: &str,
    args: &[&str],
) -> Result<String, String> {
    let logfile = directory.join(format!("{name}.log"));
    let log = fs::File::create(&logfile).map_err(|error| error.to_string())?;
    eprintln!("application: {} / {name}", directory.display());
    let mut command = Command::new(cli);
    command
        .args(args)
        .env_remove("SEEDBOX_VMM")
        .env("SEEDBOX_STATE", state)
        .stdout(log.try_clone().map_err(|error| error.to_string())?)
        .stderr(log);
    let status = command
        .guarded_status()
        .map_err(|error| format!("{}: {error}", cli.display()))?;
    if !status.success() {
        // CI keeps only the console, so show the end of the failing log there.
        let text = fs::read(&logfile).map(|bytes| String::from_utf8_lossy(&bytes).into_owned());
        if let Ok(text) = &text {
            let lines: Vec<_> = text.lines().collect();
            let tail = &lines[lines.len().saturating_sub(80)..];
            eprintln!(
                "--- {name} failed; last {} lines of {}:",
                tail.len(),
                logfile.display()
            );
            for line in tail {
                eprintln!("{line}");
            }
            eprintln!("--- end of {name}");
        }
        return Err(format!("{name}: {status}; see {}", logfile.display()));
    }
    Ok(
        String::from_utf8_lossy(&fs::read(&logfile).map_err(|error| error.to_string())?)
            .into_owned(),
    )
}

fn first_run(cli: &Path, directory: &Path, state: &Path, args: &[&str]) -> Result<String, String> {
    let result = invoke(cli, directory, state, "first", args);
    if let Err(error) = &result {
        let failure = directory.join("failure");
        failure_capture::note(&failure, error);
        let guard = failure_capture::budget();
        let capture = || -> Result<(), String> {
            let bytes = failure_capture::read(&directory.join("first.log"))?;
            fs::write(failure.join("first.log"), &bytes).map_err(|e| e.to_string())?;
            let output = String::from_utf8_lossy(&bytes);
            let id = output
                .lines()
                .find_map(|line| line.strip_prefix("simulation: "))
                .or_else(|| output.lines().find_map(|line| line.strip_prefix("FAIL: ")))
                .ok_or("failed first run has no simulation ID")?;
            // Validate before using the ID as a public observation argument.
            let id: seedbox_api::SimulationId =
                id.parse().map_err(|e| format!("simulation ID: {e}"))?;
            for (label, args) in [
                (
                    "oracle-logs",
                    vec!["logs", id.as_str(), "oracle", "--tail", "100"],
                ),
                (
                    "application-logs",
                    vec!["logs", id.as_str(), "--tail", "100"],
                ),
                (
                    "definition",
                    vec!["show", "--view", "definition", id.as_str(), "--json"],
                ),
                (
                    "world",
                    vec!["show", "--view", "world", id.as_str(), "--json"],
                ),
                (
                    "timeline",
                    vec!["show", "--view", "timeline", id.as_str(), "--json"],
                ),
            ] {
                let mut command = Command::new(cli);
                command
                    .args(args)
                    .env_remove("SEEDBOX_VMM")
                    .env("SEEDBOX_STATE", state);
                if let Err(error) = failure_capture::command(command, &failure, label, &guard) {
                    failure_capture::note(&failure, &format!("{label}: {error}"));
                }
            }
            Ok(())
        };
        if let Err(error) = capture() {
            failure_capture::note(&failure, &error);
        }
    }
    result
}

fn simulation_id(output: &str) -> Result<&str, String> {
    output
        .lines()
        .find_map(|line| line.strip_prefix("PASS: "))
        .ok_or_else(|| format!("missing PASS simulation ID: {output}"))
}

fn require_full_state_match(output: &str) -> Result<(), String> {
    let comparison: seedbox_api::SimulationComparison =
        serde_json::from_str(output).map_err(|error| format!("comparison: {error}"))?;
    if comparison.full_state_matches != Some(true) {
        return Err(format!("missing full-state MATCH: {output}"));
    }
    Ok(())
}

// Capture both sides while the session is still alive. The CI artifact collector
// bounds these files; shutdown would otherwise remove the evidence we need.
fn compare(
    cli: &Path,
    directory: &Path,
    state: &Path,
    name: &str,
    baseline: &str,
    repeated: &str,
) -> Result<(), String> {
    let result = invoke(
        cli,
        directory,
        state,
        name,
        &["compare", baseline, repeated, "--json"],
    )
    .and_then(|output| require_full_state_match(&output));
    if let Err(error) = &result {
        let failure = directory.join("failure");
        failure_capture::note(&failure, error);
        let guard = failure_capture::budget();
        for (side, id) in [("baseline", baseline), ("repeated", repeated)] {
            for (kind, args) in [
                (
                    "definition",
                    vec!["show", "--view", "definition", id, "--json"],
                ),
                ("world", vec!["show", "--view", "world", id, "--json"]),
                ("timeline", vec!["show", "--view", "timeline", id, "--json"]),
                ("logs", vec!["logs", id, "--tail", "100"]),
            ] {
                let mut command = Command::new(cli);
                command
                    .args(&args)
                    .env_remove("SEEDBOX_VMM")
                    .env("SEEDBOX_STATE", state);
                if let Err(error) = failure_capture::command(
                    command,
                    &failure,
                    &format!("{name}-{side}-{kind}"),
                    &guard,
                ) {
                    failure_capture::note(&failure, &format!("comparison diagnostic: {error}"));
                }
            }
        }
        log_definition_differences(&failure, name);
    }
    result
}

/// Show in the CI log where the two resolved definitions (prepared inputs
/// included) differ, so a run tells MISMATCH and NOT_COMPARABLE causes apart.
/// Reads the bounded definition observations captured in `failure`.
fn log_definition_differences(failure: &Path, name: &str) {
    let spec = |side: &str| {
        failure_capture::read(&failure.join(format!("{name}-{side}-definition.stdout")))
            .ok()
            .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
            .map(|resource| resource["spec"].clone())
    };
    let (Some(baseline), Some(repeated)) = (spec("baseline"), spec("repeated")) else {
        eprintln!("{name}: definition diff unavailable");
        return;
    };
    let mut differences = Vec::new();
    json_diff::json_differences("spec", &baseline, &repeated, &mut differences);
    eprintln!(
        "{name}: baseline/repeated definitions differ at {} paths",
        differences.len()
    );
    for difference in differences.iter().take(60) {
        eprintln!("  {difference}");
    }
}

macro_rules! applications {
    ($($case:ident => $workload:literal),+ $(,)?) => {
        const WORKLOADS: &[&str] = &[$($workload),+];
        $(#[test] fn $case() { application($workload); })+
    };
}
applications! {
    bandwidth => "bandwidth",
    clock_drift => "clock-drift",
    clock_reboot => "clock-reboot",
    clock_skew => "clock-skew",
    connections => "connections",
    device_faults => "device-faults",
    exclusives => "exclusives",
    key_value => "key-value",
    mac_capabilities => "mac-capabilities",
    network_faults => "network-faults",
    race => "race",
    runtime => "runtime",
    slow_machine => "slow-machine",
    triggers => "triggers",
    volume_crash => "volume-crash",
}

#[test]
fn every_discovered_workload_has_an_individually_selectable_case() {
    let directory =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../harness/acceptance/workloads");
    let discovered: HashSet<_> = fs::read_dir(directory)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "yaml"))
        .map(|path| path.file_stem().unwrap().to_str().unwrap().to_owned())
        .collect();
    assert_eq!(
        discovered,
        WORKLOADS.iter().map(|name| (*name).to_owned()).collect()
    );
    assert!(REPEAT_WORKLOADS.iter().all(|name| WORKLOADS.contains(name)));
}

fn application(name: &str) {
    let cli = std::env::var_os("SEEDBOX_TEST_CLI")
        .expect("harness/check.sh supplies the current release CLI");
    let reports =
        std::env::var_os("SEEDBOX_TEST_REPORT_DIR").expect("set a fresh SEEDBOX_TEST_REPORT_DIR");
    let workload = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../harness/acceptance/workloads")
        .join(format!("{name}.yaml"));
    // macOS limits Unix socket paths to 104 bytes. The checkout/report path
    // can exceed that before appending the socket name.
    let state_directory = tempfile::Builder::new()
        .prefix("seedbox-apps-")
        .tempdir_in("/tmp")
        .unwrap();
    // Let the daemon create its own state directory with private permissions.
    let state_path = state_directory.path().join("state");
    let state = state_path.as_path();
    let server = match Server::start(Path::new(&cli), state) {
        Ok(server) => server,
        Err(error) => {
            let retained = state_directory.keep();
            panic!("{error}; retained server state: {}", retained.display());
        }
    };
    // Every case owns its server/catalogue and report directory. Two cases can
    // execute independently without sharing simulation IDs or mutable state.
    let representative = REPEAT_WORKLOADS.contains(&name);
    let directory = Path::new(&reports)
        .join("applications")
        .join(workload.file_stem().unwrap());
    fs::create_dir_all(&directory).unwrap();
    let check = || -> Result<(), String> {
        let first_output = first_run(
            Path::new(&cli),
            &directory,
            state,
            &["run", workload.to_str().unwrap(), "--wall-time", "30m"],
        )?;
        let first = simulation_id(&first_output)?;
        if name == "device-faults" {
            return faulted(Path::new(&cli), &directory, state, first);
        }
        if !representative {
            return Ok(());
        }
        let described = invoke(
            Path::new(&cli),
            &directory,
            state,
            "describe-first",
            &["show", "--view", "definition", first, "--json"],
        )?;
        let resource: seedbox_api::Simulation =
            serde_json::from_str(&described).map_err(|error| format!("describe first: {error}"))?;
        let seed = resource
            .spec
            .seed
            .ok_or("first simulation has no seed")?
            .to_string();
        let second_output = invoke(
            Path::new(&cli),
            &directory,
            state,
            "second",
            &[
                "run",
                workload.to_str().unwrap(),
                "--seed",
                &seed,
                "--wall-time",
                "30m",
            ],
        )?;
        let second = simulation_id(&second_output)?;
        compare(Path::new(&cli), &directory, state, "compare", first, second)?;
        let repeated = invoke(
            Path::new(&cli),
            &directory,
            state,
            "replay",
            &["replay", first, "--wall-time", "30m"],
        )?;
        let replayed = repeated
            .lines()
            .find_map(|line| line.strip_prefix("MATCH: "))
            .ok_or_else(|| format!("missing replay MATCH: {repeated}"))?;
        compare(
            Path::new(&cli),
            &directory,
            state,
            "compare-replay",
            first,
            replayed,
        )?;
        Ok(())
    };
    let result = check();
    drop(server);
    if let Err(error) = result {
        let retained = state_directory.keep();
        panic!(
            "{name}: {error}\nretained server state: {}",
            retained.display()
        );
    }
}

/// The program `device-faults` adds to its own unfaulted run's definition:
/// phase i's fault is active from `START_NS + i * PERIOD_NS` for
/// `ACTIVE_NS`, the schedule its oracle (`device_faults.py`) runs each phase
/// in. The preempt phase is applied only where PV provides the guest yield
/// hook.
fn device_fault_program() -> Vec<serde_json::Value> {
    use serde_json::json;
    const START_NS: u64 = 1_000_000_000;
    const PERIOD_NS: u64 = 1_250_000_000;
    const ACTIVE_NS: u64 = 1_000_000_000;
    let phases = [
        (
            "read-eio",
            json!({"action":"disk_error","handle":"read-eio","machine":"oracle","op":"read","ppm":1_000_000,"scope":"data"}),
        ),
        (
            "corrupt",
            json!({"action":"disk_error","handle":"corrupt","machine":"oracle","op":"read","ppm":1_000_000,"effect":"corrupt","scope":"data"}),
        ),
        (
            "write-eio",
            json!({"action":"disk_error","handle":"write-eio","machine":"oracle","op":"write","ppm":500_000,"scope":"data"}),
        ),
        (
            "slow",
            json!({"action":"disk_latency","handle":"slow","machine":"oracle","op":"flush","latency_ns":20_000_000}),
        ),
        (
            "race",
            json!({"action":"preempt","handle":"race","machine":"oracle","ppm":100_000}),
        ),
    ];
    let mut program = Vec::new();
    for (index, (phase, fault)) in phases.into_iter().enumerate() {
        // Only the ARM PV kernel yields; x86_64 PV refuses preempt, so there
        // the oracle observes the race phase unfaulted.
        if phase == "race" && !cfg!(target_arch = "aarch64") {
            continue;
        }
        let start = START_NS + index as u64 * PERIOD_NS;
        program.push(json!({"id": format!("{phase}-on"), "atNs": start, "action": fault}));
        program.push(json!({
            "id": format!("{phase}-off"),
            "atNs": start + ACTIVE_NS,
            "action": {"action": "heal", "handle": phase},
        }));
    }
    program
}

/// Resubmit the unfaulted run's own definition with the fault program: the
/// oracle observes every fault from inside the guest, the world view keeps
/// the heals' counts and the victim's crash on a held FLUSH and reboot, and
/// a replay of the faulted run's retained inputs matches it in full state.
fn faulted(cli: &Path, directory: &Path, state: &Path, baseline: &str) -> Result<(), String> {
    let described = invoke(
        cli,
        directory,
        state,
        "describe-baseline",
        &["show", "--view", "definition", baseline, "--json"],
    )?;
    let mut resource: serde_json::Value =
        serde_json::from_str(&described).map_err(|error| format!("describe baseline: {error}"))?;
    resource["spec"]["overrides"] = serde_json::Value::Array(device_fault_program());
    let document = directory.join("faulted.json");
    fs::write(&document, serde_json::to_vec_pretty(&resource).unwrap())
        .map_err(|error| error.to_string())?;
    let output = invoke(
        cli,
        directory,
        state,
        "faulted",
        &["run", document.to_str().unwrap(), "--wall-time", "30m"],
    )?;
    let faulted = simulation_id(&output)?;
    let logs = invoke(
        cli,
        directory,
        state,
        "faulted-logs",
        &["logs", faulted, "oracle", "--tail", "100"],
    )?;
    let expected = if cfg!(target_arch = "aarch64") {
        "RESULT pass device faults faulted=read-eio,corrupt,write-eio,slow,race"
    } else {
        "RESULT pass device faults faulted=read-eio,corrupt,write-eio,slow"
    };
    if !logs.contains(expected) || !logs.contains("VICTIM crashed on a held flush and rebooted") {
        return Err(format!("faulted run did not observe its faults: {logs}"));
    }
    let world = invoke(
        cli,
        directory,
        state,
        "faulted-world",
        &["show", "--view", "world", faulted, "--json"],
    )?;
    let world: seedbox_api::WorldState =
        serde_json::from_str(&world).map_err(|error| format!("faulted world: {error}"))?;
    let heals: Vec<_> = world
        .faults
        .iter()
        .filter(|fault| fault.action == "heal")
        .collect();
    let counts = |handle: &str| {
        heals
            .iter()
            .find(|fault| {
                fault
                    .plan
                    .as_ref()
                    .is_some_and(|plan| plan["handle"] == handle)
            })
            .and_then(|fault| fault.counts)
            .ok_or_else(|| format!("{handle} heal has no counts: {heals:?}"))
    };
    for (handle, at_least) in [
        ("read-eio", 8),
        ("corrupt", 8),
        ("write-eio", 1),
        ("slow", 4),
    ] {
        let counts = counts(handle)?;
        if counts.affected < at_least || counts.affected > counts.matched {
            return Err(format!("{handle} heal counts {counts:?}"));
        }
    }
    let victim = world
        .machines
        .iter()
        .find(|machine| machine.name == "victim")
        .ok_or("victim missing from the world")?;
    if victim.incarnation < 1 {
        return Err(format!("victim never rebooted: {victim:?}"));
    }
    let replay = invoke(
        cli,
        directory,
        state,
        "faulted-replay",
        &["replay", faulted, "--wall-time", "30m"],
    )?;
    let replayed = replay
        .lines()
        .find_map(|line| line.strip_prefix("MATCH: "))
        .ok_or_else(|| format!("missing faulted replay MATCH: {replay}"))?;
    compare(cli, directory, state, "compare-faulted", faulted, replayed)
}

/// The acceptance fixture owns its server process; retained reports outlive it.
struct Server(Child);
impl Server {
    fn start(cli: &Path, state: &Path) -> Result<Self, String> {
        let mut server = Self(
            Command::new(cli)
                .args([
                    "server",
                    "start",
                    "--foreground",
                    "--startup-handshake",
                    "--full-state",
                ])
                .env("SEEDBOX_STATE", state)
                .stdout(Stdio::piped())
                .guarded_spawn()
                .map_err(|e| e.to_string())?,
        );
        let mut ready = String::new();
        BufReader::new(
            server
                .0
                .stdout
                .take()
                .ok_or("server startup pipe missing")?,
        )
        .read_line(&mut ready)
        .map_err(|e| e.to_string())?;
        let ready: serde_json::Value = serde_json::from_str(&ready).map_err(|e| e.to_string())?;
        if ready["status"] != "ready" {
            return Err("server failed to start".into());
        }
        Ok(server)
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        if self.0.try_wait().ok().flatten().is_some() {
            return;
        }
        // SAFETY: this is the unreaped child owned by this fixture.
        unsafe {
            libc::kill(self.0.id() as i32, libc::SIGTERM);
        }
        let pid = self.0.id();
        std::thread::scope(|scope| {
            let (done, completed) = std::sync::mpsc::channel();
            scope.spawn(move || {
                // Observe exit without reaping: the PID stays owned until the
                // main thread has decided whether it must send SIGKILL.
                let result = loop {
                    let mut info = std::mem::MaybeUninit::<libc::siginfo_t>::uninit();
                    // SAFETY: info is writable and pid names our unreaped child.
                    let rc = unsafe {
                        libc::waitid(
                            libc::P_PID,
                            pid,
                            info.as_mut_ptr(),
                            libc::WEXITED | libc::WNOWAIT,
                        )
                    };
                    if rc == 0 {
                        break Ok(());
                    }
                    let error = std::io::Error::last_os_error();
                    if error.kind() != std::io::ErrorKind::Interrupted {
                        break Err(error);
                    }
                };
                let _ = done.send(result);
            });
            if !matches!(
                completed.recv_timeout(std::time::Duration::from_secs(30)),
                Ok(Ok(()))
            ) {
                let _ = self.0.kill();
            }
        });
        self.0.wait().expect("reap owned application server");
    }
}

// Native CLI interactions share this discovered application acceptance target.
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
mod driver_preparation_native;

#[test]
fn first_run_failure_captures_live_guest_errors_and_preserves_exit() {
    let temporary = tempfile::tempdir().unwrap();
    let directory = temporary.path().join("reports");
    let state = temporary.path().join("state");
    fs::create_dir_all(&directory).unwrap();
    fs::create_dir_all(&state).unwrap();
    fs::write(state.join("live"), b"ready").unwrap();
    let cli = temporary.path().join("cli");
    seedbox_test::process::install_executable(&cli, br#"#!/bin/sh
case "$1" in
run) printf 'simulation: kmm5wkwf\nFAIL: kmm5wkwf\n'; exit 1;;
*) test -f "$SEEDBOX_STATE/live" || exit 9
   printf '%s\n' "$*" >> "$SEEDBOX_STATE/observations"
   case "$1" in
   logs) printf 'Traceback: failed rate assertion\n'; printf 'runtime-probe: assertion failed\n' >&2;;
   show) printf 'observation unavailable\n' >&2; exit 7;;
   esac;;
esac
"#, 0o755).unwrap();
    let error = first_run(&cli, &directory, &state, &["run", "fixture"]).unwrap_err();
    // Cleanup after the actual first-run helper; fixtures reject observations
    // after this boundary rather than inferring order from elapsed host time.
    let observed = fs::read_to_string(state.join("observations")).unwrap();
    fs::remove_dir_all(&state).unwrap();
    assert!(error.starts_with("first: exit status: 1;"), "{error}");
    assert_eq!(observed.lines().count(), 5);
    assert!(observed.contains("logs kmm5wkwf oracle --tail 100"));
    let failure = directory.join("failure");
    assert!(fs::read_to_string(failure.join("oracle-logs.stdout"))
        .unwrap()
        .contains("Traceback"));
    assert!(fs::read_to_string(failure.join("application-logs.stderr"))
        .unwrap()
        .contains("assertion failed"));
    let summary = fs::read_to_string(failure.join("summary.txt")).unwrap();
    assert!(summary.contains(&error));
    assert!(summary.contains("definition: definition: exit status: 7"));
    assert!(summary.contains("timeline:"));
}

#[test]
fn first_run_success_avoids_failure_capture() {
    let temporary = tempfile::tempdir().unwrap();
    let cli = temporary.path().join("cli");
    seedbox_test::process::install_executable(
        &cli,
        b"#!/bin/sh\ntest \"$1\" = run || exit 9\nprintf 'PASS: kmm5wkwf\\n'\n",
        0o755,
    )
    .unwrap();
    assert_eq!(
        first_run(&cli, temporary.path(), temporary.path(), &["run"]).unwrap(),
        "PASS: kmm5wkwf\n"
    );
    assert!(!temporary.path().join("failure").exists());
}

#[test]
fn failure_capture_bounds_a_long_guest_line_and_drains_both_pipes() {
    let temporary = tempfile::tempdir().unwrap();
    let mut command = Command::new("/bin/sh");
    command.args([
        "-c",
        "head -c 8388627 /dev/zero; printf 'assertion marker\\n' >&2",
    ]);
    let error = failure_capture::command(
        command,
        temporary.path(),
        "oracle-log",
        &failure_capture::budget(),
    )
    .unwrap_err();
    assert!(error.contains("omitted 19 bytes"), "{error}");
    assert_eq!(
        fs::metadata(temporary.path().join("oracle-log.stdout"))
            .unwrap()
            .len(),
        failure_capture::LIMIT
    );
    assert_eq!(
        fs::read_to_string(temporary.path().join("oracle-log.stderr")).unwrap(),
        "assertion marker\n"
    );
}

#[test]
fn comparison_failure_keeps_original_error_and_captures_both_sides() {
    let temporary = tempfile::tempdir().unwrap();
    let cli = temporary.path().join("cli");
    seedbox_test::process::install_executable(
        &cli,
        br#"#!/bin/sh
case "$1" in
compare) printf 'comparison mismatch\n'; exit 1;;
*) printf '%s\n' "$*" >> "$SEEDBOX_STATE/observations"
   printf 'diagnostic assertion marker\n'; exit 7;;
esac
"#,
        0o755,
    )
    .unwrap();
    let error = compare(
        &cli,
        temporary.path(),
        temporary.path(),
        "compare",
        "kmm5wkwf",
        "23456789",
    )
    .unwrap_err();
    assert!(error.starts_with("compare: exit status: 1;"), "{error}");
    assert_eq!(
        fs::read_to_string(temporary.path().join("observations"))
            .unwrap()
            .lines()
            .count(),
        8
    );
    let failure = temporary.path().join("failure");
    assert!(fs::read_to_string(failure.join("summary.txt"))
        .unwrap()
        .contains(&error));
    for side in ["baseline", "repeated"] {
        for kind in ["definition", "world", "timeline", "logs"] {
            assert_eq!(
                fs::read_to_string(failure.join(format!("compare-{side}-{kind}.stdout"))).unwrap(),
                "diagnostic assertion marker\n"
            );
        }
    }
}
