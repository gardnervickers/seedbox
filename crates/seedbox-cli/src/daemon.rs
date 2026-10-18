//! Process environment and platform discovery for the server executable.

use super::{backends::Executor, RuntimeInputs, Signals};
use seedbox_application::{
    config::Simulation,
    execution::{CancellationToken, ExecutionBackend, PreparedInputs},
    store::ContentStore,
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

#[cfg(test)]
mod retained_tests;
mod startup;
pub(super) use startup::connect;

struct NativeBackends {
    inputs: RuntimeInputs,
    environment: Vec<(std::ffi::OsString, std::ffi::OsString)>,
    session_lease: PathBuf,
    session_root: PathBuf,
}

impl BackendProvider for NativeBackends {
    fn project(
        &self,
        configuration: &Simulation,
        backend: seedbox_spec::backend::CpuBackend,
        store: &ContentStore,
        cancellation: &CancellationToken,
    ) -> Result<Backends> {
        let (preparation, execution) = self.inputs.backends(
            store,
            configuration,
            backend,
            cancellation,
            &self.session_root,
        )?;
        let execution = Executor(Box::new(
            (*execution.0)
                .with_launch_context(self.environment.clone(), Some(self.session_lease.clone())),
        ));
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
        let driver_backend = inputs.profile.driver_backend().map_err(Error::Setup)?;
        let descriptor = inputs
            .profile
            .semantic_parameters
            .engine
            .as_ref()
            .ok_or_else(|| {
                Error::Integrity("Driver contract has no executable descriptor".into())
            })?;
        let backend = match driver_backend
            .ok_or_else(|| Error::Setup("Driver contract has no validated CPU backend".into()))?
        {
            seedbox_spec::backend::CpuBackend::Pv => {
                seedbox_driver_execution::driver_world::DriverBackend::Pv
            }
            seedbox_spec::backend::CpuBackend::Tcg => {
                seedbox_driver_execution::driver_world::DriverBackend::Tcg
            }
        };
        let digest = seedbox_driver_execution::driver_world::runtime_digest(inputs, store)?;
        let mut archive = tempfile::NamedTempFile::new_in(&self.session_root).map_err(|error| {
            Error::Setup(format!(
                "retained Driver runtime scratch {}: {error}",
                self.session_root.display()
            ))
        })?;
        store.copy_to(&digest, archive.as_file_mut())?;
        let runtime = super::backends::driver_runtime(
            Some(archive.path().to_path_buf()),
            &self.session_root,
        )?;
        let runtime_backend = match backend {
            seedbox_driver_execution::driver_world::DriverBackend::Pv => {
                seedbox_driver_execution::runtime_assets::Backend::Pv
            }
            seedbox_driver_execution::driver_world::DriverBackend::Tcg => {
                seedbox_driver_execution::runtime_assets::Backend::Tcg
            }
        };
        let architecture = match descriptor.guest_arch.as_str() {
            "x86_64" => seedbox_spec::definition::Architecture::X86_64,
            "aarch64" => seedbox_spec::definition::Architecture::Aarch64,
            value => {
                return Err(Error::Setup(format!(
                    "retained Driver guest architecture {value:?} is not supported"
                )))
            }
        };
        let selected = runtime.select(runtime_backend, architecture)?;
        let executor = seedbox_driver_execution::driver_world::DriverExecutor::new(
            inputs.profile.clone(),
            backend,
            1,
        )?
        .with_runtime(selected)?
        .with_launch_context(self.environment.clone(), Some(self.session_lease.clone()));
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

/// Explicit startup shares the same options and readiness protocol as clients.
pub(super) fn start(
    options: &crate::server_args::ServerArgs,
    foreground: bool,
    startup_handshake: bool,
) -> Result<u8> {
    if foreground || startup_handshake {
        serve(options.state.clone(), options.inputs(), startup_handshake)
    } else {
        startup::background(options)?;
        super::output::status("STARTED", "local server");
        Ok(0)
    }
}

/// Inspect an existing server without automatically starting it.
pub(super) fn status(options: &crate::server_args::ServerArgs, json: bool) -> Result<u8> {
    let root = state_directory(options.state.clone())?;
    let socket = root.join("server.sock");
    if !startup::listening(&socket)? {
        if json {
            super::output::json(&serde_json::json!({ "server": null, "capacity": null }))?;
        } else {
            super::output::status("STOPPED", "local server");
        }
        return Ok(0);
    }
    let client = seedbox_client::Client::connect_to(&socket).map_err(setup)?;
    let runtime = super::simulations::runtime()?;
    runtime.block_on(async {
        let info = client.info().await.map_err(setup)?;
        let capacity = client.capacity().await.map_err(setup)?;
        if json {
            super::output::json(&serde_json::json!({ "server": info, "capacity": capacity }))?;
        } else {
            super::output::status("RUNNING", "local server");
            super::output::field("process", info.process_id);
            super::output::field("slots", capacity.slots);
            super::output::field("occupied", capacity.occupying.len());
            super::output::field("queued", capacity.queued.len());
            super::output::field("paused", capacity.paused);
            capacity_tables(&capacity)?;
        }
        Ok(0)
    })
}

fn capacity_tables(capacity: &seedbox_api::ExecutionCapacity) -> Result<()> {
    let occupants = capacity
        .occupying
        .iter()
        .map(|o| {
            [
                o.simulation.to_string(),
                o.phase
                    .as_ref()
                    .map(super::output::word)
                    .unwrap_or_else(|| "cleanup".into()),
                o.stage
                    .as_ref()
                    .map(super::output::word)
                    .unwrap_or_else(|| "-".into()),
                o.paused.to_string(),
                o.guest_memory_bytes
                    .map(|n| n.to_string())
                    .unwrap_or_else(|| "-".into()),
            ]
        })
        .collect();
    super::output::table(
        [
            "SIMULATION",
            "PHASE",
            "STAGE",
            "PAUSED",
            "GUEST RAM (BYTES)",
        ],
        occupants,
    )?;
    let queued = capacity
        .queued
        .iter()
        .map(|q| {
            [
                q.simulation.to_string(),
                super::simulations::queue_reason(q.reason).into(),
            ]
        })
        .collect();
    super::output::table(["QUEUED", "REASON"], queued)?;
    Ok(())
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
            Arc::new(NativeBackends {
                inputs,
                environment: std::env::vars_os().collect(),
                session_lease: scratch.lease_path(),
                session_root: scratch.path().to_path_buf(),
            }),
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

mod session;
use session::session_directory;

#[cfg(test)]
pub(super) fn session_test_scope() -> std::sync::MutexGuard<'static, ()> {
    session::test_scope()
}

fn remove_stale_socket(socket: &Path) -> Result<()> {
    match std::os::unix::net::UnixStream::connect(socket) {
        Ok(_) => {
            return Err(Error::Setup(format!(
                "server socket {} is already accepting connections",
                socket.display()
            )));
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
            )));
        }
    }
    match fs::symlink_metadata(socket) {
        Ok(metadata) if metadata.file_type().is_socket() => fs::remove_file(socket)?,
        Ok(_) => {
            return Err(Error::Setup(format!(
                "server socket {} is not a socket; choose another state directory",
                socket.display()
            )));
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => {
            return Err(Error::Setup(format!(
                "server socket {}: {e}",
                socket.display()
            )));
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
                return Err(Error::Setup(format!(
                    "server stopped but session cleanup is incomplete: {}; the next startup will retry",
                    entry.path().display()
                )));
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
        let _scope = session::test_scope();
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
        let _scope = session::test_scope();
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
