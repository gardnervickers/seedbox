fn main() -> std::process::ExitCode {
    let arguments: Vec<_> = std::env::args_os().collect();
    if arguments.get(1).is_some_and(|arg| arg == "__engine")
        && arguments
            .get(2)
            .is_some_and(|arg| arg == seedbox_arm64::application::worker::CONTRACT)
    {
        #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
        let result = seedbox_arm64::application::worker::serve::<seedbox_arm64::backend::hvf::Hvf>(
            &mut std::io::stdin().lock(),
            &mut std::io::stdout().lock(),
        );
        #[cfg(all(target_os = "linux", target_arch = "aarch64"))]
        let result = seedbox_arm64::application::worker::serve::<seedbox_arm64::backend::kvm::Kvm>(
            &mut std::io::stdin().lock(),
            &mut std::io::stdout().lock(),
        );
        #[cfg(not(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux"))))]
        let result: Result<(), String> =
            Err("ARM machine worker requires an arm64 hypervisor host".into());
        return match result {
            Ok(()) => std::process::ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("error: {error}");
                std::process::ExitCode::from(2)
            }
        };
    }
    seedbox_arm64::cli::main_from(arguments)
}
