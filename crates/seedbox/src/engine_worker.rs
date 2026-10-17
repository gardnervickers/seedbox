//! Private worker entry, before public argument parsing or state initialization.

use std::{ffi::OsString, process::ExitCode};

pub fn dispatch(args: impl IntoIterator<Item = OsString>) -> Option<ExitCode> {
    let mut args = args.into_iter();
    let _executable = args.next();
    #[cfg(unix)]
    let remaining = match seedbox_application::process_lifetime::worker_arguments(args.collect()) {
        Ok(arguments) => arguments,
        Err(message) => return Some(error(&format!("execution lease: {message}"))),
    };
    #[cfg(not(unix))]
    let remaining: Vec<_> = args.collect();
    let mut args = remaining.into_iter();
    match args.next().as_deref() {
        #[cfg(unix)]
        Some(marker) if marker == seedbox_application::process_lifetime::MONITOR_ENTRY => {
            return Some(
                match seedbox_application::process_lifetime::serve_monitor(args.collect()) {
                    Ok(()) => ExitCode::SUCCESS,
                    Err(message) => error(&format!("execution monitor: {message}")),
                },
            );
        }
        #[cfg(unix)]
        Some(marker) if marker == seedbox_application::process_lifetime::ANCHOR_ENTRY => {
            return Some(
                match seedbox_application::process_lifetime::serve_anchor() {
                    Ok(()) => ExitCode::SUCCESS,
                    Err(message) => error(&format!("execution anchor: {message}")),
                },
            );
        }
        Some(marker) if marker == "--seedbox-pv-worker-v1" => {
            return Some(match seedbox_machine_pv::serve_worker() {
                Ok(()) => ExitCode::SUCCESS,
                Err(message) => {
                    eprintln!("{message}");
                    ExitCode::from(2)
                }
            });
        }
        Some(marker) if marker == "__engine" => {}
        _ => return None,
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
        Some(seedbox_application::engine::DRIVER_WORLD_V1) => driver_world(args.collect()),
        _ => error("unsupported internal engine contract; use the CLI retained with this run"),
    })
}

fn driver_world(arguments: Vec<OsString>) -> ExitCode {
    use std::io::{Read, Write};
    let run = || -> Result<seedbox_application::execution::ExecutionReport, String> {
        if arguments.len() != 2 || arguments[0] != "--request" {
            return Err("driver-world-v1 needs exactly --request PATH; use the launch recorded with this run".into());
        }
        let path = std::path::Path::new(&arguments[1]);
        let file = std::fs::File::open(path)
            .map_err(|error| format!("Driver request {}: {error}", path.display()))?;
        let mut bytes = vec![];
        file.take(seedbox_engine_protocol::driver_world::MAX_ENVELOPE_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|error| format!("Driver request {}: {error}", path.display()))?;
        if bytes.len() as u64 > seedbox_engine_protocol::driver_world::MAX_ENVELOPE_BYTES {
            return Err("Driver launch request exceeds the metadata envelope limit".into());
        }
        let request = serde_json::from_slice(&bytes)
            .map_err(|error| format!("Driver request {}: {error}", path.display()))?;
        seedbox_application::driver_world::run_request(&request).map_err(|error| error.to_string())
    };
    match run() {
        Ok(report) => {
            let mut output = std::io::stdout().lock();
            if let Err(error) = serde_json::to_writer(&mut output, &report)
                .and_then(|()| output.flush().map_err(serde_json::Error::io))
            {
                return self::error(&format!("Driver report: {error}"));
            }
            if report.reason == seedbox_application::execution::StopReason::Error {
                ExitCode::from(2)
            } else {
                ExitCode::SUCCESS
            }
        }
        Err(message) => error(&message),
    }
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

fn error(message: &str) -> ExitCode {
    eprintln!("error: {message}");
    ExitCode::from(2)
}
