//! Long-running application tier, run by `harness/check.sh mac-apps-long` or
//! `linux-apps-long` and never by default. `harness/acceptance/long` holds the
//! workloads that take minutes, or whose coverage overlaps the ordinary corpus
//! enough not to justify its CI time: each runs once through its own server,
//! then replays its retained inputs and must match its full-state witness.
//! Derived cases start from a workload (in either directory) and branch, crash
//! or edit it (support/derived.rs); each takes a minute or more.
#![cfg(any(
    all(target_os = "linux", target_arch = "x86_64"),
    all(target_os = "macos", target_arch = "aarch64")
))]
use std::{collections::HashSet, fs, path::Path, time::Instant};

pub mod support;
use support::application::{compare, first_run, invoke, replayed, simulation_id, with_server};
use support::derived;

fn acceptance() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../harness/acceptance")
}

macro_rules! long_applications {
    ($($(#[$attribute:meta])* $case:ident => $workload:literal),+ $(,)?) => {
        const WORKLOADS: &[&str] = &[$($workload),+];
        $($(#[$attribute])* #[test] fn $case() { long_application($workload); })+
    };
}
long_applications! {
    checkpoint_altstack => "checkpoint-altstack",
    checkpoint_links => "checkpoint-links",
    checkpoint_pending_stop => "checkpoint-pending-stop",
    checkpoint_tcp => "checkpoint-tcp",
    checkpoint_throttle => "checkpoint-throttle",
    checkpoint_waits => "checkpoint-waits",
    crash_peer => "crash-peer",
    disk_rebuild => "disk-rebuild",
    durability => "durability",
    etcd_lease => "etcd-lease",
    kafka_idempotent_retry => "kafka-idempotent-retry",
    kafka_startup_crash => "kafka-startup-crash",
    large_volume => "large-volume",
    logs_leader_exit => "logs-leader-exit",
    logs_storm => "logs-storm",
    postgres_cancel => "postgres-cancel",
    redis_aof => "redis-aof",
    sqlite_wal => "sqlite-wal",
    two_crash => "two-crash",
}

/// Simulations derived from a workload run rather than submitted from its
/// file, each with its own server.
macro_rules! derived {
    ($($(#[$attribute:meta])* $case:ident => $check:ident($workload:literal)),+ $(,)?) => {
        const DERIVED: &[&str] = &[$($workload),+];
        $($(#[$attribute])* #[test] fn $case() {
            with_server("derived", stringify!($case), |cli, directory, state| {
                derived::$check(cli, directory, state, &workload($workload))
            });
        })+
    };
}
derived! {
    checkpoint_altstack_branches => checkpoint_branches("checkpoint-altstack"),
    checkpoint_links_branches => checkpoint_branches("checkpoint-links"),
    checkpoint_pending_stop_branches => checkpoint_branches("checkpoint-pending-stop"),
    checkpoint_tcp_branches => checkpoint_branches("checkpoint-tcp"),
    checkpoint_throttle_branches => checkpoint_branches("checkpoint-throttle"),
    checkpoint_waits_branches => checkpoint_branches("checkpoint-waits"),
    durability_torn_writes => torn_writes("durability"),
    two_crash_torn_writes => torn_writes("two-crash"),
    stale_ack_crash_while_stop_held => crashed_pending_stop("stale-ack"),
    logs_storm_retained_and_replayed => retained_output("logs-storm"),
    logs_leader_exit_retained_and_replayed => retained_output("logs-leader-exit"),
}

/// A workload from the ordinary corpus, or else from the long tier.
fn workload(name: &str) -> std::path::PathBuf {
    let file = format!("{name}.yaml");
    let ordinary = acceptance().join("workloads").join(&file);
    if ordinary.is_file() {
        ordinary
    } else {
        acceptance().join("long").join(file)
    }
}

fn stems(directory: &Path) -> HashSet<String> {
    fs::read_dir(directory)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "yaml"))
        .map(|path| path.file_stem().unwrap().to_str().unwrap().to_owned())
        .collect()
}

#[test]
fn every_long_workload_and_checkpoint_workload_has_a_case() {
    assert_eq!(
        stems(&acceptance().join("long")),
        WORKLOADS.iter().map(|name| (*name).to_owned()).collect()
    );
    let ordinary = stems(&acceptance().join("workloads"));
    assert!(DERIVED
        .iter()
        .all(|name| ordinary.contains(*name) || WORKLOADS.contains(name)));
    // Every checkpoint workload is also branched from its marked phase.
    let long = stems(&acceptance().join("long"));
    for name in ordinary
        .iter()
        .chain(&long)
        .filter(|name| name.starts_with("checkpoint-"))
    {
        assert!(
            DERIVED.contains(&name.as_str()),
            "{name} has no branch check"
        );
    }
}

fn long_application(name: &str) {
    let workload = workload(name);
    let workload = workload.to_str().unwrap();
    with_server("long-applications", name, |cli, directory, state| {
        let started = Instant::now();
        let first = first_run(
            cli,
            directory,
            state,
            &["run", workload, "--wall-time", "90m"],
        )?;
        let first = simulation_id(&first)?.to_owned();
        eprintln!(
            "long application: {name} {first} passed in {:.0}s",
            started.elapsed().as_secs_f64()
        );
        let started = Instant::now();
        let repeated = invoke(
            cli,
            directory,
            state,
            "replay",
            &["replay", &first, "--wall-time", "90m"],
        )?;
        let repeated = replayed(&repeated)?;
        eprintln!(
            "long application: {name} replay {repeated} matched in {:.0}s",
            started.elapsed().as_secs_f64()
        );
        compare(cli, directory, state, "compare", &first, &repeated)
    });
}
