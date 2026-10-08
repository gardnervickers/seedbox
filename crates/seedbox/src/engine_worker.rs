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
        _ => error("unsupported internal engine contract; use the CLI retained with this run"),
    })
}

fn error(message: &str) -> ExitCode {
    eprintln!("error: {message}");
    ExitCode::from(2)
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
fn world_worker(executable: OsString, args: Vec<OsString>) -> ExitCode {
    fn run(executable: OsString, args: Vec<OsString>) -> Result<(), String> {
        let args = std::iter::once(executable)
            .chain(args)
            .map(|arg| {
                arg.into_string()
                    .map_err(|_| "worker argument is not UTF-8".to_owned())
            })
            .collect::<Result<Vec<_>, _>>()?;
        let undo = std::env::var("SEEDBOX_UNDO").ok();
        let margin = std::env::var("SEEDBOX_MARGIN").ok();
        let performance =
            seedbox_vmm::PerformanceOptions::parse(undo.as_deref(), margin.as_deref())?;
        let mut options = seedbox_vmm::WorkerOptions::from_args(&args, performance)?;
        if options.pv_clock {
            if let Ok(value) = std::env::var("SEEDBOX_PV_WATCHDOG_MS") {
                options.performance.pv_watchdog_ms = value
                    .parse()
                    .map_err(|e| format!("SEEDBOX_PV_WATCHDOG_MS: {e}"))?;
            }
        }
        if std::env::var_os("SEEDBOX_TRACE_CONSOLE").is_some() {
            options.console_trace = Some(|trace| {
                eprintln!(
                    "[host {:.3}s guest {:.6}s br {}] {}",
                    trace.host_seconds,
                    trace.guest_seconds,
                    trace.branches,
                    String::from_utf8_lossy(trace.line)
                );
            });
        }
        let report = seedbox_vmm::run(&options)?;
        println!(
            "{}",
            serde_json::to_string_pretty(&report).map_err(|error| error.to_string())?
        );
        Ok(())
    }
    match run(executable, args) {
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
