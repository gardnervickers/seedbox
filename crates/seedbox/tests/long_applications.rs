//! Long-running application tier: `harness/acceptance/long` workloads take tens of
//! minutes each, so they stay out of `applications` and its sub-minute corpus. Each
//! runs once through the server, then reruns and must match its full-state witness.
#![cfg(any(
    all(target_os = "linux", target_arch = "x86_64"),
    all(target_os = "macos", target_arch = "aarch64")
))]
use std::{fs, path::Path, time::Instant};

mod support;
use support::{compare, invoke, simulation_id, Server};

#[test]
fn long_applications_pass_and_rerun_identically() {
    let cli = std::env::var_os("SEEDBOX_TEST_CLI")
        .expect("harness/check.sh supplies the current release CLI");
    let reports =
        std::env::var_os("SEEDBOX_TEST_REPORT_DIR").expect("set a fresh SEEDBOX_TEST_REPORT_DIR");
    let cli = Path::new(&cli);
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../harness/acceptance/long");
    let mut workloads: Vec<_> = fs::read_dir(root)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "yaml")
        })
        .collect();
    workloads.sort();
    assert!(!workloads.is_empty());
    // macOS limits Unix socket paths to 104 bytes; keep the state path short.
    let state_directory = tempfile::Builder::new()
        .prefix("seedbox-long-")
        .tempdir_in("/tmp")
        .unwrap();
    let state_path = state_directory.path().join("state");
    let state = state_path.as_path();
    let server = match Server::start(cli, state) {
        Ok(server) => server,
        Err(error) => {
            let retained = state_directory.keep();
            panic!("{error}; retained server state: {}", retained.display());
        }
    };
    let mut failures = Vec::new();
    for workload in workloads {
        let directory = Path::new(&reports)
            .join("long-applications")
            .join(workload.file_stem().unwrap());
        fs::create_dir_all(&directory).unwrap();
        let check = || -> Result<(), String> {
            let started = Instant::now();
            let first = invoke(
                cli,
                &directory,
                state,
                "run",
                &["run", workload.to_str().unwrap(), "--wall-time", "90m"],
            )?;
            let first = simulation_id(&first)?.to_owned();
            eprintln!(
                "long application: {} {first} passed in {:.0}s",
                workload.display(),
                started.elapsed().as_secs_f64()
            );
            let started = Instant::now();
            let repeated = invoke(
                cli,
                &directory,
                state,
                "rerun",
                &["rerun", &first, "--wall-time", "90m"],
            )?;
            let repeated = simulation_id(&repeated)?.to_owned();
            eprintln!(
                "long application: {} rerun {repeated} passed in {:.0}s",
                workload.display(),
                started.elapsed().as_secs_f64()
            );
            compare(cli, &directory, state, "compare", &first, &repeated)
        };
        if let Err(error) = check() {
            failures.push(format!("{}: {error}", workload.display()));
        }
    }
    drop(server);
    if !failures.is_empty() {
        let retained = state_directory.keep();
        panic!(
            "{}\nretained server state: {}",
            failures.join("\n"),
            retained.display()
        );
    }
}
