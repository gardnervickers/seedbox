use clap::{Parser, Subcommand};
use seedbox::{config, orchestration, Result};
use std::{collections::BTreeMap, path::PathBuf, process::ExitCode};

#[derive(Parser)]
#[command(
    version,
    about = "Seedbox simulation configuration and recorded results"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Validate the authoring subset without resolving images or running guests.
    Validate {
        compose: PathBuf,
        /// Print the resolved configuration (including environment values).
        #[arg(long)]
        json: bool,
    },
    /// Read and verify a saved run manifest.
    Inspect {
        run: PathBuf,
        #[arg(long)]
        json: bool,
    },
}

fn run(cli: Cli) -> Result<()> {
    match cli.command {
        Command::Validate { compose, json } => {
            let environment: BTreeMap<_, _> = std::env::vars().collect();
            let simulation = config::load(&compose, &environment)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&simulation)?);
            } else {
                println!(
                    "VALID: {} ({} services; image compatibility requires preparation)",
                    simulation.name,
                    simulation.services.len()
                );
            }
        }
        Command::Inspect { run, json } => {
            let record = orchestration::inspect(&run)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&record)?);
            } else {
                println!(
                    "{}: {:?} (seed {})",
                    record.attempt, record.outcome, record.identity.seed
                );
                if let Some(replay) = record.replay {
                    println!("replay: {:?}", replay.status);
                }
                if let Some(error) = record.error {
                    println!("error: {error}");
                }
            }
        }
    }
    Ok(())
}

fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::from(2)
        }
    }
}
