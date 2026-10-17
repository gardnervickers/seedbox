//! Discover native workloads; the public CLI and guest checks own each verdict.
#![cfg(any(
    all(target_os = "linux", target_arch = "x86_64"),
    all(target_os = "macos", target_arch = "aarch64")
))]
use seedbox_test::process::Guarded;
use std::{
    fs,
    io::{BufRead, BufReader},
    path::Path,
    process::{Child, Command, Stdio},
};

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
    if result.is_err() {
        for (side, id) in [("baseline", baseline), ("repeated", repeated)] {
            for (kind, args) in [
                ("definition", vec!["describe", "simulation", id, "--json"]),
                ("world", vec!["get", "world", id, "--json"]),
                ("timeline", vec!["get", "timeline", id, "--json"]),
                ("logs", vec!["logs", id, "--tail", "100"]),
            ] {
                if let Err(error) = invoke(
                    cli,
                    directory,
                    state,
                    &format!("{name}-{side}-{kind}"),
                    &args,
                ) {
                    eprintln!("comparison diagnostic: {error}");
                }
            }
        }
    }
    result
}

#[test]
fn every_application_runs_twice_and_reruns_through_the_server() {
    let cli = std::env::var_os("SEEDBOX_TEST_CLI")
        .expect("harness/check.sh supplies the current release CLI");
    let reports =
        std::env::var_os("SEEDBOX_TEST_REPORT_DIR").expect("set a fresh SEEDBOX_TEST_REPORT_DIR");
    let workloads =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../harness/acceptance/workloads");
    let mut workloads: Vec<_> = fs::read_dir(workloads)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "yaml")
        })
        .collect();
    workloads.sort();
    assert!(!workloads.is_empty());
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
    let mut failures = Vec::new();
    for workload in workloads {
        let directory = Path::new(&reports)
            .join("applications")
            .join(workload.file_stem().unwrap());
        fs::create_dir_all(&directory).unwrap();
        let check = || -> Result<(), String> {
            let first_output = invoke(
                Path::new(&cli),
                &directory,
                state,
                "first",
                &["run", workload.to_str().unwrap(), "--wall-time", "30m"],
            )?;
            let first = simulation_id(&first_output)?;
            let described = invoke(
                Path::new(&cli),
                &directory,
                state,
                "describe-first",
                &["describe", "simulation", first, "--json"],
            )?;
            let resource: seedbox_api::Simulation = serde_json::from_str(&described)
                .map_err(|error| format!("describe first: {error}"))?;
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
                "rerun",
                &["rerun", first, "--wall-time", "30m"],
            )?;
            compare(
                Path::new(&cli),
                &directory,
                state,
                "compare-rerun",
                first,
                simulation_id(&repeated)?,
            )?;
            Ok(())
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

/// The acceptance fixture owns its server process; retained reports outlive it.
struct Server(Child);
impl Server {
    fn start(cli: &Path, state: &Path) -> Result<Self, String> {
        let mut server = Self(
            Command::new(cli)
                .args(["serve", "--startup-handshake", "--full-state"])
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
        self.0.wait().expect("reap owned application server");
    }
}
