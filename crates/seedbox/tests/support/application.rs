//! CLI and owned-server helpers shared by the application acceptance targets.
use super::{failure_capture, json_diff};
use seedbox_test::process::Guarded;
use std::{
    fs,
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
};

/// The release CLI and fresh report directory the checker supplies.
pub fn environment() -> (PathBuf, PathBuf) {
    let cli = std::env::var_os("SEEDBOX_TEST_CLI")
        .expect("harness/check.sh supplies the current release CLI");
    let reports =
        std::env::var_os("SEEDBOX_TEST_REPORT_DIR").expect("set a fresh SEEDBOX_TEST_REPORT_DIR");
    (cli.into(), reports.into())
}

/// Run one CLI command with stdout and stderr in `NAME.log`; returns both.
pub fn invoke(
    cli: &Path,
    directory: &Path,
    state: &Path,
    name: &str,
    args: &[&str],
) -> Result<String, String> {
    let logfile = directory.join(format!("{name}.log"));
    let log = fs::File::create(&logfile).map_err(|error| error.to_string())?;
    let stderr = log.try_clone().map_err(|error| error.to_string())?;
    run(cli, directory, state, name, args, log, stderr, &[&logfile])?;
    Ok(
        String::from_utf8_lossy(&fs::read(&logfile).map_err(|error| error.to_string())?)
            .into_owned(),
    )
}

/// Like [`invoke`], but returns stdout alone (`NAME.log`; stderr goes to
/// `NAME.stderr`), so a stderr diagnostic cannot break `--json` parsing.
pub fn invoke_stdout(
    cli: &Path,
    directory: &Path,
    state: &Path,
    name: &str,
    args: &[&str],
) -> Result<String, String> {
    let logfile = directory.join(format!("{name}.log"));
    let errfile = directory.join(format!("{name}.stderr"));
    let log = fs::File::create(&logfile).map_err(|error| error.to_string())?;
    let stderr = fs::File::create(&errfile).map_err(|error| error.to_string())?;
    run(
        cli,
        directory,
        state,
        name,
        args,
        log,
        stderr,
        &[&logfile, &errfile],
    )?;
    Ok(
        String::from_utf8_lossy(&fs::read(&logfile).map_err(|error| error.to_string())?)
            .into_owned(),
    )
}

#[allow(clippy::too_many_arguments)]
fn run(
    cli: &Path,
    directory: &Path,
    state: &Path,
    name: &str,
    args: &[&str],
    stdout: fs::File,
    stderr: fs::File,
    logs: &[&Path],
) -> Result<(), String> {
    eprintln!("application: {} / {name}", directory.display());
    let mut command = Command::new(cli);
    command
        .args(args)
        .env_remove("SEEDBOX_VMM")
        .env("SEEDBOX_STATE", state)
        .stdout(stdout)
        .stderr(stderr);
    let status = command
        .guarded_status()
        .map_err(|error| format!("{}: {error}", cli.display()))?;
    if !status.success() {
        // CI keeps only the console, so show the end of the failing logs there.
        for logfile in logs {
            let text = fs::read(logfile).map(|bytes| String::from_utf8_lossy(&bytes).into_owned());
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
        }
        return Err(format!("{name}: {status}; see {}", logs[0].display()));
    }
    Ok(())
}

pub fn first_run(
    cli: &Path,
    directory: &Path,
    state: &Path,
    args: &[&str],
) -> Result<String, String> {
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

pub fn simulation_id(output: &str) -> Result<&str, String> {
    output
        .lines()
        .find_map(|line| line.strip_prefix("PASS: "))
        .ok_or_else(|| format!("missing PASS simulation ID: {output}"))
}

/// The simulation a `replay` reran, from its `MATCH:` status line.
pub fn replayed(output: &str) -> Result<String, String> {
    output
        .lines()
        .find_map(|line| line.strip_prefix("MATCH: "))
        .map(str::to_owned)
        .ok_or_else(|| format!("missing MATCH simulation ID: {output}"))
}

/// A MATCH, with matching full state.
pub fn require_full_state_match(output: &str) -> Result<(), String> {
    let comparison: seedbox_api::SimulationComparison =
        serde_json::from_str(output).map_err(|error| format!("comparison: {error}"))?;
    if comparison.full_state_matches != Some(true) {
        return Err(format!("missing full-state MATCH: {output}"));
    }
    if comparison.status != seedbox_api::ComparisonStatus::Match {
        return Err(format!("missing MATCH: {output}"));
    }
    Ok(())
}

/// The one line of the oracle's retained output starting with `prefix`.
pub fn oracle_line(
    cli: &Path,
    directory: &Path,
    state: &Path,
    name: &str,
    id: &str,
    prefix: &str,
) -> Result<String, String> {
    let logs = invoke_stdout(cli, directory, state, name, &["logs", id, "oracle"])?;
    let mut lines = logs.lines().filter(|line| line.starts_with(prefix));
    match (lines.next(), lines.next()) {
        (Some(line), None) => Ok(line.to_owned()),
        _ => Err(format!("{id}: expected one {prefix:?} oracle line")),
    }
}

// Capture both sides while the session is still alive. The CI artifact collector
// bounds these files; shutdown would otherwise remove the evidence we need.
pub fn compare(
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

/// Run `check` against a fresh owned server in a short state path (macOS
/// limits Unix socket paths to 104 bytes), with its own report directory.
/// A failure keeps the server state for diagnosis.
pub fn with_server(
    group: &str,
    name: &str,
    check: impl FnOnce(&Path, &Path, &Path) -> Result<(), String>,
) {
    let (cli, reports) = environment();
    let state_directory = tempfile::Builder::new()
        .prefix("seedbox-apps-")
        .tempdir_in("/tmp")
        .unwrap();
    // Let the daemon create its own state directory with private permissions.
    let state_path = state_directory.path().join("state");
    let state = state_path.as_path();
    let server = match Server::start(&cli, state) {
        Ok(server) => server,
        Err(error) => {
            let retained = state_directory.keep();
            panic!("{error}; retained server state: {}", retained.display());
        }
    };
    let directory = reports.join(group).join(name);
    fs::create_dir_all(&directory).unwrap();
    let result = check(&cli, &directory, state);
    drop(server);
    if let Err(error) = result {
        let retained = state_directory.keep();
        panic!(
            "{name}: {error}\nretained server state: {}",
            retained.display()
        );
    }
}

/// The acceptance fixture owns its server process; retained reports outlive it.
pub struct Server(Child);
impl Server {
    pub fn start(cli: &Path, state: &Path) -> Result<Self, String> {
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
