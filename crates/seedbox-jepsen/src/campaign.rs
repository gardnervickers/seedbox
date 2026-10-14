//! A host-side seed campaign: run scenarios over seeds, collect each
//! simulation's logs and events as soon as it completes (the server keeps
//! application logs in one shared 64 MiB budget), replay a sample and compare
//! every split log byte for byte, export bundles for hits, delete the
//! simulations (each is about 1 GB) and print a summary table.
//!
//! This is research tooling, not an acceptance runner. It drives the `seedbox`
//! CLI given by `--seedbox` rather than linking the client library: the CLI
//! prepares projects and owns the server lifecycle, and the binary under test
//! may be a different build from this checkout.
use crate::kit;
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, VecDeque},
    fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{Arc, Mutex},
    time::Instant,
};

#[derive(clap::Args, Debug, Clone)]
pub struct Args {
    /// Jobs as SCENARIO.yaml:SEEDS, where SEEDS is `N`, `A-B` or `A,B,C`.
    #[arg(required = true)]
    pub jobs: Vec<String>,
    /// The seedbox CLI to run.
    #[arg(long, default_value = "seedbox")]
    pub seedbox: PathBuf,
    /// Server state directory (SEEDBOX_STATE for every command).
    #[arg(long)]
    pub state: PathBuf,
    /// Docker-compatible engine socket URL (DOCKER_HOST for every command).
    #[arg(long)]
    pub engine: Option<String>,
    /// Directory for per-run evidence, bundles and the summary.
    #[arg(long)]
    pub out: PathBuf,
    /// Simulations at once (a replay counts as one).
    #[arg(long, default_value_t = 2)]
    pub lanes: usize,
    /// Replay the first N runs of each scenario.
    #[arg(long, default_value_t = 1)]
    pub replay_first: usize,
    /// Also replay every run whose oracle reported a violation.
    #[arg(long)]
    pub replay_hits: bool,
    /// Export bundles for up to this many runs per scenario whose oracle
    /// reported a violation (bundles are hundreds of MB each).
    #[arg(long, default_value_t = 0)]
    pub export_hits: usize,
    /// Keep the simulations instead of deleting them.
    #[arg(long)]
    pub keep: bool,
    /// Stop launching when the filesystem holding --state has less free space.
    #[arg(long, default_value_t = 40)]
    pub min_free_gb: u64,
    /// Host wall-time limit for each run and replay, as the CLI accepts it.
    #[arg(long, default_value = "7200s")]
    pub wall_time: String,
    /// Start `seedbox serve` when no server answers.
    #[arg(long)]
    pub serve: bool,
}

#[derive(Clone, Debug)]
struct Job {
    scenario: PathBuf,
    seed: u64,
    replay: bool,
    label: String,
}

fn seeds(text: &str) -> Result<Vec<u64>, String> {
    let mut out = Vec::new();
    for part in text.split(',') {
        match part.split_once('-') {
            Some((a, b)) => {
                let (a, b): (u64, u64) = (
                    a.parse().map_err(|_| format!("bad seed {a}"))?,
                    b.parse().map_err(|_| format!("bad seed {b}"))?,
                );
                out.extend(a..=b);
            }
            None => out.push(part.parse().map_err(|_| format!("bad seed {part}"))?),
        }
    }
    Ok(out)
}

fn plan(args: &Args) -> Result<Vec<Job>, String> {
    let mut jobs = Vec::new();
    for spec in &args.jobs {
        let (path, list) = spec
            .rsplit_once(':')
            .ok_or_else(|| format!("{spec}: expected SCENARIO.yaml:SEEDS"))?;
        let scenario = PathBuf::from(path);
        if !scenario.is_file() {
            return Err(format!("{path}: not found"));
        }
        let stem = scenario
            .file_stem()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string();
        for (index, seed) in seeds(list)?.into_iter().enumerate() {
            jobs.push(Job {
                label: format!("{stem}-s{seed}"),
                scenario: scenario.clone(),
                seed,
                replay: index < args.replay_first,
            });
        }
    }
    Ok(jobs)
}

struct Cli<'a> {
    args: &'a Args,
}

impl Cli<'_> {
    fn command(&self) -> Command {
        let mut command = Command::new(&self.args.seedbox);
        command.env("SEEDBOX_STATE", &self.args.state);
        if let Some(engine) = &self.args.engine {
            command.env("DOCKER_HOST", engine);
        }
        command
    }

    /// Run to completion with stdout to `stdout` and stderr to `stderr`; the exit code.
    fn run(&self, args: &[&str], stdout: &Path, stderr: &Path) -> i32 {
        let files = fs::File::create(stdout).and_then(|out| Ok((out, fs::File::create(stderr)?)));
        let Ok((out, err)) = files else {
            return -1;
        };
        self.command()
            .args(args)
            .stdin(Stdio::null())
            .stdout(out)
            .stderr(err)
            .status()
            .map(|s| s.code().unwrap_or(-1))
            .unwrap_or(-1)
    }

    fn quiet(&self, args: &[&str]) -> bool {
        self.command()
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    }
}

/// Split `seedbox logs ID --json` records into `MACHINE-INCARNATION-STREAM.txt` files.
pub fn split_logs(dir: &Path) -> Result<Vec<String>, String> {
    let text = fs::read_to_string(dir.join("logs.jsonl")).map_err(|e| e.to_string())?;
    let mut streams: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    let mut gaps = 0;
    for line in text.lines() {
        let Ok(item) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let entry = if item.get("entry").is_some() {
            &item["entry"]
        } else {
            &item
        };
        match entry["type"].as_str() {
            Some("bytes") => {
                let name = format!(
                    "{}-{}-{}",
                    entry["machine"].as_str().unwrap_or("?"),
                    entry["incarnation"],
                    entry["stream"].as_str().unwrap_or("?")
                );
                let bytes = entry["bytes"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|b| b.as_u64())
                    .map(|b| b as u8);
                streams.entry(name).or_default().extend(bytes);
            }
            Some("gap") => gaps += 1,
            _ => {}
        }
    }
    for (name, data) in &streams {
        fs::write(dir.join(format!("{name}.txt")), data).map_err(|e| e.to_string())?;
    }
    let mut names: Vec<String> = streams.into_keys().collect();
    if gaps > 0 {
        names.push(format!("({gaps} gaps)"));
    }
    Ok(names)
}

fn free_gb(path: &Path) -> Option<u64> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    let path = CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut stats: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: a valid C path and a zeroed statvfs to fill.
    if unsafe { libc::statvfs(path.as_ptr(), &mut stats) } != 0 {
        return None;
    }
    Some(stats.f_bavail as u64 * stats.f_frsize as u64 / 1_000_000_000)
}

fn json_file(path: &Path) -> Value {
    fs::read_to_string(path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or(Value::Null)
}

fn oracle_verdict(dir: &Path) -> String {
    let mut files: Vec<PathBuf> = fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name().is_some_and(|n| {
                n.to_string_lossy().starts_with("oracle-")
                    && n.to_string_lossy().ends_with("-stdout.txt")
            })
        })
        .collect();
    files.sort();
    files
        .iter()
        .filter_map(|f| fs::read_to_string(f).ok())
        .flat_map(|t| {
            t.lines()
                .filter(|l| l.starts_with("VERDICT"))
                .map(str::to_owned)
                .collect::<Vec<_>>()
        })
        .last()
        .unwrap_or_default()
}

/// Bundles exported per scenario.
static EXPORTED: Mutex<BTreeMap<String, usize>> = Mutex::new(BTreeMap::new());

fn execute(cli: &Cli, job: &Job, out: &Path, args: &Args) -> Value {
    let dir = out.join("runs").join(&job.label);
    let _ = fs::create_dir_all(&dir);
    let _ = fs::copy(&job.scenario, dir.join("scenario.yaml"));
    let scenario = job.scenario.to_string_lossy().to_string();
    let seed = job.seed.to_string();
    let started = Instant::now();
    let code = cli.run(
        &[
            "run",
            &scenario,
            "--seed",
            &seed,
            "--wall-time",
            &args.wall_time,
            "--json",
        ],
        &dir.join("run.json"),
        &dir.join("run.stderr"),
    );
    let run_wall = started.elapsed().as_secs_f64();
    let document = json_file(&dir.join("run.json"));
    let id = document["metadata"]["id"]
        .as_str()
        .unwrap_or("")
        .to_string();
    let mut row = json!({"label": job.label, "scenario": scenario, "seed": job.seed, "id": id, "exit": code,
                         "wall_s": (run_wall * 10.0).round() / 10.0,
                         "sim_s": document["status"]["simulatedTimeNs"].as_f64().map(|n| (n / 1e8).round() / 10.0),
                         "oracle_exit": document["status"]["oracleExitCode"], "status": document["status"]["verdict"]});
    if id.is_empty() {
        row["error"] = "no simulation id".into();
        return row;
    }
    // Collect promptly: logs share one bounded server budget.
    cli.run(
        &["logs", &id, "--json"],
        &dir.join("logs.jsonl"),
        &dir.join("logs.stderr"),
    );
    cli.run(
        &["events", &id],
        &dir.join("events.jsonl"),
        &dir.join("events.stderr"),
    );
    let files = split_logs(&dir).unwrap_or_default();
    row["log_files"] = json!(files.len());
    // The server evicts the oldest log history beyond its shared budget and says
    // so with gap records; such a run's history is incomplete for offline checks.
    row["log_gaps"] = json!(files.iter().any(|f| f.starts_with('(')));
    let verdict = oracle_verdict(&dir);
    let exited = document["status"]["termination"]["reason"] == "oracleExited";
    let hit = exited && document["status"]["oracleExitCode"] == 1;
    if !exited {
        row["error"] = document["status"]["termination"].clone();
        cli.run(
            &["diagnostics", &id],
            &dir.join("diagnostics.txt"),
            &dir.join("diagnostics.stderr"),
        );
    }
    row["hit"] = hit.into();
    row["verdict"] = verdict.chars().take(300).collect::<String>().into();
    if job.replay || (hit && args.replay_hits) {
        let started = Instant::now();
        let replay_dir = dir.join("replay");
        let _ = fs::create_dir_all(&replay_dir);
        cli.run(
            &["replay", &id, "--json", "--wall-time", &args.wall_time],
            &dir.join("replay.json"),
            &dir.join("replay.stderr"),
        );
        let replay = json_file(&dir.join("replay.json"));
        let replay_id = replay["simulation"].as_str().unwrap_or("").to_string();
        row["replay"] = json!({"id": replay_id, "status": replay["status"],
                               "wall_s": (started.elapsed().as_secs_f64() * 10.0).round() / 10.0});
        if !replay_id.is_empty() {
            cli.run(
                &["logs", &replay_id, "--json"],
                &replay_dir.join("logs.jsonl"),
                &replay_dir.join("logs.stderr"),
            );
            let _ = split_logs(&replay_dir);
            let (mut same, mut differ) = (0, Vec::new());
            for name in &files {
                if name.starts_with('(') {
                    continue;
                }
                let file = format!("{name}.txt");
                if fs::read(dir.join(&file)).ok() == fs::read(replay_dir.join(&file)).ok() {
                    same += 1;
                } else {
                    differ.push(name.clone());
                }
            }
            row["replay"]["logs_identical"] = same.into();
            row["replay"]["logs_differ"] = json!(differ);
            if !args.keep {
                cli.quiet(&["delete", "simulation", &replay_id]);
            }
        }
    }
    let export = hit && {
        let mut exported = EXPORTED.lock().unwrap();
        let count = exported.entry(scenario.clone()).or_insert(0);
        *count += 1;
        *count <= args.export_hits
    };
    if export {
        let bundles = out.join("bundles");
        let _ = fs::create_dir_all(&bundles);
        let path = bundles.join(format!("{}-{id}.seedbox.tar", job.label));
        let path_text = path.to_string_lossy().to_string();
        let ok = cli.quiet(&["export", &id, "-o", &path_text]);
        row["bundle"] = if ok { path_text.into() } else { Value::Null };
    }
    if !args.keep {
        cli.quiet(&["delete", "simulation", &id]);
        row["deleted"] = true.into();
    }
    let _ = fs::write(
        dir.join("row.json"),
        serde_json::to_string_pretty(&row).unwrap_or_default(),
    );
    row
}

fn ensure_server(cli: &Cli, args: &Args) -> Result<(), String> {
    if cli.quiet(&["get", "simulations"]) {
        return Ok(());
    }
    if !args.serve {
        return Err(format!(
            "no seedbox server answers for state {}; start `seedbox serve` or pass --serve",
            args.state.display()
        ));
    }
    let log = fs::File::create(args.out.join("serve.log")).map_err(|e| e.to_string())?;
    cli.command()
        .arg("serve")
        .stdin(Stdio::null())
        .stdout(log.try_clone().map_err(|e| e.to_string())?)
        .stderr(log)
        .spawn()
        .map_err(|e| format!("seedbox serve: {e}"))?;
    for _ in 0..120 {
        if cli.quiet(&["get", "simulations"]) {
            return Ok(());
        }
        kit::pause(0.5);
    }
    Err("seedbox serve did not start answering".into())
}

/// Per-scenario totals: runs, hits, replays and MATCHes.
pub fn summary(rows: &[Value]) -> String {
    let mut by: BTreeMap<String, [usize; 8]> = BTreeMap::new();
    for row in rows {
        let name = Path::new(row["scenario"].as_str().unwrap_or(""))
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default();
        let entry = by.entry(name).or_default();
        entry[0] += 1;
        entry[1] += usize::from(row["hit"] == true);
        entry[2] += usize::from(row["oracle_exit"] == 3);
        entry[6] += usize::from(!row["error"].is_null());
        entry[7] += usize::from(row["log_gaps"] == true);
        if !row["replay"].is_null() {
            entry[3] += 1;
            entry[4] += usize::from(row["replay"]["status"] == "MATCH");
            entry[5] += usize::from(
                row["replay"]["logs_differ"]
                    .as_array()
                    .is_some_and(|d| d.is_empty()),
            );
        }
    }
    let mut lines = vec![
        "| Scenario | Seeds | Hits | Inconclusive | Errors | Replays | MATCH | Logs identical | Log gaps |"
            .to_string(),
        "| --- | --- | --- | --- | --- | --- | --- | --- | --- |".to_string(),
    ];
    for (name, [runs, hits, inconclusive, replays, matched, identical, errors, gaps]) in by {
        lines.push(format!(
            "| {name} | {runs} | {hits} | {inconclusive} | {errors} | {replays} | {matched} | {identical} | {gaps} |"
        ));
    }
    lines.push(String::new());
    lines.push("| Run | ID | Oracle | Verdict | Sim s | Wall s | Replay |".to_string());
    lines.push("| --- | --- | --- | --- | --- | --- | --- |".to_string());
    for row in rows {
        let replay = if row["replay"].is_null() {
            String::new()
        } else {
            format!(
                "{} {}",
                row["replay"]["id"].as_str().unwrap_or(""),
                row["replay"]["status"].as_str().unwrap_or("?")
            )
        };
        let verdict: String = row["verdict"]
            .as_str()
            .unwrap_or("")
            .chars()
            .take(90)
            .collect();
        lines.push(format!(
            "| {} | {} | {} | {} | {} | {} | {replay} |",
            row["label"].as_str().unwrap_or(""),
            row["id"].as_str().unwrap_or(""),
            row["oracle_exit"],
            verdict.replace('|', "/"),
            row["sim_s"],
            row["wall_s"]
        ));
    }
    lines.join("\n")
}

pub fn run(args: Args) -> u8 {
    let jobs = match plan(&args) {
        Ok(jobs) => jobs,
        Err(error) => {
            eprintln!("error: {error}");
            return 2;
        }
    };
    if let Err(error) = fs::create_dir_all(args.out.join("runs")) {
        eprintln!("error: {}: {error}", args.out.display());
        return 2;
    }
    let cli = Cli { args: &args };
    if let Err(error) = ensure_server(&cli, &args) {
        eprintln!("error: {error}");
        return 2;
    }
    let queue = Arc::new(Mutex::new(jobs.into_iter().collect::<VecDeque<_>>()));
    let rows = Arc::new(Mutex::new(Vec::new()));
    let journal = Arc::new(Mutex::new(
        fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(args.out.join("campaign.jsonl"))
            .ok(),
    ));
    std::thread::scope(|scope| {
        for _ in 0..args.lanes.max(1) {
            let (queue, rows, journal, args, cli) =
                (queue.clone(), rows.clone(), journal.clone(), &args, &cli);
            scope.spawn(move || loop {
                let Some(job) = queue.lock().unwrap().pop_front() else {
                    return;
                };
                let free = free_gb(&args.state).or_else(|| free_gb(Path::new("/")));
                if free.is_some_and(|free| free < args.min_free_gb) {
                    eprintln!(
                        "campaign: {} GB free, below --min-free-gb {}; not launching {}",
                        free.unwrap_or(0),
                        args.min_free_gb,
                        job.label
                    );
                    rows.lock().unwrap().push(
                        json!({"label": job.label, "scenario": job.scenario, "seed": job.seed,
                                                       "error": "skipped: low disk"}),
                    );
                    continue;
                }
                eprintln!("campaign: start {}", job.label);
                let row = execute(cli, &job, &args.out, args);
                eprintln!(
                    "campaign: done {} id {} oracle {} replay {} {}",
                    job.label,
                    row["id"].as_str().unwrap_or("-"),
                    row["oracle_exit"],
                    row["replay"]["status"].as_str().unwrap_or("-"),
                    row["verdict"].as_str().unwrap_or("")
                );
                if let Some(file) = journal.lock().unwrap().as_mut() {
                    use std::io::Write;
                    let _ = writeln!(file, "{row}");
                }
                rows.lock().unwrap().push(row);
            });
        }
    });
    let mut rows = rows.lock().unwrap().clone();
    rows.sort_by(|a, b| a["label"].as_str().cmp(&b["label"].as_str()));
    let table = summary(&rows);
    let _ = fs::write(args.out.join("summary.md"), format!("{table}\n"));
    println!("{table}");
    let failed = rows.iter().any(|r| {
        !r["error"].is_null() || r["replay"]["status"].as_str().is_some_and(|s| s != "MATCH")
    });
    u8::from(failed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seed_lists_expand() {
        assert_eq!(seeds("5").unwrap(), [5]);
        assert_eq!(seeds("1-3,7").unwrap(), [1, 2, 3, 7]);
        assert!(seeds("x").is_err());
    }

    #[test]
    fn logs_split_by_machine_incarnation_and_stream() {
        let dir = std::env::temp_dir().join(format!("seedbox-jepsen-split-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("logs.jsonl"),
            "{\"type\":\"entry\",\"entry\":{\"type\":\"bytes\",\"machine\":\"n1\",\"incarnation\":0,\"stream\":\"stderr\",\"bytes\":[104,105]}}\n\
             {\"type\":\"entry\",\"entry\":{\"type\":\"bytes\",\"machine\":\"n1\",\"incarnation\":0,\"stream\":\"stderr\",\"bytes\":[10]}}\n\
             {\"type\":\"gap\"}\n",
        )
        .unwrap();
        assert_eq!(split_logs(&dir).unwrap(), ["n1-0-stderr", "(1 gaps)"]);
        assert_eq!(fs::read(dir.join("n1-0-stderr.txt")).unwrap(), b"hi\n");
        fs::remove_dir_all(dir).unwrap();
    }
}
