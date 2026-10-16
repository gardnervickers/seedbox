//! Private native worker entrypoint; never used as a public command-line API.
fn main() {
    let arguments = match seedbox_machine::process_lease::worker_arguments(
        std::env::args_os().skip(1).collect(),
    ) {
        Ok(arguments) => arguments,
        Err(error) => {
            eprintln!("PV worker execution lease: {error}");
            std::process::exit(2);
        }
    };
    if arguments.first().and_then(|argument| argument.to_str()) != Some("--seedbox-pv-worker-v1") {
        eprintln!("private seedbox PV worker requires its protocol launch");
        std::process::exit(2);
    }
    if let Err(error) = seedbox_machine_pv::serve_worker() {
        eprintln!("{error}");
        std::process::exit(2);
    }
}
