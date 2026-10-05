use clap::{Parser, Subcommand};
use seedbox::{
    config::load_manifest,
    error::Result,
    runtime::{Simulation, canonical},
};
use serde_json::json;
use std::{
    fs,
    io::{self, Write},
    path::PathBuf,
};

#[derive(Parser)]
#[command(version, about = "Seedbox deterministic execution sandbox")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}
#[derive(Subcommand)]
enum Command {
    Run {
        manifest: PathBuf,
        #[arg(long, default_value_t = 0)]
        seed: u64,
        #[arg(long)]
        output: Option<PathBuf>,
        #[arg(long)]
        instruction_trace: bool,
    },
    BuildGuests {
        #[arg(long, default_value = ".")]
        directory: PathBuf,
    },
    Verify {
        #[arg(long, default_value = ".")]
        directory: PathBuf,
        #[arg(long, default_value_t = 64)]
        seeds: u64,
        #[arg(long, default_value = "artifacts/verification.json")]
        output: PathBuf,
    },
    VerifyRedis {
        #[arg(long, default_value = ".")]
        directory: PathBuf,
        #[arg(long, default_value_t = 8)]
        seeds: u64,
        #[arg(long, default_value = "artifacts/redis-verification.json")]
        output: PathBuf,
    },
    #[command(hide = true)]
    LoadWorker,
}

fn execute(cli: Cli) -> Result<i32> {
    match cli.command {
        Command::VerifyRedis {
            directory,
            seeds,
            output,
        } => {
            let summary = seedbox::verification::verify_redis(
                &directory,
                &std::env::current_exe()?,
                seeds,
                &output,
            )?;
            println!("{}", serde_json::to_string_pretty(&summary)?);
            Ok(0)
        }
        Command::LoadWorker => {
            let mut value = 0u64;
            loop {
                value = std::hint::black_box(value.wrapping_mul(1664525).wrapping_add(1013904223));
            }
        }
        Command::Verify {
            directory,
            seeds,
            output,
        } => {
            let summary = seedbox::verification::verify(
                &directory,
                &std::env::current_exe()?,
                seeds,
                &output,
            )?;
            println!("{}", serde_json::to_string_pretty(&summary)?);
            Ok(0)
        }
        Command::BuildGuests { directory } => {
            println!("{}", seedbox::fixtures::build_guests(&directory)?.display());
            Ok(0)
        }
        Command::Run {
            manifest,
            seed,
            output,
            instruction_trace,
        } => {
            let (config, specs) = load_manifest(&manifest)?;
            let report = Simulation::new(specs, seed, config, instruction_trace)?.run()?;
            let mut bytes = canonical(&report);
            bytes.push(b'\n');
            if let Some(output) = output {
                if let Some(parent) = output.parent().filter(|p| !p.as_os_str().is_empty()) {
                    fs::create_dir_all(parent)?;
                }
                fs::write(output, bytes)?;
                let summary: serde_json::Map<_, _> = [
                    "status",
                    "error",
                    "seed",
                    "instructions",
                    "virtual_time_ns",
                    "instruction_sha256",
                    "trace_sha256",
                ]
                .iter()
                .map(|key| ((*key).into(), report[*key].clone()))
                .collect();
                println!("{}", serde_json::Value::Object(summary));
            } else {
                io::stdout().lock().write_all(&bytes)?;
            }
            Ok(if report["status"] == "completed" {
                0
            } else {
                1
            })
        }
    }
}
fn main() {
    let code = match execute(Cli::parse()) {
        Ok(code) => code,
        Err(error) => {
            eprintln!(
                "{}",
                json!({"status": "invalid_input", "error": error.to_string()})
            );
            2
        }
    };
    std::process::exit(code);
}
