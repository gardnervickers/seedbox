//! Private worker entry, before public argument parsing or state initialization.

use std::{ffi::OsString, process::ExitCode};

pub fn dispatch(args: impl IntoIterator<Item = OsString>) -> Option<ExitCode> {
    let mut args = args.into_iter();
    let executable = args.next().unwrap_or_else(|| "seedbox".into());
    if args.next().as_deref() != Some(std::ffi::OsStr::new("__engine")) {
        return None;
    }
    Some(match args.next().as_deref().and_then(|arg| arg.to_str()) {
        Some(seedbox::engine::INTERNAL_WORKER_V1) => world_worker(executable, args.collect()),
        Some("arm64-fixture-v1") => arm_fixture(executable, args.collect()),
        Some("arm64-machine-v2") => arm_machine(args.collect()),
        Some(seedbox::engine::ARM_WORLD_V1) => arm_world(executable, args.collect()),
        _ => error("unsupported internal engine contract; use the CLI retained with this run"),
    })
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
fn arm_world(executable: OsString, args: Vec<OsString>) -> ExitCode {
    let run = || -> Result<(), String> {
        let mut config = None;
        let mut output = None;
        let mut seed = None;
        let mut host_control = None;
        let mut full_state = false;
        let mut console_limit = 1024 * 1024 * 1024;
        let mut machine_workers = 1;
        let mut args = args.iter();
        while let Some(arg) = args.next() {
            let text = arg.to_str().ok_or("worker argument is not UTF-8")?;
            match text {
                "--config" => {
                    config = Some(std::path::PathBuf::from(
                        args.next().ok_or("--config needs a path")?,
                    ))
                }
                "--out" => {
                    output = Some(std::path::PathBuf::from(
                        args.next().ok_or("--out needs a path")?,
                    ))
                }
                "--seed" => {
                    seed = Some(
                        args.next()
                            .and_then(|n| n.to_str())
                            .ok_or("--seed needs a value")?
                            .parse::<u64>()
                            .map_err(|e| format!("worker seed: {e}"))?,
                    )
                }
                "--host-control" => {
                    host_control = Some(std::path::PathBuf::from(
                        args.next().ok_or("--host-control needs a path")?,
                    ))
                }
                "--digest" => full_state = true,
                "--stream-console" => {}
                "--console-limit" => {
                    console_limit = args
                        .next()
                        .and_then(|n| n.to_str())
                        .ok_or("--console-limit needs a value")?
                        .parse::<usize>()
                        .map_err(|e| format!("console limit: {e}"))?;
                }
                "--machine-workers" => {
                    machine_workers = args
                        .next()
                        .and_then(|n| n.to_str())
                        .and_then(|n| n.parse::<usize>().ok())
                        .filter(|n| (1..=64).contains(n))
                        .ok_or("--machine-workers: use 1..64")?;
                }
                _ => return Err(format!("unsupported ARM world argument {text}")),
            }
        }
        let options = seedbox_arm64::application::world::Options {
            executable: executable.into(),
            config: config.ok_or("worker requires --config")?,
            output: output.ok_or("worker requires --out")?,
            seed: seed.ok_or("worker requires --seed")?,
            full_state,
            host_control,
            console_limit,
            machine_workers,
        };
        let report = seedbox_arm64::application::world::run(&options)?;
        println!(
            "{}",
            serde_json::to_string_pretty(&report).map_err(|e| e.to_string())?
        );
        Ok(())
    };
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => error(&message),
    }
}
#[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
fn arm_world(_: OsString, _: Vec<OsString>) -> ExitCode {
    error("internal HVF world requires Apple Silicon macOS")
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
fn arm_machine(args: Vec<OsString>) -> ExitCode {
    if !args.is_empty() {
        return error("internal ARM machine contract takes no arguments");
    }
    match seedbox_arm64::application::worker::serve::<seedbox_arm64::backend::hvf::Hvf>(
        &mut std::io::stdin().lock(),
        &mut std::io::stdout().lock(),
    ) {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => error(&message),
    }
}
#[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
fn arm_machine(_: Vec<OsString>) -> ExitCode {
    error("internal HVF machine requires Apple Silicon macOS")
}

fn error(message: &str) -> ExitCode {
    eprintln!("error: {message}");
    ExitCode::from(2)
}

/// The x86 world worker takes `--request PATH`, the launch request JSON the
/// CLI wrote beside the world, and prints the engine report on stdout.
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
fn world_worker(_: OsString, args: Vec<OsString>) -> ExitCode {
    fn run(args: Vec<OsString>) -> Result<(), String> {
        let path = match <[OsString; 2]>::try_from(args) {
            Ok([flag, path]) if flag == "--request" => std::path::PathBuf::from(path),
            _ => return Err("internal world worker requires --request PATH".into()),
        };
        let bytes = std::fs::read(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        let mut request: seedbox_vmm::WorkerRequest =
            serde_json::from_slice(&bytes).map_err(|e| format!("{}: {e}", path.display()))?;
        let trace =
            seedbox_vmm::DebugEnv::from_env(|name| std::env::var_os(name))?.apply(&mut request);
        let report = seedbox_vmm::run(&request, trace)?;
        println!(
            "{}",
            serde_json::to_string_pretty(&report).map_err(|error| error.to_string())?
        );
        Ok(())
    }
    match run(args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("error: {message}");
            ExitCode::from(1)
        }
    }
}

#[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
fn world_worker(_: OsString, _: Vec<OsString>) -> ExitCode {
    error("internal Compose world engine requires Linux x86-64; ARM world integration is not available yet")
}

#[cfg(all(target_arch = "aarch64", any(target_os = "linux", target_os = "macos")))]
fn arm_fixture(executable: OsString, args: Vec<OsString>) -> ExitCode {
    seedbox_arm64::cli::main_from(std::iter::once(executable).chain(args))
}

#[cfg(not(all(target_arch = "aarch64", any(target_os = "linux", target_os = "macos"))))]
fn arm_fixture(_: OsString, _: Vec<OsString>) -> ExitCode {
    error("internal ARM fixture engine requires an ARM hypervisor host")
}
