use clap::{Parser, Subcommand, ValueEnum};
use seedbox_arm64::{machine::Machine, witness::Report, Result};
use std::{path::PathBuf, process::ExitCode};
#[derive(Clone, Copy, ValueEnum)]
enum Choice {
    /// Use Hypervisor.framework (arm64 macOS 15+, signed binary).
    Hvf,
    /// Use KVM (Linux arm64 with /dev/kvm and vGICv3).
    Kvm,
}
#[derive(Parser)]
#[command(version, about = "Experimental deterministic arm64 Linux PV machine")]
struct Args {
    #[command(subcommand)]
    command: Command,
}
#[derive(Subcommand)]
enum Command {
    /// Run a minimal PV guest (arm64 hypervisor host required).
    Run {
        /// Read an uncompressed arm64 Linux Image.
        #[arg(long)]
        kernel: PathBuf,
        /// Read a static initramfs in newc format.
        #[arg(long)]
        initrd: PathBuf,
        /// Select the deterministic run seed.
        #[arg(long)]
        seed: u64,
        /// Select the host backend [default: hvf on macOS, kvm on Linux].
        #[arg(long, value_enum)]
        backend: Option<Choice>,
        /// Write the report, full exit trace, and console to a new JSON file.
        #[arg(long)]
        witness: Option<PathBuf>,
    },
}
fn execute(args: Args) -> Result<()> {
    let Command::Run {
        kernel,
        initrd,
        seed,
        backend,
        witness,
    } = args.command;
    let choice = backend.unwrap_or(if cfg!(target_os = "macos") {
        Choice::Hvf
    } else {
        Choice::Kvm
    });
    let kernel =
        std::fs::read(&kernel).map_err(|e| format!("--kernel {}: {e}", kernel.display()))?;
    let initrd =
        std::fs::read(&initrd).map_err(|e| format!("--initrd {}: {e}", initrd.display()))?;
    let report = run(choice, &kernel, &initrd, seed)?;
    if let Some(path) = witness {
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(|e| {
                format!(
                    "--witness {}: {e}; choose a new output path",
                    path.display()
                )
            })?;
        let json =
            serde_json::to_vec_pretty(&report).map_err(|e| format!("encode witness: {e}"))?;
        file.write_all(&json)
            .and_then(|_| file.write_all(b"\n"))
            .map_err(|e| format!("write {}: {e}", path.display()))?;
    }
    eprint!("{}", report.console);
    eprintln!(
        "exits: {}\nvirtual_ns: {}\nwall_seconds: {:.6}",
        report.exits, report.virtual_ns, report.wall_seconds
    );
    println!("PASS: {}", report.witness);
    Ok(())
}
fn run(choice: Choice, kernel: &[u8], initrd: &[u8], seed: u64) -> Result<Report> {
    match choice {
        Choice::Hvf => {
            #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
            {
                Machine::<seedbox_arm64::backend::hvf::Hvf>::new(kernel, initrd, seed)?.run(|| {})
            }
            #[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
            {
                Err("--backend hvf requires arm64 macOS 15+".into())
            }
        }
        Choice::Kvm => {
            #[cfg(all(target_os = "linux", target_arch = "aarch64"))]
            {
                Machine::<seedbox_arm64::backend::kvm::Kvm>::new(kernel, initrd, seed)?.run(|| {})
            }
            #[cfg(not(all(target_os = "linux", target_arch = "aarch64")))]
            {
                Err("--backend kvm requires Linux arm64".into())
            }
        }
    }
}
fn main() -> ExitCode {
    match execute(Args::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("ERROR: {e}");
            ExitCode::from(2)
        }
    }
}
