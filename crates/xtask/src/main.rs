use clap::{Parser, Subcommand};
use std::path::Path;

#[derive(Parser)]
struct Args {
    #[command(subcommand)]
    command: Task,
}

#[derive(Subcommand)]
enum Task {
    /// Build and atomically publish architecture-matched guest assets.
    BuildAssets,
    /// Run portable checks and explicitly selected system suites.
    Check(xtask::checks::Args),
    #[command(hide = true)]
    ProbeKvm,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()?;
    match Args::parse().command {
        Task::BuildAssets => println!("{}", xtask::assets::build(&root)?.display()),
        Task::Check(args) => {
            let code = xtask::checks::check(&root, args)?;
            if code != 0 {
                std::process::exit(code);
            }
        }
        Task::ProbeKvm => std::process::exit(if xtask::checks::probe_kvm() { 0 } else { 2 }),
    }
    Ok(())
}
