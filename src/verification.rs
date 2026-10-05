//! Acceptance checks: fresh executions, host load, race oracle, and atomic fix.
use crate::{
    config::load_manifest,
    error::{Error, Result},
    runtime::{Simulation, digest},
};
use serde_json::{Value, json};
use std::{
    fs,
    path::Path,
    process::{Child, Command, Stdio},
    time::Instant,
};

struct Workers(Vec<Child>);
impl Drop for Workers {
    fn drop(&mut self) {
        for worker in &mut self.0 {
            let _ = worker.kill();
            let _ = worker.wait();
        }
    }
}
fn fresh(runner: &Path, root: &Path, output: &Path) -> Result<Vec<u8>> {
    let result = Command::new(runner)
        .arg("run")
        .arg(root.join("examples/cluster.json"))
        .args(["--seed", "42", "--output"])
        .arg(output)
        .output()?;
    if !result.status.success() || !result.stderr.is_empty() {
        return Err(Error::Invariant(format!(
            "fresh execution failed: {}",
            String::from_utf8_lossy(&result.stderr)
        )));
    }
    Ok(fs::read(output)?)
}
fn counter(report: &Value) -> Result<u32> {
    if report["status"] != "completed" {
        return Err(Error::Invariant(format!(
            "race guest failed: {}",
            report["error"]
        )));
    }
    let bytes = hex::decode(
        report["nodes"]["race"]["stdout_hex"]
            .as_str()
            .ok_or_else(|| Error::Invariant("missing counter".into()))?,
    )
    .map_err(|e| Error::Invariant(e.to_string()))?;
    Ok(u32::from_le_bytes(bytes.try_into().map_err(|_| {
        Error::Invariant("counter must be four bytes".into())
    })?))
}
pub fn verify(root: &Path, runner: &Path, seeds: u64, output: &Path) -> Result<Value> {
    if !(1..=10000).contains(&seeds) {
        return Err(Error::InvalidInput("seeds must be in 1..10000".into()));
    }
    let directory = output
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    fs::create_dir_all(directory)?;
    let baseline = fresh(runner, root, &directory.join("baseline.json"))?;
    if baseline != fresh(runner, root, &directory.join("fresh.json"))? {
        return Err(Error::Invariant("fresh executions diverged".into()));
    }
    let mut workers = Workers(vec![]);
    for _ in 0..2 {
        workers.0.push(
            Command::new(runner)
                .arg("load-worker")
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()?,
        );
    }
    let loaded = fresh(runner, root, &directory.join("under-load.json"))?;
    drop(workers);
    if baseline != loaded {
        return Err(Error::Invariant("host CPU load changed execution".into()));
    }
    let (race_config, race_nodes) = load_manifest(&root.join("examples/race.json"))?;
    let (fixed_config, fixed_nodes) = load_manifest(&root.join("examples/atomic.json"))?;
    let mut failures = Vec::new();
    let mut counters = Vec::new();
    let start = Instant::now();
    for seed in 0..seeds {
        let broken =
            Simulation::new(race_nodes.clone(), seed, race_config.clone(), false)?.run()?;
        let fixed =
            Simulation::new(fixed_nodes.clone(), seed, fixed_config.clone(), false)?.run()?;
        let value = counter(&broken)?;
        counters.push(value);
        if value != 64 {
            failures.push(seed);
        }
        if counter(&fixed)? != 64 {
            return Err(Error::Invariant(format!(
                "atomic control failed at seed {seed}"
            )));
        }
    }
    let elapsed = start.elapsed().as_secs_f64();
    if failures.is_empty() || counters.iter().all(|v| *v == counters[0]) {
        return Err(Error::Invariant(
            "seed sweep failed to expose and vary the intentional race".into(),
        ));
    }
    let (mut config, specs) = load_manifest(&root.join("examples/cluster.json"))?;
    config.network.loss_ppm = 1_000_000;
    let faulted = Simulation::new(specs, 42, config, false)?.run()?;
    if faulted["status"] != "deadlock" {
        return Err(Error::Invariant(
            "total packet loss did not expose blocked protocol".into(),
        ));
    }
    let original: Value = serde_json::from_slice(&baseline)?;
    let summary = json!({
        "fresh_process_reports_identical": true, "reports_identical_under_two_host_cpu_workers": true,
        "report_sha256": digest(&baseline), "instruction_sha256": original["instruction_sha256"], "trace_sha256": original["trace_sha256"],
        "race_seed_count": seeds, "race_failure_count": failures.len(), "race_counter_range": [counters.iter().min(), counters.iter().max()],
        "first_failing_seeds": failures.iter().take(10).collect::<Vec<_>>(), "atomic_fix_pass_count": seeds, "packet_loss_oracle": faulted["status"],
        // Wall-clock timing is confined to this separate acceptance artifact.
        "seed_sweep_wall_seconds": elapsed, "seed_sweep_executions_per_second": 2.0 * seeds as f64 / elapsed,
        "cluster_virtual_time_ns": original["virtual_time_ns"]
    });
    fs::write(output, serde_json::to_string_pretty(&summary)? + "\n")?;
    Ok(summary)
}

fn redis_oracle(report: &Value) -> Result<String> {
    let output = hex::decode(
        report["nodes"]["client"]["stdout_hex"]
            .as_str()
            .ok_or_else(|| Error::Invariant("missing Redis client output".into()))?,
    )
    .map_err(|e| Error::Invariant(e.to_string()))?;
    let output = String::from_utf8(output).map_err(|e| Error::Invariant(e.to_string()))?;
    if report["status"] != "completed"
        || report["nodes"]["client"]["stderr_hex"] != ""
        || [
            "PASS lazy expiration",
            "PASS absolute expiration",
            "PASS persisted key",
            "PASS active expiration",
            "PASS blocking command timeout",
            "PASS Redis TTL suite",
        ]
        .iter()
        .any(|marker| !output.contains(marker))
    {
        return Err(Error::Invariant(format!(
            "Redis TTL oracle failed: {}; {}",
            report["error"], output
        )));
    }
    if report["nodes"]["redis"]["threads"]
        .as_object()
        .map(|t| t.len())
        .unwrap_or(0)
        < 4
    {
        return Err(Error::Invariant(
            "Redis background threads did not execute".into(),
        ));
    }
    Ok(output)
}
fn fresh_redis(runner: &Path, root: &Path, output: &Path, timezone: &str) -> Result<Vec<u8>> {
    let result = Command::new(runner)
        .arg("run")
        .arg(root.join("examples/redis.json"))
        .args(["--seed", "42", "--output"])
        .arg(output)
        .env("TZ", timezone)
        .output()?;
    if !result.status.success() || !result.stderr.is_empty() {
        return Err(Error::Invariant(format!(
            "fresh Redis execution failed: {} {}",
            String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr)
        )));
    }
    let bytes = fs::read(output)?;
    redis_oracle(&serde_json::from_slice(&bytes)?)?;
    Ok(bytes)
}
/// Run real Redis/hiredis, independently regenerate its report, and challenge
/// the application oracle with an intentionally incorrect expiration deadline.
pub fn verify_redis(root: &Path, runner: &Path, seeds: u64, output: &Path) -> Result<Value> {
    if !(1..=64).contains(&seeds) {
        return Err(Error::InvalidInput("Redis seeds must be in 1..64".into()));
    }
    let directory = output
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    fs::create_dir_all(directory)?;
    let start = Instant::now();
    let baseline = fresh_redis(runner, root, &directory.join("redis-baseline.json"), "UTC")?;
    if baseline
        != fresh_redis(
            runner,
            root,
            &directory.join("redis-fresh.json"),
            "Pacific/Honolulu",
        )?
    {
        return Err(Error::Invariant(
            "fresh Redis reports or host timezones diverged".into(),
        ));
    }
    let mut workers = Workers(vec![]);
    for _ in 0..2 {
        workers.0.push(
            Command::new(runner)
                .arg("load-worker")
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()?,
        );
    }
    let loaded = fresh_redis(
        runner,
        root,
        &directory.join("redis-under-load.json"),
        "Asia/Tokyo",
    )?;
    drop(workers);
    if loaded != baseline {
        return Err(Error::Invariant(
            "host CPU load changed Redis execution".into(),
        ));
    }
    let (config, specs) = load_manifest(&root.join("examples/redis.json"))?;
    let mut traces = std::collections::BTreeSet::new();
    let mut runs = Vec::new();
    for seed in 0..seeds {
        let report = Simulation::new(specs.clone(), seed, config.clone(), false)?.run()?;
        let transcript = redis_oracle(&report)?;
        traces.insert(report["instruction_sha256"].as_str().unwrap().to_string());
        runs.push(json!({"seed":seed,"instructions":report["instructions"],"virtual_time_ns":report["virtual_time_ns"],"instruction_sha256":report["instruction_sha256"],"trace_sha256":report["trace_sha256"],"client_output":transcript}));
    }
    if seeds > 1 && traces.len() < 2 {
        return Err(Error::Invariant(
            "Redis seeds did not change execution".into(),
        ));
    }
    let mut broken = specs;
    let client = broken
        .iter_mut()
        .find(|s| s.name == "client")
        .ok_or_else(|| Error::Invariant("missing TTL client".into()))?;
    client.argv.push("--negative-control".into());
    let negative = Simulation::new(broken, 42, config, false)?.run()?;
    let negative_stderr = hex::decode(negative["nodes"]["client"]["stderr_hex"].as_str().unwrap())
        .map_err(|e| Error::Invariant(e.to_string()))?;
    if negative["status"] != "guest_failure"
        || !String::from_utf8_lossy(&negative_stderr).contains("FAIL: GET lazy")
    {
        return Err(Error::Invariant(format!(
            "incorrect TTL escaped application oracle: {} {}",
            negative["status"],
            String::from_utf8_lossy(&negative_stderr)
        )));
    }
    let mut bytes = crate::runtime::canonical(&negative);
    bytes.push(b'\n');
    fs::write(directory.join("redis-negative-control.json"), bytes)?;
    let original: Value = serde_json::from_slice(&baseline)?;
    let summary = json!({"redis_version":"7.2.7","client_library":"Redis bundled hiredis","ttl_checks":["PTTL before expiry","lazy expiration","PXAT absolute expiration","PERSIST","active expiration without key access","BLPOP timeout","clean shutdown"],"fresh_process_reports_identical":true,"reports_identical_across_host_timezones":true,"reports_identical_under_two_host_cpu_workers":true,"report_sha256":digest(&baseline),"instruction_sha256":original["instruction_sha256"],"trace_sha256":original["trace_sha256"],"instructions":original["instructions"],"virtual_time_ns":original["virtual_time_ns"],"client_output":redis_oracle(&original)?,"seed_count":seeds,"seed_runs":runs,"incorrect_ttl_rejected":true,"negative_control_status":negative["status"],"negative_control_stderr":String::from_utf8_lossy(&negative_stderr),"verification_wall_seconds":start.elapsed().as_secs_f64()});
    fs::write(output, serde_json::to_string_pretty(&summary)? + "\n")?;
    Ok(summary)
}
