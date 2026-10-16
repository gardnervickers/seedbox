//! Race-free automatic startup. The lock coordinates clients; the catalogue
//! lock separately prevents two server processes from owning the same state.

use seedbox_application::{Error, Result};
use seedbox_client::Client;
use serde::{Deserialize, Serialize};
use std::{
    fs::OpenOptions,
    io::{IsTerminal, Read, Write},
    os::{
        fd::AsRawFd,
        unix::{fs::OpenOptionsExt, net::UnixStream, process::CommandExt},
    },
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};

const TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "camelCase")]
enum Handshake {
    Ready,
    Error { message: String },
}

pub(super) fn ready() -> Result<()> {
    handshake(&Handshake::Ready)
}

pub(super) fn failed(error: &Error) {
    let message: String = error.to_string().chars().take(2048).collect();
    let _ = handshake(&Handshake::Error { message });
}

fn handshake(value: &Handshake) -> Result<()> {
    let mut output = std::io::stdout().lock();
    serde_json::to_writer(&mut output, value)?;
    output.write_all(b"\n")?;
    output.flush()?;
    Ok(())
}

pub(crate) fn connect(state: Option<PathBuf>) -> Result<Client> {
    let root = super::state_directory(state)?;
    super::prepare_directory(&root)?;
    let socket = root.join("server.sock");
    if listening(&socket)? {
        return Client::connect_to(&socket).map_err(super::setup);
    }
    let lock_path = root.join("startup.lock");
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&lock_path)
        .map_err(|e| Error::Setup(format!("server startup lock {}: {e}", lock_path.display())))?;
    let deadline = Instant::now() + TIMEOUT;
    loop {
        // SAFETY: the lock file remains open until startup has completed.
        if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
            break;
        }
        let error = std::io::Error::last_os_error();
        if error.kind() != std::io::ErrorKind::WouldBlock {
            return Err(super::setup(error));
        }
        if Instant::now() >= deadline {
            return Err(Error::Setup(
                "timed out waiting for server startup lock".into(),
            ));
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    if !listening(&socket)? {
        confirm_start()?;
        start(&root, Instant::now() + TIMEOUT)?;
    }
    Client::connect_to(&socket).map_err(super::setup)
}

fn listening(socket: &Path) -> Result<bool> {
    match UnixStream::connect(socket) {
        Ok(_) => Ok(true),
        Err(e)
            if matches!(
                e.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
            ) =>
        {
            Ok(false)
        }
        Err(e) => Err(Error::Setup(format!(
            "server socket {}: {e}",
            socket.display()
        ))),
    }
}

fn start(root: &Path, deadline: Instant) -> Result<()> {
    let executable = std::env::current_exe().map_err(super::setup)?;
    let mut child = Command::new(&executable)
        .args(["serve", "--startup-handshake", "--state"])
        .arg(root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .map_err(|e| Error::Setup(format!("start server {}: {e}", executable.display())))?;
    let result = (|| {
        let mut output = child
            .stdout
            .take()
            .ok_or_else(|| Error::Setup("server readiness pipe unavailable".into()))?;
        let fd = output.as_raw_fd();
        // SAFETY: fd is the live, owned read end of the readiness pipe.
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
            return Err(super::setup(std::io::Error::last_os_error()));
        }
        let mut bytes = Vec::new();
        loop {
            let mut buffer = [0; 1024];
            match output.read(&mut buffer) {
                Ok(0) => return Err(Error::Setup("server exited before reporting readiness; run seedbox serve to inspect startup".into())),
                Ok(count) => {
                    bytes.extend_from_slice(&buffer[..count]);
                    if bytes.len() > 16 * 1024 { return Err(Error::Setup("server readiness response exceeds limit".into())); }
                    if bytes.contains(&b'\n') {
                        return match serde_json::from_slice::<Handshake>(&bytes)? {
                            Handshake::Ready => Ok(()),
                            Handshake::Error { message } => Err(Error::Setup(message)),
                        };
                    }
                }
                Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted) => {},
                Err(e) => return Err(super::setup(e)),
            }
            if Instant::now() >= deadline {
                return Err(Error::Setup(
                    "server did not become ready within 15 seconds".into(),
                ));
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    })();
    if result.is_err() {
        // This is the process we just created, never an existing server located
        // by a persisted PID. Reap it before releasing the startup lock.
        let _ = child.kill();
        let _ = child.wait();
    }
    result
}

fn confirm_start() -> Result<()> {
    if !std::io::stdin().is_terminal() {
        return Err(Error::Setup(
            "no local server is running; start `seedbox serve` in another terminal".into(),
        ));
    }
    eprint!(
        "no local server is running. Start a persistent Seedbox server on this machine? [y/N] "
    );
    std::io::stderr().flush()?;
    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer)?;
    if !matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
        return Err(Error::Setup(
            "server startup declined; start `seedbox serve` when ready".into(),
        ));
    }
    Ok(())
}
