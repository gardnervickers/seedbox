//! Standalone compatibility boundary for the Seedbox world worker.

use seedbox_vmm::{ConsoleTrace, PerformanceOptions, WorkerOptions};

fn trace_console(trace: ConsoleTrace<'_>) {
    eprintln!(
        "[host {:.3}s guest {:.6}s] {}",
        trace.host_seconds,
        trace.guest_seconds,
        String::from_utf8_lossy(trace.line)
    );
}

fn run() -> Result<serde_json::Value, String> {
    let args: Vec<String> = std::env::args().collect();
    let mut options = WorkerOptions::from_args(&args, PerformanceOptions::default())?;
    if let Ok(value) = std::env::var("SEEDBOX_WATCHDOG_MS") {
        options.performance.watchdog_ms = value
            .parse()
            .map_err(|e| format!("SEEDBOX_WATCHDOG_MS: {e}"))?;
    }
    if let Ok(value) = std::env::var("SEEDBOX_WINDOWED_SKEW_US") {
        options.performance.windowed_skew_us = value
            .parse()
            .map_err(|e| format!("SEEDBOX_WINDOWED_SKEW_US: {e}"))?;
    }
    if std::env::var("SEEDBOX_TRACE_CONSOLE").is_ok() {
        options.console_trace = Some(trace_console);
    }
    seedbox_vmm::run(&options)
}

fn main() {
    match run() {
        Ok(v) => println!("{}", serde_json::to_string_pretty(&v).unwrap()),
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
    }
}
