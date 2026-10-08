mod cli;
mod engine_worker;

fn main() -> std::process::ExitCode {
    if let Some(status) = engine_worker::dispatch(std::env::args_os()) {
        return status;
    }
    cli::main()
}
