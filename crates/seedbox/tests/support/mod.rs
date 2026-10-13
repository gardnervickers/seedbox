//! Shared CLI and server helpers for the application acceptance targets.
#![allow(dead_code)] // Each test target uses its own subset.
use std::{
    fs,
    io::{BufRead, BufReader},
    path::Path,
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

/// Run one CLI command, keeping its stdout and stderr in separate report files.
/// Returns stdout only: `--json` documents and status lines never include
/// stderr diagnostics such as the dirty-build warning.
pub fn invoke(
    cli: &Path,
    directory: &Path,
    state: &Path,
    name: &str,
    args: &[&str],
) -> Result<String, String> {
    let logfile = directory.join(format!("{name}.log"));
    let errfile = directory.join(format!("{name}.stderr.log"));
    let log = fs::File::create(&logfile).map_err(|error| error.to_string())?;
    let err = fs::File::create(&errfile).map_err(|error| error.to_string())?;
    eprintln!("application: {} / {name}", directory.display());
    let mut command = Command::new(cli);
    command
        .args(args)
        .env("SEEDBOX_STATE", state)
        .stdout(log)
        .stderr(err);
    let status = command
        .status()
        .map_err(|error| format!("{}: {error}", cli.display()))?;
    if !status.success() {
        // CI keeps only the console, so show the end of the failing logs there.
        for file in [&logfile, &errfile] {
            let Ok(bytes) = fs::read(file) else { continue };
            let text = String::from_utf8_lossy(&bytes);
            let lines: Vec<_> = text.lines().collect();
            let tail = &lines[lines.len().saturating_sub(80)..];
            eprintln!(
                "--- {name} failed; last {} lines of {}:",
                tail.len(),
                file.display()
            );
            for line in tail {
                eprintln!("{line}");
            }
            eprintln!("--- end of {name}");
        }
        return Err(format!(
            "{name}: {status}; see {} and {}",
            logfile.display(),
            errfile.display()
        ));
    }
    Ok(
        String::from_utf8_lossy(&fs::read(&logfile).map_err(|error| error.to_string())?)
            .into_owned(),
    )
}

pub fn simulation_id(output: &str) -> Result<&str, String> {
    output
        .lines()
        .find_map(|line| line.strip_prefix("PASS: "))
        .ok_or_else(|| format!("missing PASS simulation ID: {output}"))
}

pub fn require_full_state_match(output: &str) -> Result<(), String> {
    let comparison: seedbox_api::SimulationComparison =
        serde_json::from_str(output).map_err(|error| format!("comparison: {error}"))?;
    if comparison.full_state_matches != Some(true) {
        return Err(format!("missing full-state MATCH: {output}"));
    }
    Ok(())
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

/// The acceptance fixture owns its server process; retained reports outlive it.
pub struct Server(Child);
impl Server {
    pub fn start(cli: &Path, state: &Path) -> Result<Self, String> {
        let mut server = Self(
            Command::new(cli)
                .args(["serve", "--startup-handshake", "--full-state"])
                .env("SEEDBOX_STATE", state)
                .stdout(Stdio::piped())
                .spawn()
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
        let deadline = Instant::now() + Duration::from_secs(30);
        while Instant::now() < deadline {
            if self.0.try_wait().ok().flatten().is_some() {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
