fn main() -> std::process::ExitCode {
    seedbox_arm64::cli::main_from(std::env::args_os())
}
