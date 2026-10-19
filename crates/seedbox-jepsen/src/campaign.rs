//! A host-side seed campaign: run scenarios over seeds, replay a sample and
//! compare every split log byte for byte, export bundles for hits, delete the
//! simulations (each is about 1 GB) and print a summary table.
//!
//! The server keeps application logs in one shared 64 MiB history budget and
//! evicts the oldest records beyond it, so a long, chatty run loses its start
//! before it completes. Each run and replay is therefore submitted detached,
//! and every machine's logs (`seedbox logs ID MACHINE --follow --json`) and the
//! events (`seedbox events ID --follow`) stream into the run directory from
//! creation until the simulation completes. A gap that still appears, for
//! example from a follower that started late or fell behind, is reported in
//! the row and the summary.
//!
//! This is research tooling, not an acceptance runner. It drives the `seedbox`
//! CLI given by `--seedbox` rather than linking the client library: the CLI
//! prepares projects and owns the server lifecycle, and the binary under test
//! may be a different build from this checkout.
use crate::kit;
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    fs,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

#[derive(clap::Args, Debug, Clone)]
pub struct Args {
    /// Jobs as SCENARIO.yaml[@PROFILE.yaml]:SEEDS, where SEEDS is `N`, `A-B`
    /// or `A,B,C` and PROFILE generates faults from each run's seed.
    pub jobs: Vec<String>,
    /// More jobs, one per line in the same form; `#` starts a comment, and
    /// relative paths resolve against the file's directory.
    #[arg(long)]
    pub jobs_file: Option<PathBuf>,
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
    profile: Option<PathBuf>,
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

/// Job specs from the command line, then the jobs file, with the file's
/// relative paths resolved against its directory.
fn specs(args: &Args) -> Result<Vec<(String, PathBuf)>, String> {
    let mut specs: Vec<(String, PathBuf)> = args
        .jobs
        .iter()
        .map(|spec| (spec.clone(), PathBuf::new()))
        .collect();
    if let Some(file) = &args.jobs_file {
        let text = fs::read_to_string(file).map_err(|e| format!("{}: {e}", file.display()))?;
        let base = file.parent().map(Path::to_path_buf).unwrap_or_default();
        for line in text.lines() {
            let line = line.split('#').next().unwrap_or("").trim();
            if !line.is_empty() {
                specs.push((line.to_string(), base.clone()));
            }
        }
    }
    if specs.is_empty() {
        return Err("no jobs: give SCENARIO.yaml:SEEDS arguments or --jobs-file".into());
    }
    Ok(specs)
}

fn stem(path: &Path) -> String {
    path.file_stem()
        .unwrap_or_default()
        .to_string_lossy()
        .to_string()
}

/// Every job of every spec, interleaved by seed index so each scenario runs
/// early instead of after all the seeds of the specs before it.
fn plan(args: &Args) -> Result<Vec<Job>, String> {
    let mut per_spec = Vec::new();
    for (spec, base) in specs(args)? {
        let (paths, list) = spec
            .rsplit_once(':')
            .ok_or_else(|| format!("{spec}: expected SCENARIO.yaml[@PROFILE.yaml]:SEEDS"))?;
        let (scenario, profile) = match paths.split_once('@') {
            Some((scenario, profile)) => (base.join(scenario), Some(base.join(profile))),
            None => (base.join(paths), None),
        };
        for path in std::iter::once(&scenario).chain(profile.as_ref()) {
            if !path.is_file() {
                return Err(format!("{}: not found", path.display()));
            }
        }
        let name = match &profile {
            Some(profile) => format!("{}@{}", stem(&scenario), stem(profile)),
            None => stem(&scenario),
        };
        let jobs: Vec<Job> = seeds(list)?
            .into_iter()
            .enumerate()
            .map(|(index, seed)| Job {
                label: format!("{name}-s{seed}"),
                scenario: scenario.clone(),
                profile: profile.clone(),
                seed,
                replay: index < args.replay_first,
            })
            .collect();
        per_spec.push(jobs.into_iter());
    }
    let mut jobs = Vec::new();
    loop {
        let before = jobs.len();
        jobs.extend(per_spec.iter_mut().filter_map(Iterator::next));
        if jobs.len() == before {
            return Ok(jobs);
        }
    }
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

    /// Start in the background with stdout to `stdout` and stderr to `stderr`.
    fn spawn(&self, args: &[&str], stdout: &Path, stderr: &Path) -> Result<Child, String> {
        let out = fs::File::create(stdout).map_err(|e| format!("{}: {e}", stdout.display()))?;
        let err = fs::File::create(stderr).map_err(|e| format!("{}: {e}", stderr.display()))?;
        self.command()
            .args(args)
            .stdin(Stdio::null())
            .stdout(out)
            .stderr(err)
            .spawn()
            .map_err(|e| format!("{}: {e}", self.args.seedbox.display()))
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

/// What one collected log or event file says about lost observations.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Losses {
    /// Sequence numbers this reader never received (`{"type":"gap"}`): history
    /// the server evicted under its shared budget before the reader got there,
    /// or records a follower missed by falling behind its live stream.
    pub missed: u64,
    /// Application output records the worker omitted before publication.
    pub omitted: u64,
    /// Worker observation ranges lost before the server, or output the server
    /// marked unavailable.
    pub source: u64,
    /// The file ends with a followed stream's `closed` record.
    pub closed: bool,
}

impl Losses {
    pub fn any(&self) -> bool {
        self.missed + self.omitted + self.source > 0
    }

    fn describe(&self) -> String {
        let mut parts = Vec::new();
        if self.missed > 0 {
            parts.push(format!(
                "{} records missed (evicted or lagged)",
                self.missed
            ));
        }
        if self.omitted > 0 {
            parts.push(format!("{} records omitted by the worker", self.omitted));
        }
        if self.source > 0 {
            parts.push(format!("{} source gaps", self.source));
        }
        parts.join(", ")
    }

    fn gap(&mut self, record: &Value) {
        let (first, last) = (record["first"].as_u64(), record["last"].as_u64());
        self.missed += match (first, last) {
            (Some(first), Some(last)) if last >= first => last - first + 1,
            _ => 1,
        };
    }
}

/// Append the bytes of one `seedbox logs ... --json` file to `streams`, keyed
/// `MACHINE-INCARNATION-STREAM`, in delivery order; return what it lost.
pub fn assemble(text: &str, streams: &mut BTreeMap<String, Vec<u8>>) -> Losses {
    let mut losses = Losses::default();
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
            Some("gap") => losses.gap(entry),
            Some("machineGap") => losses.omitted += entry["records"].as_u64().unwrap_or(1),
            Some("sourceGap") | Some("sourceUnavailable") => losses.source += 1,
            Some("closed") => losses.closed = true,
            _ => {}
        }
    }
    losses
}

/// What a `seedbox events` file lost.
pub fn event_losses(text: &str) -> Losses {
    let mut losses = Losses::default();
    for line in text.lines() {
        let Ok(item) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        match item["type"].as_str() {
            Some("gap") => losses.gap(&item),
            Some("closed") => losses.closed = true,
            _ => {}
        }
        if matches!(
            item["observation"]["event"]["type"].as_str(),
            Some("sourceGap" | "sourceUnavailable")
        ) {
            losses.source += 1;
        }
    }
    losses
}

/// A run directory's collected logs, split into `MACHINE-INCARNATION-STREAM.txt`.
#[derive(Debug, Default)]
pub struct Split {
    /// The stream names written, sorted.
    pub names: Vec<String>,
    /// Losses by collected file name.
    pub losses: BTreeMap<String, Losses>,
}

/// The collected log files in `dir`: one followed `logs-MACHINE.jsonl` per
/// machine, or else one all-machine `logs.jsonl`.
fn log_files(dir: &Path) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name().is_some_and(|n| {
                let n = n.to_string_lossy();
                n.starts_with("logs-") && n.ends_with(".jsonl")
            })
        })
        .collect();
    files.sort();
    if files.is_empty() && dir.join("logs.jsonl").is_file() {
        files.push(dir.join("logs.jsonl"));
    }
    files
}

/// Split the collected logs in `dir`. Each machine's records are in delivery
/// order within its own file, so the split is the same whether they came from
/// per-machine streams or the all-machine stream.
pub fn split_logs(dir: &Path) -> Result<Split, String> {
    let files = log_files(dir);
    if files.is_empty() {
        return Err(format!("{}: no collected logs", dir.display()));
    }
    let mut streams: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    let mut split = Split::default();
    for file in files {
        let text = fs::read_to_string(&file).map_err(|e| format!("{}: {e}", file.display()))?;
        let name = file.file_name().unwrap_or_default().to_string_lossy();
        split
            .losses
            .insert(name.to_string(), assemble(&text, &mut streams));
    }
    for (name, data) in &streams {
        fs::write(dir.join(format!("{name}.txt")), data).map_err(|e| e.to_string())?;
    }
    split.names = streams.into_keys().collect();
    Ok(split)
}

/// Every way a directory's collection is incomplete: losses in each file, and
/// for followed streams, a file that never saw its stream close.
fn incomplete(losses: &BTreeMap<String, Losses>, followed: bool) -> Vec<String> {
    let mut problems = Vec::new();
    for (file, losses) in losses {
        if losses.any() {
            problems.push(format!("{file}: {}", losses.describe()));
        }
        if followed && !losses.closed {
            problems.push(format!("{file}: stream did not close"));
        }
    }
    problems
}

/// Machines whose logs a simulation resource carries: its machines and its
/// oracle. A submitted project leaves the oracle unnamed until the server
/// resolves it as `oracle`.
fn machines(resource: &Value) -> Vec<String> {
    let mut names: BTreeSet<String> = resource["spec"]["machines"]
        .as_object()
        .map(|m| m.keys().cloned().collect())
        .unwrap_or_default();
    let oracle = &resource["spec"]["oracle"];
    if oracle.is_object() {
        names.insert(oracle["name"].as_str().unwrap_or("oracle").to_string());
    }
    names.into_iter().collect()
}

/// Log and event streams following one simulation into its run directory.
#[derive(Default)]
struct Followers {
    children: Vec<(String, Child)>,
    problems: Vec<String>,
}

impl Followers {
    /// Follow every machine's logs and the events of simulation `id` into `dir`.
    fn start(cli: &Cli, id: &str, machines: &[String], dir: &Path) -> Self {
        let mut followers = Self::default();
        let mut start = |name: String, args: Vec<&str>| {
            let stdout = dir.join(format!("{name}.jsonl"));
            let stderr = dir.join(format!("{name}.stderr"));
            match cli.spawn(&args, &stdout, &stderr) {
                Ok(child) => followers.children.push((name, child)),
                Err(error) => followers.problems.push(format!("{name}: {error}")),
            }
        };
        if machines.is_empty() {
            start("logs".into(), vec!["logs", id, "--follow", "--json"]);
        }
        for machine in machines {
            start(
                format!("logs-{machine}"),
                vec!["logs", id, machine, "--follow", "--json"],
            );
        }
        start("events".into(), vec!["events", id, "--follow"]);
        followers
    }

    /// After the simulation completed, wait for each stream to close; stop one
    /// still open after `grace`. Returns what went wrong.
    fn finish(mut self, grace: Duration) -> Vec<String> {
        let deadline = Instant::now() + grace;
        let mut problems = std::mem::take(&mut self.problems);
        for (name, mut child) in std::mem::take(&mut self.children) {
            loop {
                match child.try_wait() {
                    Ok(Some(status)) => {
                        if !status.success() {
                            problems.push(format!("{name}: follower exited with {status}"));
                        }
                        break;
                    }
                    Ok(None) if Instant::now() < deadline => kit::pause(0.1),
                    Ok(None) => {
                        let _ = child.kill();
                        let _ = child.wait();
                        problems.push(format!(
                            "{name}: still open {}s after completion; stopped",
                            grace.as_secs()
                        ));
                        break;
                    }
                    Err(error) => {
                        problems.push(format!("{name}: {error}"));
                        break;
                    }
                }
            }
        }
        problems
    }
}

impl Drop for Followers {
    fn drop(&mut self) {
        for (_, child) in &mut self.children {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// The exit code `seedbox run` reports for a completed simulation resource.
fn outcome_code(resource: &Value) -> i32 {
    let status = &resource["status"];
    match status["oracleExitCode"].as_u64() {
        Some(0) => 0,
        Some(_) => 1,
        None => match status["termination"]["reason"].as_str() {
            Some("error") => 2,
            Some("simulatedTimeLimit" | "wallTimeLimit" | "preparationTimeLimit") => 124,
            _ => 3,
        },
    }
}

/// A detached simulation observed from creation to completion.
struct Observed {
    id: String,
    /// The final resource, as `seedbox run --json` prints it.
    resource: Value,
    /// Follower failures, plus logs and events the collection lost.
    log_problems: Vec<String>,
    event_problems: Vec<String>,
    split: Split,
}

/// Submit with `submit` (a `--detach --json` command whose stdout is the new
/// resource), follow its logs and events into `dir` from creation, wait for it
/// to complete and write its final resource to `dir/<name>.json`.
fn observe(cli: &Cli, submit: &[&str], dir: &Path, name: &str) -> Result<Observed, String> {
    let submitted = dir.join(format!("{name}-submit.json"));
    let code = cli.run(submit, &submitted, &dir.join(format!("{name}.stderr")));
    let created = json_file(&submitted);
    let id = created["metadata"]["id"].as_str().unwrap_or("").to_string();
    if id.is_empty() {
        return Err(format!("no simulation id (submission exit {code})"));
    }
    let followed = machines(&created);
    let followers = Followers::start(cli, &id, &followed, dir);
    let final_path = dir.join(format!("{name}.json"));
    let complete = |timeout: Option<&str>| {
        let mut args = vec!["wait", &id, "--for", "completed"];
        if let Some(timeout) = timeout {
            args.extend(["--timeout", timeout]);
        }
        cli.run(&args, &dir.join("wait.txt"), &dir.join("wait.stderr"));
        cli.run(
            &["describe", "simulation", &id, "--json"],
            &final_path,
            &dir.join("describe.stderr"),
        );
        json_file(&final_path)
    };
    let mut resource = complete(None);
    if resource["status"]["phase"] != "completed" {
        // Interrupted while waiting: stop it, as a foreground `seedbox run` would.
        cli.quiet(&["stop", &id]);
        resource = complete(Some("120s"));
    }
    let (mut event_problems, mut log_problems): (Vec<String>, Vec<String>) = followers
        .finish(Duration::from_secs(60))
        .into_iter()
        .partition(|p| p.starts_with("events"));
    let split = split_logs(dir).unwrap_or_else(|error| {
        log_problems.push(error);
        Split::default()
    });
    log_problems.extend(incomplete(&split.losses, true));
    if !followed.is_empty() {
        // The resolved resource names every machine; one not followed lost its logs.
        for machine in machines(&resource) {
            if !followed.contains(&machine) {
                log_problems.push(format!("{machine}: logs not followed"));
            }
        }
    }
    let events = fs::read_to_string(dir.join("events.jsonl")).unwrap_or_default();
    event_problems.extend(incomplete(
        &BTreeMap::from([("events.jsonl".to_string(), event_losses(&events))]),
        true,
    ));
    Ok(Observed {
        id,
        resource,
        log_problems,
        event_problems,
        split,
    })
}

/// Compare split logs by name: (identical, differing or present on one side only).
fn compare_logs(
    dir: &Path,
    replay_dir: &Path,
    original: &[String],
    replay: &[String],
) -> (usize, Vec<String>) {
    let names: BTreeSet<&String> = original.iter().chain(replay).collect();
    let (mut same, mut differ) = (0, Vec::new());
    for name in names {
        let file = format!("{name}.txt");
        match (fs::read(dir.join(&file)), fs::read(replay_dir.join(&file))) {
            (Ok(a), Ok(b)) if a == b => same += 1,
            _ => differ.push(name.clone()),
        }
    }
    (same, differ)
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
    let profile = job
        .profile
        .as_ref()
        .map(|p| p.to_string_lossy().to_string());
    let mut submit = vec![
        "run",
        &scenario,
        "--seed",
        &seed,
        "--wall-time",
        &args.wall_time,
        "--detach",
        "--json",
    ];
    if let Some(profile) = &profile {
        submit.extend(["--fault-profile", profile.as_str()]);
    }
    let observed = observe(cli, &submit, &dir, "run");
    let run_wall = started.elapsed().as_secs_f64();
    let mut row = json!({"label": job.label, "scenario": scenario, "profile": profile, "seed": job.seed,
                         "wall_s": (run_wall * 10.0).round() / 10.0});
    let observed = match observed {
        Ok(observed) => observed,
        Err(error) => {
            row["id"] = "".into();
            row["error"] = error.into();
            return row;
        }
    };
    let (id, document) = (observed.id.clone(), &observed.resource);
    row["id"] = id.clone().into();
    row["exit"] = outcome_code(document).into();
    row["sim_s"] = json!(document["status"]["simulatedTimeNs"]
        .as_f64()
        .map(|n| (n / 1e8).round() / 10.0));
    row["oracle_exit"] = document["status"]["oracleExitCode"].clone();
    row["status"] = document["status"]["verdict"].clone();
    let files = &observed.split.names;
    row["log_files"] = json!(files.len());
    // A run whose logs lost records (evicted before the follower read them, or
    // missed by a lagging follower) has an incomplete history for offline checks.
    row["log_gaps"] = json!(!observed.log_problems.is_empty());
    row["log_losses"] = json!(observed.log_problems);
    row["event_gaps"] = json!(!observed.event_problems.is_empty());
    row["event_losses"] = json!(observed.event_problems);
    let verdict = oracle_verdict(&dir);
    let exited = document["status"]["termination"]["reason"] == "oracleExited";
    let hit = exited && document["status"]["oracleExitCode"] == 1;
    if !exited {
        row["error"] = document["status"]["termination"].clone();
        if row["error"].is_null() {
            row["error"] = "no final status".into();
        }
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
        let submit = [
            "rerun",
            &id,
            "--wall-time",
            &args.wall_time,
            "--detach",
            "--json",
        ];
        match observe(cli, &submit, &replay_dir, "run") {
            Err(error) => row["replay"] = json!({"id": "", "status": Value::Null, "error": error}),
            Ok(replay) => {
                // `seedbox replay ID` is this rerun followed by this comparison.
                cli.run(
                    &["compare", &id, &replay.id, "--json"],
                    &dir.join("replay.json"),
                    &dir.join("replay.stderr"),
                );
                let comparison = json_file(&dir.join("replay.json"));
                let (same, differ) = compare_logs(&dir, &replay_dir, files, &replay.split.names);
                row["replay"] = json!({"id": replay.id, "status": comparison["status"],
                                       "wall_s": (started.elapsed().as_secs_f64() * 10.0).round() / 10.0,
                                       "logs_identical": same, "logs_differ": differ,
                                       "log_gaps": !replay.log_problems.is_empty(),
                                       "log_losses": replay.log_problems,
                                       "event_losses": replay.event_problems});
                if !args.keep {
                    cli.quiet(&["delete", "simulation", &replay.id]);
                }
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
        .args(["serve", "--capacity", &args.lanes.max(1).to_string()])
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
        let mut name = stem(Path::new(row["scenario"].as_str().unwrap_or("")));
        if let Some(profile) = row["profile"].as_str() {
            name = format!("{name}@{}", stem(Path::new(profile)));
        }
        let entry = by.entry(name).or_default();
        entry[0] += 1;
        entry[1] += usize::from(row["hit"] == true);
        entry[2] += usize::from(row["oracle_exit"] == 3);
        entry[6] += usize::from(!row["error"].is_null());
        entry[7] += usize::from(row["log_gaps"] == true || row["replay"]["log_gaps"] == true);
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
    let incomplete: Vec<String> = rows
        .iter()
        .flat_map(|row| {
            let label = row["label"].as_str().unwrap_or("").to_string();
            let losses = |value: &Value| -> Vec<String> {
                value
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|p| p.as_str().map(str::to_owned))
                    .collect()
            };
            let run = losses(&row["log_losses"])
                .into_iter()
                .chain(losses(&row["event_losses"]));
            let replay = losses(&row["replay"]["log_losses"])
                .into_iter()
                .chain(losses(&row["replay"]["event_losses"]))
                .map(|p| format!("replay {p}"));
            run.chain(replay)
                .map(move |problem| format!("- {label}: {problem}"))
                .collect::<Vec<_>>()
        })
        .collect();
    if !incomplete.is_empty() {
        lines.push(String::new());
        lines.push("Incomplete logs or events:".to_string());
        lines.push(String::new());
        lines.extend(incomplete);
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

    fn parse(argv: &[&str]) -> Args {
        #[derive(clap::Parser)]
        struct Cli {
            #[command(flatten)]
            args: Args,
        }
        <Cli as clap::Parser>::parse_from(std::iter::once("campaign").chain(argv.iter().copied()))
            .args
    }

    #[test]
    fn jobs_interleave_and_resolve_profiles_against_the_jobs_file() {
        let dir = scratch("plan");
        for name in ["a.yaml", "b.yaml", "p.yaml"] {
            fs::write(dir.join(name), "").unwrap();
        }
        fs::write(
            dir.join("jobs"),
            "# comment\na.yaml@p.yaml:1-3\n\nb.yaml:7 # trailing\n",
        )
        .unwrap();
        let jobs_file = dir.join("jobs");
        let args = parse(&[
            "--jobs-file",
            jobs_file.to_str().unwrap(),
            "--state",
            "s",
            "--out",
            "o",
            "--replay-first",
            "1",
        ]);
        let jobs = plan(&args).unwrap();
        let labels: Vec<&str> = jobs.iter().map(|j| j.label.as_str()).collect();
        assert_eq!(labels, ["a@p-s1", "b-s7", "a@p-s2", "a@p-s3"]);
        assert_eq!(
            jobs[0].profile.as_deref(),
            Some(dir.join("p.yaml").as_path())
        );
        assert_eq!(jobs[1].profile, None);
        assert!(jobs[0].replay && jobs[1].replay && !jobs[2].replay);
        let missing = parse(&["x.yaml@y.yaml:1", "--state", "s", "--out", "o"]);
        assert!(plan(&missing).unwrap_err().contains("not found"));
        let none = parse(&["--state", "s", "--out", "o"]);
        assert!(plan(&none).unwrap_err().contains("no jobs"));
    }

    #[test]
    fn seed_lists_expand() {
        assert_eq!(seeds("5").unwrap(), [5]);
        assert_eq!(seeds("1-3,7").unwrap(), [1, 2, 3, 7]);
        assert!(seeds("x").is_err());
    }

    fn bytes(sequence: u64, machine: &str, stream: &str, bytes: &[u8]) -> String {
        json!({"type": "entry", "sequence": sequence, "entry": {"type": "bytes", "machine": machine,
               "incarnation": 0, "simulatedTimeNs": sequence, "stream": stream, "bytes": bytes}})
        .to_string()
    }

    fn scratch(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("seedbox-jepsen-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn assembly_appends_in_order_and_counts_every_loss() {
        let mut streams = BTreeMap::new();
        let text = [
            json!({"type": "gap", "first": 1, "last": 41610}).to_string(),
            bytes(41611, "oracle", "stdout", b"he"),
            "not json".to_string(),
            json!({"type": "entry", "sequence": 41612, "entry": {"type": "machineGap", "machine": "oracle",
                   "incarnation": 0, "simulatedTimeNs": 5, "records": 3}})
            .to_string(),
            bytes(41613, "oracle", "stdout", b"llo\n"),
            json!({"type": "entry", "sequence": 41614, "entry": {"type": "sourceGap", "first": 2, "last": 4}})
                .to_string(),
            json!({"type": "entry", "sequence": 41615, "entry": {"type": "sourceUnavailable"}}).to_string(),
            json!({"type": "closed"}).to_string(),
        ]
        .join("\n");
        let losses = assemble(&text, &mut streams);
        assert_eq!(
            losses,
            Losses {
                missed: 41610,
                omitted: 3,
                source: 2,
                closed: true
            }
        );
        assert_eq!(streams["oracle-0-stdout"], b"hello\n");
        assert_eq!(
            losses.describe(),
            "41610 records missed (evicted or lagged), 3 records omitted by the worker, 2 source gaps"
        );
        assert!(!Losses {
            closed: true,
            ..Losses::default()
        }
        .any());
    }

    #[test]
    fn event_files_report_gaps_and_closure() {
        let text = [
            json!({"type": "event", "sequence": 1, "observation": {"simulation": "x", "revision": 1,
                   "event": {"type": "stopRequested"}}})
            .to_string(),
            json!({"type": "gap", "first": 2, "last": 3}).to_string(),
            json!({"type": "event", "sequence": 4, "observation": {"simulation": "x", "revision": 1,
                   "event": {"type": "sourceGap", "first": 9, "last": 9}}})
            .to_string(),
        ]
        .join("\n");
        let losses = event_losses(&text);
        assert_eq!((losses.missed, losses.source, losses.closed), (2, 1, false));
        let closed = event_losses(&json!({"type": "closed"}).to_string());
        assert!(closed.closed && !closed.any());
    }

    /// Per-machine followed files split to exactly the bytes the post-hoc
    /// all-machine collection produced from the same records.
    #[test]
    fn per_machine_files_split_like_the_all_machine_stream() {
        let records = [
            (1, "n1", "stderr", &b"boot\n"[..]),
            (2, "oracle", "stdout", b"op 1\n"),
            (3, "n1", "stderr", b"ready"),
            (4, "oracle", "stdout", b"op 2\n"),
            (5, "oracle", "stderr", b"warn\n"),
            (6, "n1", "stderr", b"\n"),
        ];
        let all = scratch("split-all");
        let lines: Vec<String> = records
            .iter()
            .map(|(s, m, st, b)| bytes(*s, m, st, b))
            .collect();
        fs::write(all.join("logs.jsonl"), lines.join("\n")).unwrap();
        let followed = scratch("split-followed");
        for machine in ["n1", "oracle"] {
            let mut lines: Vec<String> = records
                .iter()
                .filter(|(_, m, _, _)| *m == machine)
                .enumerate()
                .map(|(i, (_, m, st, b))| bytes(i as u64 + 1, m, st, b))
                .collect();
            lines.push(json!({"type": "closed"}).to_string());
            fs::write(
                followed.join(format!("logs-{machine}.jsonl")),
                lines.join("\n"),
            )
            .unwrap();
        }
        let a = split_logs(&all).unwrap();
        let b = split_logs(&followed).unwrap();
        assert_eq!(
            a.names,
            ["n1-0-stderr", "oracle-0-stderr", "oracle-0-stdout"]
        );
        assert_eq!(a.names, b.names);
        assert_eq!(
            compare_logs(&all, &followed, &a.names, &b.names),
            (3, vec![])
        );
        assert_eq!(
            fs::read(followed.join("n1-0-stderr.txt")).unwrap(),
            b"boot\nready\n"
        );
        assert!(incomplete(&b.losses, true).is_empty());
        // The post-hoc stream never closes; that is only a problem when followed.
        assert!(incomplete(&a.losses, false).is_empty());
        assert_eq!(
            incomplete(&a.losses, true),
            ["logs.jsonl: stream did not close"]
        );
        fs::remove_dir_all(all).unwrap();
        fs::remove_dir_all(followed).unwrap();
    }

    #[test]
    fn replay_comparison_names_streams_on_either_side() {
        let (a, b) = (scratch("cmp-a"), scratch("cmp-b"));
        fs::write(a.join("n1-0-stderr.txt"), b"x").unwrap();
        fs::write(b.join("n1-0-stderr.txt"), b"x").unwrap();
        fs::write(a.join("oracle-0-stdout.txt"), b"1").unwrap();
        fs::write(b.join("oracle-0-stdout.txt"), b"2").unwrap();
        fs::write(b.join("n2-0-stderr.txt"), b"extra").unwrap();
        let names_a = ["n1-0-stderr".to_string(), "oracle-0-stdout".to_string()];
        let names_b = [
            "n1-0-stderr".to_string(),
            "n2-0-stderr".to_string(),
            "oracle-0-stdout".to_string(),
        ];
        assert_eq!(
            compare_logs(&a, &b, &names_a, &names_b),
            (
                1,
                vec!["n2-0-stderr".to_string(), "oracle-0-stdout".to_string()]
            )
        );
        fs::remove_dir_all(a).unwrap();
        fs::remove_dir_all(b).unwrap();
    }

    #[test]
    fn machines_and_outcomes_come_from_the_resource() {
        let resource =
            json!({"spec": {"machines": {"n2": {}, "n1": {}}, "oracle": {"name": "oracle"}}});
        assert_eq!(machines(&resource), ["n1", "n2", "oracle"]);
        assert!(machines(&Value::Null).is_empty());
        let submitted = json!({"spec": {"machines": {"n1": {}}, "oracle": {"memoryMiB": 2048}}});
        assert_eq!(machines(&submitted), ["n1", "oracle"]);
        let status = |status: Value| json!({ "status": status });
        assert_eq!(outcome_code(&status(json!({"oracleExitCode": 0}))), 0);
        assert_eq!(outcome_code(&status(json!({"oracleExitCode": 3}))), 1);
        assert_eq!(
            outcome_code(&status(
                json!({"termination": {"reason": "error", "message": "x"}})
            )),
            2
        );
        assert_eq!(
            outcome_code(&status(json!({"termination": {"reason": "wallTimeLimit"}}))),
            124
        );
        assert_eq!(
            outcome_code(&status(json!({"termination": {"reason": "stopped"}}))),
            3
        );
    }

    #[test]
    fn summary_lists_incomplete_collections() {
        let rows = [
            json!({"label": "a-s1", "scenario": "x/a.yaml", "log_gaps": true,
                   "log_losses": ["logs-oracle.jsonl: 5 records missed (evicted or lagged)"],
                   "replay": {"status": "MATCH", "logs_differ": [], "log_gaps": false, "log_losses": [],
                              "event_losses": ["events.jsonl: stream did not close"]}}),
            json!({"label": "a-s2", "scenario": "x/a.yaml", "log_gaps": false, "log_losses": []}),
        ];
        let table = summary(&rows);
        assert!(
            table.contains("| a | 2 | 0 | 0 | 0 | 1 | 1 | 1 | 1 |"),
            "{table}"
        );
        assert!(table.ends_with(
            "Incomplete logs or events:\n\n\
             - a-s1: logs-oracle.jsonl: 5 records missed (evicted or lagged)\n\
             - a-s1: replay events.jsonl: stream did not close"
        ));
        assert!(!summary(&rows[1..]).contains("Incomplete"));
    }
}
