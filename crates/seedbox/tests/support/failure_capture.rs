//! Bounded observations of a failed test's still-live server. Never changes its verdict.
use seedbox_test_time::HangGuard;
use std::{
    fs::{self, OpenOptions},
    io::{Read, Write},
    path::Path,
    process::{Command, Stdio},
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWriteExt},
    process::Command as AsyncCommand,
};

pub const LIMIT: u64 = 8 * 1024 * 1024;

pub fn read(path: &Path) -> Result<Vec<u8>, String> {
    let mut bytes = Vec::new();
    fs::File::open(path)
        .and_then(|file| file.take(LIMIT + 1).read_to_end(&mut bytes))
        .map_err(|error| format!("{}: {error}", path.display()))?;
    if bytes.len() as u64 > LIMIT {
        return Err(format!("{} exceeds diagnostic 8 MiB limit", path.display()));
    }
    Ok(bytes)
}

pub fn note(directory: &Path, message: &str) {
    eprintln!("failure capture: {message}");
    let result = fs::create_dir_all(directory).and_then(|()| {
        writeln!(
            OpenOptions::new()
                .create(true)
                .append(true)
                .open(directory.join("summary.txt"))?,
            "{message}"
        )
    });
    if let Err(error) = result {
        eprintln!("failure capture summary {}: {error}", directory.display());
    }
}

// One budget covers every observer and both pipe drains in a capture batch.
pub fn budget() -> HangGuard {
    HangGuard::new("failure diagnostics", Duration::from_secs(30))
}

// Keep draining after the retained prefix fills, without renewing the deadline
// for each read or line. Dropping this future closes the reader on cancellation.
async fn drain(mut input: impl AsyncRead + Unpin, path: &Path) -> Result<(), String> {
    let mut output = tokio::fs::File::create(path)
        .await
        .map_err(|e| e.to_string())?;
    let mut total = 0u64;
    let mut buffer = [0; 8192];
    loop {
        let count = input.read(&mut buffer).await.map_err(|e| e.to_string())?;
        if count == 0 {
            break;
        }
        let keep = (LIMIT.saturating_sub(total) as usize).min(count);
        output
            .write_all(&buffer[..keep])
            .await
            .map_err(|e| e.to_string())?;
        total += count as u64;
    }
    output.flush().await.map_err(|e| e.to_string())?;
    if total > LIMIT {
        return Err(format!(
            "{}: retained {LIMIT} bytes; omitted {} bytes",
            path.display(),
            total - LIMIT
        ));
    }
    Ok(())
}

pub fn command(
    command: Command,
    directory: &Path,
    label: &str,
    guard: &HangGuard,
) -> Result<(), String> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?
        .block_on(command_async(command, directory, label, guard))
}

pub async fn command_async(
    command: Command,
    directory: &Path,
    label: &str,
    guard: &HangGuard,
) -> Result<(), String> {
    // Do not launch more observers once this batch/scenario is out of time.
    let deadline = tokio::time::Instant::now() + guard.remaining().map_err(|e| e.to_string())?;
    fs::create_dir_all(directory).map_err(|error| error.to_string())?;
    // Preserve the caller's executable, arguments and environment.
    let mut command = AsyncCommand::from(command);
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let child =
        seedbox_test::process::spawn_with(|| command.spawn()).map_err(|error| error.to_string())?;
    capture_child(child, directory, label, deadline).await
}

async fn capture_child(
    mut child: tokio::process::Child,
    directory: &Path,
    label: &str,
    deadline: tokio::time::Instant,
) -> Result<(), String> {
    let stdout = child.stdout.take().ok_or("missing observation stdout")?;
    let stderr = child.stderr.take().ok_or("missing observation stderr")?;
    let out_path = directory.join(format!("{label}.stdout"));
    let err_path = directory.join(format!("{label}.stderr"));
    let result = tokio::time::timeout_at(deadline, async {
        tokio::join!(
            drain(stdout, &out_path),
            drain(stderr, &err_path),
            child.wait()
        )
    })
    .await;
    let (out, err, status) = match result {
        Ok(result) => result,
        Err(_) => {
            // Cancelling the joined future closes both pipes, even if another
            // process inherited a writer. Kill and reap our owned observer.
            child
                .kill()
                .await
                .map_err(|e| format!("{label}: diagnostic timeout; cleanup: {e}"))?;
            return Err(format!("{label}: diagnostic timeout"));
        }
    };
    let mut errors = Vec::new();
    for result in [out, err] {
        if let Err(error) = result {
            errors.push(error);
        }
    }
    match status {
        Ok(status) if status.success() => {}
        Ok(status) => errors.push(format!("{label}: {status}")),
        Err(error) => errors.push(error.to_string()),
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("; "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncReadExt;

    #[tokio::test]
    async fn diagnostic_timeout_reaps_a_ready_observer() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("ready.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let mut command = AsyncCommand::new("python3");
        command.args(["-c", "import os,signal,socket,sys; s=socket.socket(socket.AF_UNIX); s.connect(sys.argv[1]); s.sendall(str(os.getpid()).encode()); s.close(); print('ready observer', flush=True); signal.pause()"]);
        command
            .arg(&socket)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let child = seedbox_test::process::spawn_with(|| command.spawn()).unwrap();
        let readiness = budget();
        let (mut ready, _) =
            tokio::time::timeout(readiness.remaining().unwrap(), listener.accept())
                .await
                .unwrap()
                .unwrap();
        let mut bytes = Vec::new();
        tokio::time::timeout(
            readiness.remaining().unwrap(),
            ready.read_to_end(&mut bytes),
        )
        .await
        .unwrap()
        .unwrap();
        let pid = String::from_utf8(bytes).unwrap().parse::<i32>().unwrap();
        // Start the short diagnostic deadline only after positive readiness.
        // Production still passes its original absolute 30-second deadline.
        let deadline = tokio::time::Instant::now() + Duration::from_millis(10);
        let error = capture_child(child, directory.path(), "ready", deadline)
            .await
            .unwrap_err();
        assert_eq!(error, "ready: diagnostic timeout");
        let mut status = 0;
        assert_eq!(
            unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) },
            -1
        );
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ECHILD)
        );
    }

    #[tokio::test]
    async fn cancelling_capture_kills_owned_observer_without_blocking_runtime() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("ready.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let mut command = Command::new("python3");
        command.args(["-c", "import os,signal,socket,sys; s=socket.socket(socket.AF_UNIX); s.connect(sys.argv[1]); s.sendall(str(os.getpid()).encode()); s.close(); signal.pause()"]);
        command.arg(&socket);
        let guard = budget();
        let pid = {
            let capture = command_async(command, directory.path(), "cancelled", &guard);
            tokio::pin!(capture);
            // The same single-thread runtime must process readiness while the
            // diagnostic child is stuck. A synchronous child.wait deadlocks here.
            tokio::select! {
                result = &mut capture => panic!("observer exited before cancellation: {result:?}"),
                ready = tokio::time::timeout(guard.remaining().unwrap(), listener.accept()) => {
                    let (mut ready, _) = ready.unwrap().unwrap();
                    let mut bytes = Vec::new();
                    ready.read_to_end(&mut bytes).await.unwrap();
                    String::from_utf8(bytes).unwrap().parse::<i32>().unwrap()
                }
            }
            // Dropping the future cancels its owned child and both readers.
        };
        // Let Tokio's orphan reaper run while checking the positive cleanup
        // event. No elapsed-time or timed-absence assertion establishes order.
        tokio::task::spawn_blocking(move || {
            budget()
                .wait_for(|| {
                    let result = unsafe { libc::kill(pid, 0) };
                    (result == -1
                        && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH))
                    .then_some(())
                })
                .unwrap();
        })
        .await
        .unwrap();
    }
}

#[test]
fn expired_budget_refuses_to_spawn_another_diagnostic() {
    let directory = tempfile::tempdir().unwrap();
    let guard = HangGuard::new("expired diagnostic batch", Duration::ZERO);
    let error = command(
        Command::new("does-not-exist"),
        directory.path(),
        "unused",
        &guard,
    )
    .unwrap_err();
    assert!(error.contains("test infrastructure hang"), "{error}");
}
