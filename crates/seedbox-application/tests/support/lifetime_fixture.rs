//! Process-only fixture. No VM or QEMU acceptance is implied.
use seedbox_application::process_lifetime::{self, OwnedExecution, ANCHOR_ENTRY, MONITOR_ENTRY};
use std::{
    fs, io,
    path::Path,
    process::{Command, Stdio},
    time::{Duration, Instant},
};

fn wait(path: &Path) -> io::Result<()> {
    let start = Instant::now();
    while !path.exists() {
        if start.elapsed() > Duration::from_secs(15) {
            return Err(io::Error::other(format!(
                "fixture timed out waiting for {}",
                path.display()
            )));
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    Ok(())
}
fn launch(
    root: &Path,
    mode: &std::ffi::OsStr,
    session: Option<&Path>,
) -> io::Result<OwnedExecution> {
    let executable = std::env::current_exe()?;
    OwnedExecution::spawn(
        &executable,
        &executable,
        &[
            seedbox_machine::process_lease::ENTRY.into(),
            "--worker".into(),
            root.as_os_str().to_owned(),
            mode.to_owned(),
        ],
        &[],
        fs::File::create(root.join("stdout"))?,
        fs::File::create(root.join("stderr"))?,
        process_lifetime::LeasePaths {
            execution: &root.join(process_lifetime::EXECUTION_LEASE_FILE),
            session,
        },
    )
}
fn run() -> io::Result<()> {
    let arguments =
        seedbox_machine::process_lease::worker_arguments(std::env::args_os().skip(1).collect())?;
    let entry = arguments.first().and_then(|s| s.to_str()).unwrap_or("");
    match entry {
        MONITOR_ENTRY => process_lifetime::serve_monitor(arguments[1..].to_vec()),
        ANCHOR_ENTRY => process_lifetime::serve_anchor(),
        "--server" => {
            let root = Path::new(&arguments[1]);
            let mode = &arguments[2];
            use std::os::fd::AsRawFd;
            let session = fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create_new(true)
                .open(root.join(process_lifetime::SESSION_LEASE_FILE))?;
            if unsafe { libc::flock(session.as_raw_fd(), libc::LOCK_SH | libc::LOCK_NB) } != 0 {
                return Err(io::Error::last_os_error());
            }
            let mut execution = launch(
                root,
                mode,
                Some(&root.join(process_lifetime::SESSION_LEASE_FILE)),
            )?;
            wait(&root.join("holder-ready"))?;
            fs::write(root.join("server-ready"), [])?;
            let completion = execution.finish()?;
            fs::write(root.join("completion"), serde_json::to_vec(completion)?)?;
            if completion.cleanup_authorized() {
                fs::remove_file(root.join("execution-data"))?;
            }
            Ok(())
        }
        "--siblings" => {
            let root = Path::new(&arguments[1]);
            let a = root.join("a");
            let b = root.join("b");
            fs::create_dir(&a)?;
            fs::create_dir(&b)?;
            let mut first = launch(&a, std::ffi::OsStr::new("ordinary"), None)?;
            let mut second = launch(&b, std::ffi::OsStr::new("ordinary"), None)?;
            wait(&a.join("holder-ready"))?;
            wait(&b.join("holder-ready"))?;
            fs::write(root.join("server-ready"), [])?;
            wait(&root.join("cancel-first"))?;
            first.cancel();
            let completion = first.finish()?;
            fs::write(a.join("completion"), serde_json::to_vec(completion)?)?;
            let completion = second.finish()?;
            fs::write(b.join("completion"), serde_json::to_vec(completion)?)?;
            Ok(())
        }
        "--worker" => {
            let root = Path::new(&arguments[1]);
            let mode = arguments[2].to_str().unwrap_or("");
            if mode == "environment" {
                use std::os::unix::ffi::OsStringExt;
                let mut environment: Vec<_> = std::env::vars_os()
                    .map(|(key, value)| (key.into_vec(), value.into_vec()))
                    .collect();
                environment.sort();
                fs::write(
                    root.join("worker-environment"),
                    serde_json::to_vec(&environment)?,
                )?;
            }
            fs::write(root.join("worker-pid"), std::process::id().to_string())?;
            let mut holder = Command::new(std::env::current_exe()?);
            if seedbox_machine::process_lease::inherit(&mut holder)? {
                holder.arg(seedbox_machine::process_lease::ENTRY);
            }
            holder
                .arg("--holder")
                .arg(root)
                .arg(mode)
                .stdin(Stdio::null());
            let _child = holder.spawn()?;
            drop(holder);
            wait(&root.join("holder-ready"))?;
            fs::write(root.join("worker-ready"), [])?;
            if mode == "stopped" {
                unsafe {
                    libc::raise(libc::SIGSTOP);
                }
            }
            wait(&root.join("kill-worker"))?;
            unsafe {
                libc::raise(libc::SIGKILL);
            }
            Err(io::Error::other("SIGKILL returned"))
        }
        "--holder" => {
            let root = Path::new(&arguments[1]);
            if arguments[2] == "escaped" && unsafe { libc::setsid() } < 0 {
                return Err(io::Error::last_os_error());
            }
            fs::write(root.join("holder-pid"), std::process::id().to_string())?;
            fs::write(root.join("holder-ready"), [])?;
            wait(&root.join("exit-holder"))
        }
        _ => Err(io::Error::other("expected private process fixture entry")),
    }
}
fn main() {
    if let Err(error) = run() {
        eprintln!("{error}");
        std::process::exit(2);
    }
}
