//! Standalone compatibility boundary for the Seedbox world worker.

use seedbox_vmm::{ConsoleTrace, PerformanceOptions, WorkerOptions};

fn trace_console(trace: ConsoleTrace<'_>) {
    eprintln!(
        "[host {:.3}s guest {:.6}s br {}] {}",
        trace.host_seconds,
        trace.guest_seconds,
        trace.branches,
        String::from_utf8_lossy(trace.line)
    );
}

fn run() -> Result<serde_json::Value, String> {
    let args: Vec<String> = std::env::args().collect();
    let undo = std::env::var("SEEDBOX_UNDO").ok();
    let margin = std::env::var("SEEDBOX_MARGIN").ok();
    let performance = PerformanceOptions::parse(
        undo.as_deref(), margin.as_deref(), std::env::var("SEEDBOX_PAGE_HASHES").is_ok(),
    )?;
    let mut options = WorkerOptions::from_args(&args, performance)?;
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
