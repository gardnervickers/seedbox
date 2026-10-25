//! Each native workload is independently selectable; CLI/guest checks own its verdict.
#![cfg(any(
    all(target_os = "linux", target_arch = "x86_64"),
    all(target_os = "macos", target_arch = "aarch64")
))]
use std::{collections::HashSet, fs, path::Path, process::Command};

pub mod support;
use support::application::{compare, first_run, invoke, simulation_id, with_server};
use support::failure_capture;

// Every discovered workload proves a PASS. These four retain the expensive
// independent same-seed and retained-input comparisons, covering dynamic
// network faults, volume durability across crashes, clock state across reboot,
// and concurrent application behavior.
const REPEAT_WORKLOADS: [&str; 4] = ["network-faults", "volume-crash", "clock-reboot", "race"];

macro_rules! applications {
    ($($(#[$attribute:meta])* $case:ident => $workload:literal),+ $(,)?) => {
        const WORKLOADS: &[&str] = &[$($workload),+];
        $($(#[$attribute])* #[test] fn $case() { application($workload); })+
    };
}
applications! {
    bandwidth => "bandwidth",
    checkpoint_altstack => "checkpoint-altstack",
    checkpoint_links => "checkpoint-links",
    checkpoint_pending_stop => "checkpoint-pending-stop",
    checkpoint_tcp => "checkpoint-tcp",
    checkpoint_throttle => "checkpoint-throttle",
    checkpoint_waits => "checkpoint-waits",
    clock_drift => "clock-drift",
    clock_reboot => "clock-reboot",
    clock_skew => "clock-skew",
    clock_step => "clock-step",
    connections => "connections",
    crash_peer => "crash-peer",
    disk_rebuild => "disk-rebuild",
    enospc => "enospc",
    exclusives => "exclusives",
    key_value => "key-value",
    #[ignore = "guest inspect reports the app Running after its leader exits while an orphan writes (the server world API reports it exited); follow-up: report application exits through inspect"]
    logs_leader_exit => "logs-leader-exit",
    logs_storm => "logs-storm",
    mac_capabilities => "mac-capabilities",
    network_faults => "network-faults",
    pause_kinds => "pause-kinds",
    process_groups => "process-groups",
    race => "race",
    runtime => "runtime",
    slow_machine => "slow-machine",
    stale_ack => "stale-ack",
    triggers => "triggers",
    two_crash => "two-crash",
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
