//! Development-only low-level worker batch supervisor, not application execution.
#[cfg(any(target_os = "linux", test))]
mod options;
#[cfg(target_os = "linux")]
mod supervisor;

fn main() -> std::process::ExitCode {
    #[cfg(target_os = "linux")]
    {
        // The private run function can be tested unprivileged; the production
        // entry retains batch.sh's root requirement without an override.
        if unsafe { libc::geteuid() } != 0 {
            eprintln!("batch: run through batch.sh (root supervisor required)");
            return std::process::ExitCode::FAILURE;
        }
        let result = options::Options::from_env().and_then(supervisor::run_batch);
        match result {
            Ok(code) => std::process::ExitCode::from(code as u8),
            Err(error) => {
                eprintln!("batch: {error}");
                std::process::ExitCode::FAILURE
            }
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        eprintln!("batch: Linux waitid/WNOWAIT is required");
        std::process::ExitCode::FAILURE
    }
}

#[cfg(test)]
mod options_tests;

#[cfg(all(test, target_os = "linux"))]
mod tests;
