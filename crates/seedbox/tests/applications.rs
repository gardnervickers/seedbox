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

/// TEMPORARY PROBE (never merge): repeat the `profile` workload's fresh
/// same-seed run and replay, tallying replay mismatches instead of stopping.
#[test]
fn profile_soak() {
    let Some(rounds) = std::env::var("SEEDBOX_TEST_SOAK_ROUNDS")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
    else {
        return;
    };
    let workload = workload("profile");
    let workload = workload.to_str().unwrap();
    let tally = std::sync::Mutex::new(Vec::new());
    for round in 0..rounds {
        with_server(
            "soak",
            &format!("profile-{round}"),
            |cli, directory, state| {
                let lenient = |name: &str, args: &[&str]| -> String {
                    invoke(cli, directory, state, name, args).unwrap_or_else(|_| {
                        fs::read_to_string(directory.join(format!("{name}.log")))
                            .unwrap_or_default()
                    })
                };
                let first_output = lenient("first", &["run", workload, "--wall-time", "30m"]);
                let Ok(first) = simulation_id(&first_output).map(str::to_owned) else {
                    println!(
                        "SOAK round {round} first did not pass: {}",
                        first_output.trim()
                    );
                    tally.lock().unwrap().push((String::new(), "ERROR"));
                    return Ok(());
                };
                let described = lenient(
                    "describe",
                    &["show", "--view", "definition", &first, "--json"],
                );
                let resource: seedbox_api::Simulation =
                    serde_json::from_str(&described).map_err(|error| error.to_string())?;
                let seed = resource.spec.seed.ok_or("no seed")?.to_string();
                let second_output = lenient(
                    "second",
                    &["run", workload, "--seed", &seed, "--wall-time", "30m"],
                );
                let Ok(second) = simulation_id(&second_output).map(str::to_owned) else {
                    println!(
                        "SOAK round {round} second did not pass: {}",
                        second_output.trim()
                    );
                    tally.lock().unwrap().push((seed, "ERROR"));
                    return Ok(());
                };
                let fresh = lenient("compare-fresh", &["compare", &first, &second, "--json"]);
                let replay = lenient("replay", &["replay", &first, "--wall-time", "30m"]);
                let replayed = replay
                    .lines()
                    .find_map(|line| {
                        line.strip_prefix("MATCH: ")
                            .or_else(|| line.strip_prefix("MISMATCH: "))
                    })
                    .map(str::to_owned);
                let verdict = if replay.lines().any(|line| line.starts_with("MATCH: ")) {
                    "MATCH"
                } else {
                    "MISMATCH"
                };
                println!("SOAK round {round} seed {seed} fresh: {}", fresh.trim());
                println!(
                    "SOAK round {round} seed {seed} replay {verdict}: {}",
                    replay.trim()
                );
                if let (Some(replayed), "MISMATCH") = (replayed.as_deref(), verdict) {
                    for (name, args) in [
                        ("cmp-replay", vec!["compare", &first, replayed, "--json"]),
                        ("show-first", vec!["show", &first, "--json"]),
                        ("show-replayed", vec!["show", replayed, "--json"]),
                    ] {
                        println!("SOAK round {round} {name}: {}", lenient(name, &args).trim());
                    }
                }
                tally.lock().unwrap().push((seed, verdict));
                Ok(())
            },
        );
    }
    let tally = tally.into_inner().unwrap();
    let mismatches = tally.iter().filter(|(_, v)| *v == "MISMATCH").count();
    println!(
        "SOAK TOTAL {} rounds, {mismatches} replay mismatches: {tally:?}",
        tally.len()
    );
    assert_eq!(mismatches, 0);
}
