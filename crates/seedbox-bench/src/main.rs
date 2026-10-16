//! Compare search strategies on an evaluation workload. Each strategy runs
//! once per campaign seed at the same member budget, through `seedbox
//! campaign --json` and `seedbox explore --json` on a running server, so the
//! comparison exercises exactly what users run. A run that finds nothing is
//! censored at the budget, never counted as a success.
use clap::{Parser, ValueEnum};
use seedbox_api::{Campaign, SeedRangeStatus, Simulation, SimulationStatus, TestVerdict};
use seedbox_engine_protocol::property::{Kind, PropertyStatus};
use serde::Serialize;
use std::path::PathBuf;
use std::process::Command;
use std::time::Instant;

#[derive(Parser)]
#[command(version, about)]
struct Args {
    /// Project or definition to evaluate.
    project: PathBuf,
    /// Fault profile every strategy uses.
    #[arg(long)]
    fault_profile: Option<PathBuf>,
    /// Members per run.
    #[arg(long, default_value_t = 16)]
    budget: u32,
    /// Campaign seeds; each run's first root seed and planner seed.
    #[arg(long, value_delimiter = ',', default_value = "200,300,400,500,600")]
    seeds: Vec<u64>,
    /// Strategies to compare; repeatable.
    #[arg(long = "strategy", value_enum, default_values_t = [Strategy::Campaign, Strategy::Sweep])]
    strategies: Vec<Strategy>,
    /// Extra arguments for `seedbox campaign`, such as --anchor 2s; repeatable.
    #[arg(long = "campaign-arg", allow_hyphen_values = true)]
    campaign_args: Vec<String>,
    /// The CLI to run [default: target/release/seedbox].
    #[arg(long, default_value = "target/release/seedbox")]
    seedbox: PathBuf,
    /// Print one JSON summary instead of a table.
    #[arg(long)]
    json: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum, Serialize)]
#[serde(rename_all = "camelCase")]
enum Strategy {
    /// `seedbox campaign`: seed-sweep roots, then guided branches.
    Campaign,
    /// `seedbox explore`: consecutive seeds.
    Sweep,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Run {
    strategy: Strategy,
    seed: u64,
    members: u32,
    failing: u32,
    incomplete: u32,
    /// Passing members that never evaluated a declared `always` property,
    /// such as a history check that ran out of search budget: not evidence
    /// of correctness.
    unchecked: u32,
    /// Members up to and including the first failure; None is censored.
    first_failure: Option<u32>,
    /// Distinct failure groups (property IDs, or the oracle).
    groups: Vec<String>,
    host_seconds: f64,
}

/// A passing member whose declared `always` property was never evaluated.
fn unchecked(status: &SimulationStatus) -> bool {
    status.verdict() == TestVerdict::Passed
        && status.properties.as_ref().is_some_and(|a| {
            a.properties
                .iter()
                .any(|p| p.kind == Kind::Always && p.status == PropertyStatus::Unmet)
        })
}

fn member_status(args: &Args, id: &str) -> Result<SimulationStatus, String> {
    let output = Command::new(&args.seedbox)
        .args(["get", "simulation", id, "--json"])
        .output()
        .map_err(|e| format!("{}: {e}", args.seedbox.display()))?;
    let simulation: Simulation =
        serde_json::from_slice(&output.stdout).map_err(|e| format!("simulation {id}: {e}"))?;
    Ok(simulation.status)
}

fn run(args: &Args, strategy: Strategy, seed: u64) -> Result<Run, String> {
    let mut command = Command::new(&args.seedbox);
    let seed_text = seed.to_string();
    let budget = args.budget.to_string();
    match strategy {
        Strategy::Campaign => {
            command.arg("campaign").arg(&args.project).args([
                "--seed",
                &seed_text,
                "--planner-seed",
                &seed_text,
                "--budget",
                &budget,
            ]);
            command.args(&args.campaign_args);
        }
        Strategy::Sweep => {
            command
                .arg("explore")
                .arg(&args.project)
                .args(["--seed", &seed_text, "--count", &budget]);
        }
    }
    if let Some(profile) = &args.fault_profile {
        command.arg("--fault-profile").arg(profile);
    }
    command.arg("--json");
    let started = Instant::now();
    let output = command
        .output()
        .map_err(|e| format!("{}: {e}", args.seedbox.display()))?;
    let host_seconds = started.elapsed().as_secs_f64();
    let text = String::from_utf8_lossy(&output.stdout);
    let mut unchecked_members = 0;
    let (verdicts, groups): (Vec<Option<TestVerdict>>, Vec<String>) = match strategy {
        Strategy::Campaign => {
            let campaign: Campaign = serde_json::from_str(&text).map_err(|e| {
                format!(
                    "campaign seed {seed}: {e}: {}",
                    String::from_utf8_lossy(&output.stderr)
                )
            })?;
            let mut members = campaign.members;
            members.sort_by_key(|m| m.ordinal);
            for member in &members {
                if member.verdict != Some(TestVerdict::Passed) {
                    continue;
                }
                if let Some(id) = &member.simulation {
                    unchecked_members += u32::from(unchecked(&member_status(args, id.as_str())?));
                }
            }
            (
                members.iter().map(|m| m.verdict).collect(),
                campaign.findings.into_iter().map(|f| f.group).collect(),
            )
        }
        Strategy::Sweep => {
            let range: SeedRangeStatus = serde_json::from_str(&text).map_err(|e| {
                format!(
                    "sweep seed {seed}: {e}: {}",
                    String::from_utf8_lossy(&output.stderr)
                )
            })?;
            let mut groups = std::collections::BTreeSet::new();
            for member in &range.members {
                unchecked_members += u32::from(unchecked(&member.status));
                if let Some(assessment) = &member.status.properties {
                    for p in &assessment.properties {
                        if p.status == PropertyStatus::Violated {
                            groups.insert(p.id.clone());
                        }
                    }
                }
                if member.status.oracle_verdict() == TestVerdict::Failed {
                    groups.insert("oracle".into());
                }
            }
            (
                range
                    .members
                    .iter()
                    .map(|m| Some(m.status.verdict()))
                    .collect(),
                groups.into_iter().collect(),
            )
        }
    };
    let failing = verdicts
        .iter()
        .filter(|v| **v == Some(TestVerdict::Failed))
        .count() as u32;
    let incomplete = verdicts
        .iter()
        .filter(|v| !matches!(v, Some(TestVerdict::Failed | TestVerdict::Passed)))
        .count() as u32;
    let first_failure = verdicts
        .iter()
        .position(|v| *v == Some(TestVerdict::Failed))
        .map(|i| i as u32 + 1);
    Ok(Run {
        strategy,
        seed,
        members: verdicts.len() as u32,
        failing,
        incomplete,
        unchecked: unchecked_members,
        first_failure,
        groups,
        host_seconds,
    })
}

fn main() {
    let args = Args::parse();
    let mut runs = Vec::new();
    for &seed in &args.seeds {
        for &strategy in &args.strategies {
            match run(&args, strategy, seed) {
                Ok(run) => {
                    eprintln!(
                        "{:?} seed {seed}: first {} failing {}/{} ({:.0}s)",
                        strategy,
                        run.first_failure
                            .map_or("censored".into(), |f| f.to_string()),
                        run.failing,
                        run.members,
                        run.host_seconds
                    );
                    runs.push(run);
                }
                Err(error) => {
                    eprintln!("error: {error}");
                    std::process::exit(2);
                }
            }
        }
    }
    if args.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&runs).expect("summary serializes")
        );
        return;
    }
    println!(
        "STRATEGY  RUNS  FOUND  FAILING  INCOMPLETE  UNCHECKED  FIRST-FAILURE (censored as >budget)"
    );
    for &strategy in &args.strategies {
        let mine: Vec<&Run> = runs.iter().filter(|r| r.strategy == strategy).collect();
        let found = mine.iter().filter(|r| r.first_failure.is_some()).count();
        let failing: u32 = mine.iter().map(|r| r.failing).sum();
        let members: u32 = mine.iter().map(|r| r.members).sum();
        let incomplete: u32 = mine.iter().map(|r| r.incomplete).sum();
        let unchecked: u32 = mine.iter().map(|r| r.unchecked).sum();
        let firsts: Vec<String> = mine
            .iter()
            .map(|r| {
                r.first_failure
                    .map_or(format!(">{}", args.budget), |f| f.to_string())
            })
            .collect();
        println!(
            "{:<9} {:<5} {:<6} {:<8} {:<11} {:<10} {}",
            format!("{strategy:?}").to_lowercase(),
            mine.len(),
            found,
            format!("{failing}/{members}"),
            incomplete,
            unchecked,
            firsts.join(", ")
        );
    }
}
