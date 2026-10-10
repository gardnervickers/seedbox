//! Process environment and platform discovery for the server executable.

use super::{backends::Executor, RuntimeInputs, Signals};
use seedbox_application::{
    config::Simulation,
    execution::{Backend, CancellationToken, ExecutionBackend, PreparedInputs},
    store::ContentStore,
    vmm::VmmExecutor,
    Error, Result,
};
use seedbox_server::{
    catalogue::Catalogue,
    execution::{BackendProvider, Backends, ExecutionService},
    manager::Manager,
};
use std::{
    fs,
    num::NonZeroUsize,
    os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
    sync::Arc,
};

mod startup;
pub(super) use startup::connect;

struct NativeBackends(RuntimeInputs);

impl BackendProvider for NativeBackends {
    fn project(
        &self,
        configuration: &Simulation,
        store: &ContentStore,
        cancellation: &CancellationToken,
    ) -> Result<Backends> {
        super::validate_machine(configuration)?;
        let (preparation, execution) = self.0.backends(store, configuration, cancellation)?;
        Ok(Backends {
            preparation: Box::new(preparation),
            execution: Box::new(execution),
        })
    }

    fn retained(
        &self,
        inputs: &PreparedInputs,
        store: &ContentStore,
    ) -> Result<Box<dyn ExecutionBackend>> {
        let scratch = tempfile::tempdir()?;
        let engine = store.materialize(&inputs.profile.engine, scratch.path())?;
        let executor = if inputs.profile.backend == Backend::Hvf {
            #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
            {
                Executor::Mac(seedbox_application::mac::MacExecutor::from_retained(
                    &engine,
                    store,
                    inputs.profile.clone(),
                )?)
            }
            #[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
            {
                return Err(Error::Setup(
                    "retained HVF inputs require an Apple Silicon Mac".into(),
                ));
            }
        } else {
            Executor::Linux(VmmExecutor::from_retained(
                &engine,
                store,
                inputs.profile.clone(),
            )?)
        };
        Ok(Box::new(executor))
    }
}

pub(super) fn state_directory(explicit: Option<PathBuf>) -> Result<PathBuf> {
    let path = explicit
        .or_else(|| std::env::var_os("SEEDBOX_STATE").map(PathBuf::from))
        .or_else(|| std::env::var_os("XDG_STATE_HOME").map(|p| PathBuf::from(p).join("seedbox")))
        .or_else(|| std::env::var_os("HOME").map(|p| PathBuf::from(p).join(".local/state/seedbox")))
        .ok_or_else(|| {
            Error::Setup("server state location unavailable; set SEEDBOX_STATE or HOME".into())
        })?;
    let path = if path.is_absolute() {
        path
    } else {
        std::env::current_dir()?.join(path)
    };
    let socket = path.join("server.sock");
    std::os::unix::net::SocketAddr::from_pathname(&socket).map_err(|_| {
        Error::Setup(format!(
            "server socket {}: invalid local socket path; set SEEDBOX_STATE to a shorter directory",
            socket.display()
        ))
    })?;
    Ok(path)
}

pub(super) fn serve(
    state: Option<PathBuf>,
    inputs: RuntimeInputs,
    startup_handshake: bool,
) -> Result<u8> {
    let mut ready = false;
    let result = serve_inner(state, inputs, || {
        if startup_handshake {
            startup::ready()?;
        }
        ready = true;
        Ok(())
    });
    if startup_handshake && !ready {
        if let Err(error) = &result {
            startup::failed(error);
        }
    }
    result
}

fn prepare_directory(root: &Path) -> Result<()> {
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(root)
        .map_err(|e| Error::Setup(format!("server state {}: {e}", root.display())))?;
    let metadata = fs::symlink_metadata(root)
        .map_err(|e| Error::Setup(format!("server state {}: {e}", root.display())))?;
    // SAFETY: geteuid has no pointer arguments.
    if !metadata.is_dir()
        || metadata.mode() & 0o077 != 0
        || metadata.uid() != unsafe { libc::geteuid() }
    {
        return Err(Error::Setup(format!(
            "server state {} must be a private directory owned by the current user",
            root.display()
        )));
    }
    Ok(())
}

fn serve_inner(
    state: Option<PathBuf>,
    inputs: RuntimeInputs,
    ready: impl FnOnce() -> Result<()>,
) -> Result<u8> {
    let root = state_directory(state)?;
    prepare_directory(&root)?;
    use std::os::fd::AsRawFd;
    let lock = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(root.join("server.lock"))?;
    // SAFETY: hold exclusive socket ownership through execution shutdown.
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return Err(Error::Setup(
            "a server already owns this state directory".into(),
        ));
    }
    let socket = root.join("server.sock");
    remove_stale_socket(&socket)?;
    let catalogue = Catalogue::in_memory();
    let scratch = session_directory(&root)?;
    let session = scratch.path().to_path_buf();
    let store = session_store(&root, &session)?;
    let manager = Arc::new(Manager::new(catalogue));
    let execution = Arc::new(
        ExecutionService::new(
            manager.clone(),
            session.clone(),
            store,
            Arc::new(NativeBackends(inputs)),
        )
        .map_err(setup)?,
    );
    let shutdown = CancellationToken::default();
    let _signals = Signals::register(&shutdown)?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let result = runtime.block_on(async {
        let listener = tokio::net::UnixListener::bind(&socket)
            .map_err(|e| Error::Setup(format!("server socket {}: {e}", socket.display())))?;
        ready()?;
        let capacity = NonZeroUsize::MIN.saturating_add(1);
        seedbox_server::serve(listener, manager, execution, capacity, shutdown)
            .await
            .map_err(setup)
    });
    let cleanup = scratch
        .close()
        .map_err(|error| Error::Setup(format!("session cleanup {}: {error}", session.display())));
    result?;
    cleanup?;
    // The socket lock and temporary execution files live through worker shutdown.
    // Leave the socket inode for the next owner's stale-socket check.
    Ok(0)
}

/// Binary inputs outlive the server; observations and run evidence do not.
fn session_store(root: &Path, session: &Path) -> Result<ContentStore> {
    let cache = root.join("cache");
    prepare_directory(&cache)?;
    Ok(ContentStore::with_ephemeral_evidence(
        cache,
        session.join("store"),
    ))
}

/// Called only while holding the exclusive server lock and with no listener.
fn session_directory(root: &Path) -> Result<tempfile::TempDir> {
    let sessions = root.join("sessions");
    prepare_directory(&sessions)?;
    for entry in fs::read_dir(&sessions)? {
        let entry = entry?;
        if !entry.file_name().to_string_lossy().starts_with("session-") {
            continue;
        }
        let path = entry.path();
        let result = if entry.file_type()?.is_dir() {
            fs::remove_dir_all(&path)
        } else {
            fs::remove_file(&path)
        };
        result.map_err(|error| {
            Error::Setup(format!("stale session cleanup {}: {error}", path.display()))
        })?;
    }
    tempfile::Builder::new()
        .prefix("session-")
        .tempdir_in(&sessions)
        .map_err(|error| Error::Setup(format!("session directory {}: {error}", sessions.display())))
}

fn remove_stale_socket(socket: &Path) -> Result<()> {
    match std::os::unix::net::UnixStream::connect(socket) {
        Ok(_) => {
            return Err(Error::Setup(format!(
                "server socket {} is already accepting connections",
                socket.display()
            )))
        }
        Err(e)
            if matches!(
                e.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
            ) => {}
        Err(e) => {
            return Err(Error::Setup(format!(
                "server socket {}: {e}",
                socket.display()
            )))
        }
    }
    match fs::symlink_metadata(socket) {
        Ok(metadata) if metadata.file_type().is_socket() => fs::remove_file(socket)?,
        Ok(_) => {
            return Err(Error::Setup(format!(
                "server socket {} is not a socket; choose another state directory",
                socket.display()
            )))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => {
            return Err(Error::Setup(format!(
                "server socket {}: {e}",
                socket.display()
            )))
        }
    }
    Ok(())
}

fn setup(error: impl std::fmt::Display) -> Error {
    Error::Setup(error.to_string())
}

/// Stop the server already listening at this state directory; never start one.
pub(super) fn shutdown(state: Option<PathBuf>) -> Result<u8> {
    let root = state_directory(state)?;
    let socket = root.join("server.sock");
    let client = seedbox_client::Client::connect_to(&socket).map_err(setup)?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let info = runtime.block_on(client.info()).map_err(setup)?;
    let pid = i32::try_from(info.process_id)
        .ok()
        .filter(|pid| *pid > 1)
        .ok_or_else(|| Error::Setup("server returned an invalid process ID".into()))?;
    // SAFETY: signal the local server whose identity we just read from its socket.
    if unsafe { libc::kill(pid, libc::SIGTERM) } != 0 {
        return Err(setup(std::io::Error::last_os_error()));
    }
    use std::os::fd::AsRawFd;
    let ownership = match fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(root.join("server.lock"))
    {
        Ok(file) => Some(file),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(setup(error)),
    };
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        // The server releases this lock only after its scratch cleanup finishes.
        let released = match &ownership {
            Some(file) => {
                (unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) }) == 0
            }
            None => true,
        };
        if released && std::os::unix::net::UnixStream::connect(&socket).is_err() {
            break;
        }
        if std::time::Instant::now() >= deadline {
            return Err(Error::Setup("server shutdown is still in progress".into()));
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    if let Ok(entries) = fs::read_dir(root.join("sessions")) {
        for entry in entries {
            let entry = entry?;
            if entry.file_name().to_string_lossy().starts_with("session-") {
                return Err(Error::Setup(format!("server stopped but session cleanup is incomplete: {}; the next startup will retry", entry.path().display())));
            }
        }
    }
    super::output::status("STOPPED", "local server");
    Ok(0)
}

#[cfg(test)]
mod session_tests {
    use super::*;

    #[test]
    fn cleanup_reclaims_abandoned_sessions_and_preserves_unrelated_files() {
        let root = tempfile::tempdir().unwrap();
        let first = session_directory(root.path()).unwrap();
        fs::write(first.path().join("execution"), b"scratch").unwrap();
        let abandoned = first.keep();
        let unrelated = root.path().join("sessions/keep-me");
        fs::write(&unrelated, b"unrelated").unwrap();
        let next = session_directory(root.path()).unwrap();
        assert!(!abandoned.exists());
        assert!(unrelated.exists());
        let path = next.path().to_path_buf();
        next.close().unwrap();
        assert!(!path.exists());
    }

    #[test]
    fn binary_cache_survives_shutdown_and_abandoned_sessions_without_run_state() {
        let root = tempfile::tempdir().unwrap();
        let mut assets = Vec::new();
        for abandoned in [false, true] {
            let session = session_directory(root.path()).unwrap();
            let store = session_store(root.path(), session.path()).unwrap();
            for bytes in [
                b"kernel".as_slice(),
                b"engine".as_slice(),
                b"boot archive".as_slice(),
                b"checkpoint binary".as_slice(),
            ] {
                assets.push((store.put(bytes).unwrap(), bytes.to_vec()));
            }
            let evidence = store
                .put_unsynced(b"run identity and observations")
                .unwrap();
            fs::write(session.path().join("simulation-state"), b"ephemeral").unwrap();
            let old = session.path().to_path_buf();
            if abandoned {
                let _ = session.keep();
            } else {
                session.close().unwrap();
            }
            let next = session_directory(root.path()).unwrap();
            assert!(!old.exists());
            let restarted = session_store(root.path(), next.path()).unwrap();
            for (digest, bytes) in &assets {
                assert_eq!(&restarted.read(digest).unwrap(), bytes);
            }
            assert!(restarted.read(&evidence).is_err());
            assert!(!next.path().join("simulation-state").exists());
            next.close().unwrap();
        }
    }
}
