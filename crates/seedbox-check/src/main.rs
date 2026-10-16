//! Development-only coordination. Cargo owns tests; the public server owns applications.
mod policy;
mod process;
mod run;
mod test_time_policy;
#[cfg(test)]
mod tests;

fn main() -> std::process::ExitCode {
    let code = match run::entry(std::env::args_os().skip(1).collect()) {
        Ok(code) => code,
        Err(error) => {
            eprintln!("check: {error}");
            2
        }
    };
    std::process::ExitCode::from(code)
}
