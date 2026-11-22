//! Each native workload is independently selectable; CLI/guest checks own its verdict.
#![cfg(any(
    all(target_os = "linux", target_arch = "x86_64"),
    all(target_os = "macos", target_arch = "aarch64")
))]
use seedbox_test::process::Guarded;
use std::{
    collections::HashSet,
    fs,
    path::Path,
    process::{Command, Stdio},
};

pub mod support;
use support::application::{
    compare, first_run, invoke, invoke_stdout, replayed, simulation_id, with_server,
};
use support::failure_capture;

// Every discovered workload proves a PASS. These retain the expensive
// independent same-seed and retained-input comparisons, covering dynamic
// network faults, volume durability across crashes, clock state across reboot,
// concurrent application behavior, guest property findings (with an allowed
// crash and a machine's reboot), and an oracle's seeded fault profile as part
// of the compared results.
const REPEAT_WORKLOADS: [&str; 7] = [
    "dns",
    "network-faults",
    "volume-crash",
    "clock-reboot",
    "race",
    "properties",
    "profile",
];

macro_rules! applications {
    ($($(#[$attribute:meta])* $case:ident => $workload:literal),+ $(,)?) => {
        const WORKLOADS: &[&str] = &[$($workload),+];
        $($(#[$attribute])* #[test] fn $case() { application($workload); })+
    };
}
applications! {
    bandwidth => "bandwidth",
    clock_drift => "clock-drift",
    clock_reboot => "clock-reboot",
    clock_skew => "clock-skew",
    clock_step => "clock-step",
    connections => "connections",
    device_faults => "device-faults",
    dns => "dns",
    enospc => "enospc",
    exclusives => "exclusives",
    key_value => "key-value",
    mac_capabilities => "mac-capabilities",
    network_faults => "network-faults",
    pause_kinds => "pause-kinds",
    process_groups => "process-groups",
    profile => "profile",
    properties => "properties",
    race => "race",
    runtime => "runtime",
    slow_machine => "slow-machine",
    stale_ack => "stale-ack",
    triggers => "triggers",
    volume_crash => "volume-crash",
}

fn workload(name: &str) -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../harness/acceptance/workloads")
        .join(format!("{name}.yaml"))
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

/// Projects whose oracle exits 0 but whose guest properties decide the run:
/// a violated `always`, an unmet requirement, a crash on a machine without
/// `allowCrashes`, and a violation sent with the guest socket's path hidden
/// must FAIL (1) giving the reason; lost records must make the run
/// INCOMPLETE (3).
#[test]
fn guest_properties_decide_runs_whose_oracle_passes() {
    with_server("applications", "verdicts", |cli, reports, state| {
        for (name, word, code, reason) in [
            (
                "property-violation",
                "FAIL",
                1,
                "violated: probe: arithmetic holds (always)",
            ),
            (
                "unmet-requirement",
                "FAIL",
                1,
                "unmet requirement: probe: never evaluated",
            ),
            (
                "unexpected-crash",
                "FAIL",
                1,
                "violated: seedbox.no_crashing_processes (never)",
            ),
            (
                "hidden-socket",
                "FAIL",
                1,
                "violated: probe: two plus two is four (never)",
            ),
            ("lost-evidence", "INCOMPLETE", 3, "lost evidence: "),
        ] {
            let project = Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../harness/acceptance/verdicts")
                .join(format!("{name}.yaml"));
            let directory = reports.join(name);
            fs::create_dir_all(&directory).map_err(|e| e.to_string())?;
            let log = directory.join("first.log");
            let status = Command::new(cli)
                .args(["run", project.to_str().unwrap(), "--wall-time", "30m"])
                .env("SEEDBOX_STATE", state)
                .stdout(fs::File::create(&log).map_err(|e| e.to_string())?)
                .stderr(Stdio::inherit())
                .guarded_status()
                .map_err(|e| e.to_string())?;
            let output = fs::read_to_string(&log).map_err(|e| e.to_string())?;
            let id = output
                .lines()
                .find_map(|line| line.strip_prefix(&format!("{word}: ")))
                .ok_or_else(|| format!("{name}: no {word} ({status}): {output}"))?;
            if status.code() != Some(code) || !output.contains(reason) {
                return Err(format!("{name}: {status}: {output}"));
            }
            // `show` gives the same reason for the completed simulation.
            let shown = invoke(cli, &directory, state, "show", &["show", id])?;
            if !shown.contains(reason) {
                return Err(format!("{name}: show: {shown}"));
            }
            let shown = invoke(cli, &directory, state, "show-json", &["show", id, "--json"])?;
            let resource: seedbox_api::Simulation =
                serde_json::from_str(&shown).map_err(|e| format!("{name}: {e}: {shown}"))?;
            let expected = if code == 1 {
                seedbox_api::PropertyAssessmentStatus::Failed
            } else {
                seedbox_api::PropertyAssessmentStatus::Incomplete
            };
            let properties = resource.status.properties.as_ref();
            if properties.map(|p| p.status()) != Some(expected)
                || properties.is_some_and(|p| code == 1 && p.lost_count > 0)
            {
                return Err(format!("{name}: {shown}"));
            }
        }
        Ok(())
    });
}

/// The README's spawn example: the oracle adds `replica` mid-run, so the
/// run's faults and full state name a machine its configuration does not.
/// It must PASS, an independent fresh run of its seed must spawn the same
/// machine and MATCH it, and its replay must boot the retained machine and
/// MATCH in full state.
#[test]
fn spawn_example_passes_and_replays_the_spawned_machine() {
    let project = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/spawn");
    let project = project.to_str().unwrap();
    with_server("applications", "spawn", |cli, directory, state| {
        let first_output = first_run(
            cli,
            directory,
            state,
            &["run", project, "--wall-time", "30m"],
        )?;
        let first = simulation_id(&first_output)?;
        let described = invoke(
            cli,
            directory,
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
            cli,
            directory,
            state,
            "second",
            &["run", project, "--seed", &seed, "--wall-time", "30m"],
        )?;
        let second = simulation_id(&second_output)?;
        compare(cli, directory, state, "compare", first, second)?;
        let repeated = invoke(
            cli,
            directory,
            state,
            "replay",
            &["replay", first, "--wall-time", "30m"],
        )?;
        let replayed = repeated
            .lines()
            .find_map(|line| line.strip_prefix("MATCH: "))
            .ok_or_else(|| format!("missing replay MATCH: {repeated}"))?;
        compare(cli, directory, state, "compare-replay", first, replayed)
    });
}

/// A seed-range member of `examples/spawn` runs the source's prepared
/// inputs and spawns from the project, as a fresh run of its seed would.
/// The range must PASS, a fresh run of the member's seed must MATCH it,
/// and the member's replay must boot its own retained machine and MATCH.
#[test]
fn spawn_example_seed_range_members_spawn_and_replay() {
    let project = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/spawn");
    let project = project.to_str().unwrap();
    with_server("applications", "spawn-range", |cli, directory, state| {
        let output = invoke_stdout(
            cli,
            directory,
            state,
            "range",
            &[
                "run",
                project,
                "--count",
                "2",
                "--wall-time",
                "30m",
                "--json",
            ],
        )?;
        let range: seedbox_api::SeedRangeStatus =
            serde_json::from_str(&output).map_err(|error| format!("range: {error}"))?;
        if !range.complete() || range.error.is_some() || range.members.len() != 2 {
            return Err(format!("incomplete range: {output}"));
        }
        for member in &range.members {
            if member.status.verdict() != seedbox_api::TestVerdict::Passed {
                return Err(format!("{} did not PASS: {output}", member.metadata.id));
            }
        }
        let member = range.members[1].metadata.id.as_str();
        let seed = (range.first_seed + 1).to_string();
        let fresh_output = invoke(
            cli,
            directory,
            state,
            "fresh",
            &["run", project, "--seed", &seed, "--wall-time", "30m"],
        )?;
        let fresh = simulation_id(&fresh_output)?;
        compare(cli, directory, state, "compare-fresh", member, fresh)?;
        let repeated = invoke(
            cli,
            directory,
            state,
            "replay",
            &["replay", member, "--wall-time", "30m"],
        )?;
        compare(
            cli,
            directory,
            state,
            "compare-replay",
            member,
            &replayed(&repeated)?,
        )
    });
}

fn application(name: &str) {
    let workload = workload(name);
    let workload = workload.to_str().unwrap();
    // Every case owns its server/catalogue and report directory. Two cases can
    // execute independently without sharing simulation IDs or mutable state.
    let representative = REPEAT_WORKLOADS.contains(&name);
    with_server("applications", name, |cli, directory, state| {
        let first_output = first_run(
            cli,
            directory,
            state,
            &["run", workload, "--wall-time", "30m"],
        )?;
        let first = simulation_id(&first_output)?;
        if name == "device-faults" {
            return faulted(cli, directory, state, first);
        }
        if !representative {
            return Ok(());
        }
        let described = invoke(
            cli,
            directory,
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
            cli,
            directory,
            state,
            "second",
            &["run", workload, "--seed", &seed, "--wall-time", "30m"],
        )?;
        let second = simulation_id(&second_output)?;
        compare(cli, directory, state, "compare", first, second)?;
        let repeated = invoke(
            cli,
            directory,
            state,
            "replay",
            &["replay", first, "--wall-time", "30m"],
        )?;
        let replayed = repeated
            .lines()
            .find_map(|line| line.strip_prefix("MATCH: "))
            .ok_or_else(|| format!("missing replay MATCH: {repeated}"))?;
        compare(cli, directory, state, "compare-replay", first, replayed)?;
        Ok(())
    });
}

/// The oracle applies and heals every fault itself (`device_faults.py`) and
/// observes each from inside the guest; the world view keeps the heals'
/// counts and the victim's crash on a held FLUSH and reboot, and a replay
/// of the run's retained inputs matches it in full state.
fn faulted(cli: &Path, directory: &Path, state: &Path, faulted: &str) -> Result<(), String> {
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
    // The oracle's disk faults in the order it applied them, each with the
    // handle the world gave it.
    let handle = |fault: &&seedbox_api::FaultState| {
        fault
            .plan
            .as_ref()
            .and_then(|plan| plan["handle"].as_str())
            .map(str::to_owned)
    };
    let disk: Vec<_> = world
        .faults
        .iter()
        .filter(|fault| matches!(fault.action.as_str(), "disk_error" | "disk_latency"))
        .collect();
    let phases = [
        ("read-eio", 8),
        ("corrupt", 8),
        ("write-eio", 1),
        ("slow", 4),
    ];
    if disk.len() != phases.len() {
        return Err(format!("expected {} disk faults: {disk:?}", phases.len()));
    }
    for ((phase, at_least), fault) in phases.into_iter().zip(&disk) {
        let applied = handle(fault).ok_or_else(|| format!("{phase} has no handle: {fault:?}"))?;
        let counts = world
            .faults
            .iter()
            .find(|heal| heal.action == "heal" && handle(heal).as_deref() == Some(&applied))
            .and_then(|heal| heal.counts)
            .ok_or_else(|| format!("{phase} ({applied}) heal has no counts"))?;
        if counts.affected < at_least || counts.affected > counts.matched {
            return Err(format!("{phase} heal counts {counts:?}"));
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

/// TEMPORARY PROBE (never merge): repeat workloads' fresh same-seed run and
/// replay in parallel lanes until a deadline, tallying mismatches instead of
/// stopping, and print the offline state diff of every mismatching pair.
#[test]
fn replay_soak() {
    let Some(minutes) = std::env::var("SEEDBOX_TEST_SOAK_MINUTES")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
    else {
        return;
    };
    let lanes = std::env::var("SEEDBOX_TEST_SOAK_LANES")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(2);
    let workloads: Vec<String> = std::env::var("SEEDBOX_TEST_SOAK_WORKLOADS")
        .unwrap_or_else(|_| "properties,profile".into())
        .split(',')
        .map(str::to_owned)
        .collect();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(minutes * 60);
    let tally = std::sync::Mutex::new(Vec::<(String, String, &'static str)>::new());
    std::thread::scope(|scope| {
        for lane in 0..lanes {
            let (tally, workloads) = (&tally, &workloads);
            scope.spawn(move || {
                let mut round = 0usize;
                while std::time::Instant::now() < deadline {
                    let name = &workloads[(round + lane) % workloads.len()];
                    let path = workload(name);
                    let path = path.to_str().unwrap();
                    let label = format!("{name}-l{lane}-r{round}");
                    round += 1;
                    soak_prune();
                    with_server("soak", &label, |cli, directory, state| {
                        let verdicts = soak_round(cli, directory, state, &label, path);
                        tally.lock().unwrap().extend(
                            verdicts
                                .into_iter()
                                .map(|(kind, verdict)| (label.clone(), kind, verdict)),
                        );
                        Ok(())
                    });
                }
            });
        }
    });
    let tally = tally.into_inner().unwrap();
    let bad: Vec<_> = tally.iter().filter(|(_, _, v)| *v != "MATCH").collect();
    println!(
        "SOAK TOTAL {} comparisons, {} not MATCH: {bad:?}",
        tally.len(),
        bad.len()
    );
    assert!(bad.iter().all(|(_, _, v)| *v != "MISMATCH"));
}

fn soak_round(
    cli: &Path,
    directory: &Path,
    state: &Path,
    label: &str,
    workload: &str,
) -> Vec<(String, &'static str)> {
    let lenient = |name: &str, args: &[&str]| -> String {
        invoke(cli, directory, state, name, args).unwrap_or_else(|_| {
            fs::read_to_string(directory.join(format!("{name}.log"))).unwrap_or_default()
        })
    };
    let first_output = lenient("first", &["run", workload, "--wall-time", "30m"]);
    let Ok(first) = simulation_id(&first_output).map(str::to_owned) else {
        println!("SOAK {label} first did not pass: {}", first_output.trim());
        return vec![("first".into(), "ERROR")];
    };
    let described = lenient(
        "describe",
        &["show", "--view", "definition", &first, "--json"],
    );
    let Some(seed) = serde_json::from_str::<seedbox_api::Simulation>(&described)
        .ok()
        .and_then(|resource| resource.spec.seed)
        .map(|seed| seed.to_string())
    else {
        println!("SOAK {label} no seed: {}", described.trim());
        return vec![("describe".into(), "ERROR")];
    };
    let second_output = lenient(
        "second",
        &["run", workload, "--seed", &seed, "--wall-time", "30m"],
    );
    let mut verdicts = Vec::new();
    match simulation_id(&second_output) {
        Ok(second) => {
            let fresh = lenient("compare-fresh", &["compare", &first, second, "--json"]);
            let verdict = soak_verdict(&fresh);
            println!("SOAK {label} seed {seed} fresh {verdict}");
            if verdict != "MATCH" {
                println!("SOAK {label} fresh compare: {}", fresh.trim());
                soak_diff(state, label, "fresh", &first, second);
            }
            verdicts.push(("fresh".into(), verdict));
        }
        Err(_) => {
            println!("SOAK {label} second did not pass: {}", second_output.trim());
            verdicts.push(("second".into(), "ERROR"));
        }
    }
    let replay = lenient("replay", &["replay", &first, "--wall-time", "30m"]);
    let replayed = replay.lines().find_map(|line| {
        line.strip_prefix("MATCH: ")
            .or_else(|| line.strip_prefix("MISMATCH: "))
    });
    let verdict = if replay.lines().any(|line| line.starts_with("MATCH: ")) {
        "MATCH"
    } else if replayed.is_some() {
        "MISMATCH"
    } else {
        "ERROR"
    };
    println!("SOAK {label} seed {seed} replay {verdict}");
    if verdict != "MATCH" {
        println!("SOAK {label} replay output: {}", replay.trim());
        if let Some(replayed) = replayed {
            let compared = lenient("compare-replay", &["compare", &first, replayed, "--json"]);
            println!("SOAK {label} replay compare: {}", compared.trim());
            soak_diff(state, label, "replay", &first, replayed);
        }
    }
    verdicts.push(("replay".into(), verdict));
    verdicts
}

fn soak_verdict(compare: &str) -> &'static str {
    match serde_json::from_str::<seedbox_api::SimulationComparison>(compare) {
        Ok(c) if c.status == seedbox_api::ComparisonStatus::Match => "MATCH",
        Ok(c) if c.status == seedbox_api::ComparisonStatus::Mismatch => "MISMATCH",
        Ok(_) => "NOT_COMPARABLE",
        Err(_) => "ERROR",
    }
}

fn soak_attempt(state: &Path, id: &str) -> Option<std::path::PathBuf> {
    for session in fs::read_dir(state.join("sessions")).ok()? {
        let attempts = session.ok()?.path().join("runs").join(id).join("attempts");
        if let Ok(entries) = fs::read_dir(&attempts) {
            for entry in entries.flatten() {
                if entry.path().is_dir() {
                    return Some(entry.path());
                }
            }
        }
    }
    None
}

fn soak_diff(state: &Path, label: &str, kind: &str, a: &str, b: &str) {
    let (Some(left), Some(right)) = (soak_attempt(state, a), soak_attempt(state, b)) else {
        println!("SOAK {label} {kind} diff: attempts not found");
        return;
    };
    let show = |result: Result<String, String>| result.unwrap_or_else(|e| format!("error: {e}"));
    let report = show(
        seedbox_application::diff::diff(&left, &right)
            .map_err(|e| e.to_string())
            .and_then(|r| serde_json::to_string(&r).map_err(|e| e.to_string())),
    );
    println!(
        "SOAK {label} {kind} diff: {}",
        &report[..report.len().min(200_000)]
    );
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(&report) {
        for machine in value["machines"].as_array().into_iter().flatten() {
            for difference in machine["differences"].as_array().into_iter().flatten() {
                if difference["field"] == "/state_digest_parts/ram_pages" {
                    soak_ram(
                        label,
                        machine["machine"].as_str().unwrap_or("?"),
                        difference["a"].as_str().unwrap_or(""),
                        difference["b"].as_str().unwrap_or(""),
                    );
                }
            }
        }
    }
    let pages = show(
        seedbox_application::diff::replay_ram_pages(&left, &right)
            .map_err(|e| e.to_string())
            .and_then(|r| serde_json::to_string(&r).map_err(|e| e.to_string())),
    );
    println!(
        "SOAK {label} {kind} pages: {}",
        &pages[..pages.len().min(200_000)]
    );
    // List the attempt files so a later probe knows what is retained.
    for (side, dir) in [("a", &left), ("b", &right)] {
        let mut names = Vec::new();
        let mut stack = vec![dir.clone()];
        while let Some(path) = stack.pop() {
            for entry in fs::read_dir(&path).into_iter().flatten().flatten() {
                let p = entry.path();
                if p.is_dir() {
                    stack.push(p);
                } else {
                    let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
                    names.push(format!("{}={size}", p.strip_prefix(dir).unwrap().display()));
                }
            }
        }
        names.sort();
        println!("SOAK {label} {kind} files {side}: {names:?}");
    }
}

/// PROBE ONLY: RAM images the probe worker kept, by page digest.
const SOAK_RAM: &str = "/tmp/sbx-probe";

fn soak_prune() {
    for entry in fs::read_dir(SOAK_RAM).into_iter().flatten().flatten() {
        let old = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|age| age > std::time::Duration::from_secs(900));
        if old {
            let _ = fs::remove_file(entry.path());
        }
    }
}

fn soak_pages(digest: &str) -> Option<std::collections::BTreeMap<u64, Vec<u8>>> {
    let bytes = fs::read(Path::new(SOAK_RAM).join(format!("{digest}.ram"))).ok()?;
    Some(
        bytes
            .chunks(8 + 4096)
            .map(|c| {
                (
                    u64::from_le_bytes(c[..8].try_into().unwrap()),
                    c[8..].to_vec(),
                )
            })
            .collect(),
    )
}

fn soak_ram(label: &str, machine: &str, a: &str, b: &str) {
    let (Some(a), Some(b)) = (soak_pages(a), soak_pages(b)) else {
        println!("SOAK {label} ram {machine}: images missing");
        return;
    };
    let zero = vec![0u8; 4096];
    let keys: std::collections::BTreeSet<u64> = a.keys().chain(b.keys()).copied().collect();
    let differing: Vec<u64> = keys
        .into_iter()
        .filter(|k| a.get(k).unwrap_or(&zero) != b.get(k).unwrap_or(&zero))
        .collect();
    println!(
        "SOAK {label} ram {machine}: {} differing pages {:x?}",
        differing.len(),
        &differing[..differing.len().min(32)]
    );
    let hex = |bytes: &[u8]| bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();
    for gpa in differing.iter().take(4) {
        let (x, y) = (a.get(gpa).unwrap_or(&zero), b.get(gpa).unwrap_or(&zero));
        let offsets: Vec<usize> = (0..4096).filter(|&i| x[i] != y[i]).collect();
        println!(
            "SOAK {label} ram {machine} page {gpa:#x}: {} bytes differ at {:x?}",
            offsets.len(),
            &offsets[..offsets.len().min(64)]
        );
        for row in (0..4096).step_by(64) {
            println!(
                "SOAK {label} ram {machine} page {gpa:#x} +{row:03x} a={} b={}",
                hex(&x[row..row + 64]),
                hex(&y[row..row + 64])
            );
        }
    }
}
