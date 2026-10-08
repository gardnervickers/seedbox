//! CLI boundary for the experimental arm64 fixture engine.
use clap::{Parser, Subcommand, ValueEnum};
use crate::{machine::Machine, witness::Report, Result};
use std::{ffi::OsString, path::PathBuf, process::ExitCode};
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
        /// Write power-off RAM page SHA-256 diagnostics to a new file (not witnessed).
        #[arg(long, value_name = "FILE")]
        ram_pages: Option<PathBuf>,
    },
}
fn execute(args: Args) -> Result<()> {
    let Command::Run {
        kernel,
        initrd,
        seed,
        backend,
        witness,
        ram_pages,
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
    let mut pages = ram_pages
        .as_ref()
        .map(|path| {
            std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(path)
                .map_err(|e| {
                    format!(
                        "--ram-pages {}: {e}; choose a new output path",
                        path.display()
                    )
                })
        })
        .transpose()?;
    let report = run(
        choice,
        &kernel,
        &initrd,
        seed,
        pages.as_mut().map(|f| f as &mut dyn std::io::Write),
    )?;
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
    println!("WITNESS: {}", report.witness);
    Ok(())
}
fn run(
    choice: Choice,
    kernel: &[u8],
    initrd: &[u8],
    seed: u64,
    ram_pages: Option<&mut dyn std::io::Write>,
) -> Result<Report> {
    match choice {
        Choice::Hvf => {
            #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
            {
                Machine::<crate::backend::hvf::Hvf>::new(kernel, initrd, seed)?
                    .run_with_diagnostics(|| {}, ram_pages)
            }
            #[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
            {
                Err("--backend hvf requires arm64 macOS 15+".into())
            }
        }
        Choice::Kvm => {
            #[cfg(all(target_os = "linux", target_arch = "aarch64"))]
            {
                Machine::<crate::backend::kvm::Kvm>::new(kernel, initrd, seed)?
                    .run_with_diagnostics(|| {}, ram_pages)
            }
            #[cfg(not(all(target_os = "linux", target_arch = "aarch64")))]
            {
                Err("--backend kvm requires Linux arm64".into())
            }
        }
    }
}
/// Run the fixture CLI with explicit arguments, including an argv0 name.
///
/// This is the command-line boundary for the standalone binary and the
/// integrated engine worker; argument diagnostics and fixture output retain
/// the standalone CLI contract.
pub fn main_from<I, T>(args: I) -> ExitCode
where
    I: IntoIterator<Item = T>,
    T: Into<OsString> + Clone,
{
    let args = match Args::try_parse_from(args) {
        Ok(args) => args,
        Err(error) => {
            let code = error.exit_code() as u8;
            let _ = error.print();
            return ExitCode::from(code);
        }
    };
    match execute(args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("ERROR: {e}");
            ExitCode::from(2)
        }
    }
}
