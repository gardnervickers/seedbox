//! Private worker entry, before public argument parsing or state initialization.

use std::{ffi::OsString, process::ExitCode};

pub fn dispatch(args: impl IntoIterator<Item = OsString>) -> Option<ExitCode> {
    let mut args = args.into_iter();
    let executable = args.next().unwrap_or_else(|| "seedbox".into());
    if args.next().as_deref() != Some(std::ffi::OsStr::new("__engine")) {
        return None;
    }
    let contract = args.next();
    let mut args = args.peekable();
    if args.peek().is_some_and(|arg| arg == "--owner-pipe") {
        args.next();
        if let Err(message) = watch_owner() {
            return Some(error(&message));
        }
    }
    Some(match contract.as_deref().and_then(|arg| arg.to_str()) {
        Some(seedbox_application::engine::INTERNAL_WORKER_V1) => {
            world_worker(executable, args.collect())
        }
        Some("arm64-fixture-v1") => arm_fixture(executable, args.collect()),
        Some("arm64-machine-v2") => arm_machine(args.collect()),
        Some(seedbox_application::engine::ARM_WORLD_V1) => arm_world(executable, args.collect()),
        _ => error("unsupported internal engine contract; use the CLI retained with this run"),
    })
}

/// The parent owns the only write end. Reading happens independently of guest
/// execution, so even a guest with no exits cannot outlive its server. Nested
/// ARM workers inherit the world's process group and are stopped with it.
#[cfg(unix)]
fn watch_owner() -> Result<(), String> {
    use std::io::Read;
    // SAFETY: these process identity queries have no pointer arguments.
    let pid = unsafe { libc::getpid() };
    if unsafe { libc::getpgrp() } != pid {
        return Err("owned worker must lead its process group".into());
    }
    std::thread::Builder::new()
        .name("owner-lifetime".into())
        .spawn(move || {
            let mut input = std::io::stdin().lock();
            let mut byte = [0];
            loop {
                match input.read(&mut byte) {
                    Ok(0) => break,
                    Ok(_) => continue,
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(_) => break,
                }
            }
            // SAFETY: this is our still-live process group, not a persisted PID that
            // might have been recycled. SIGKILL releases hypervisor ownership even
            // when other worker threads are blocked.
            unsafe {
                libc::kill(-pid, libc::SIGKILL);
            }
        })
        .map_err(|e| format!("watch execution owner: {e}"))?;
    Ok(())
}

#[cfg(not(unix))]
fn watch_owner() -> Result<(), String> {
    Err("owned execution requires Unix".into())
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
fn arm_world(executable: OsString, args: Vec<OsString>) -> ExitCode {
    let run = || -> Result<(), String> {
        let mut config = None;
        let mut output = None;
        let mut seed = None;
        let mut host_control = None;
        let mut start_paused = false;
        let mut observations = None;
        let mut interventions = None;
        let mut resume_checkpoint = None;
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
                "--resume-checkpoint" => {
                    resume_checkpoint = Some(std::path::PathBuf::from(
                        args.next().ok_or("--resume-checkpoint needs a path")?,
                    ));
                }
                "--interventions" => {
                    use std::io::Read;
                    let path =
                        std::path::Path::new(args.next().ok_or("--interventions needs a path")?);
                    let limit = seedbox_engine_protocol::intervention::MAX_BYTES;
                    let mut bytes = Vec::new();
                    std::fs::File::open(path)
                        .and_then(|file| file.take(limit as u64 + 1).read_to_end(&mut bytes))
                        .map_err(|e| format!("interventions {}: {e}", path.display()))?;
                    if bytes.len() > limit {
                        return Err(format!("interventions exceed {limit} bytes"));
                    }
                    interventions = Some(
                        serde_json::from_slice(&bytes)
                            .map_err(|e| format!("interventions {}: {e}", path.display()))?,
                    );
                }
                "--observations" => {
                    observations = Some(std::path::PathBuf::from(
                        args.next().ok_or("--observations needs a path")?,
                    ));
                }
                "--start-paused" => start_paused = true,
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
            start_paused,
            interventions,
            resume_checkpoint,
            observations,
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
