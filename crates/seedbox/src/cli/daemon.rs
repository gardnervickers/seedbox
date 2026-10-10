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
    os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt},
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
    let catalogue = Catalogue::open(&root.join("catalogue")).map_err(setup)?;
    let socket = root.join("server.sock");
    remove_stale_socket(&socket)?;
    let manager = Arc::new(Manager::new(catalogue));
    manager.recover_interrupted().map_err(setup)?;
    let execution = Arc::new(
        ExecutionService::new(
            manager.clone(),
            root.clone(),
            ContentStore::new(root.join("store")),
            Arc::new(NativeBackends(inputs)),
        )
        .map_err(setup)?,
    );
    let shutdown = CancellationToken::default();
    let _signals = Signals::register(&shutdown)?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async {
        let listener = tokio::net::UnixListener::bind(&socket)
            .map_err(|e| Error::Setup(format!("server socket {}: {e}", socket.display())))?;
        ready()?;
        let capacity = NonZeroUsize::MIN.saturating_add(1);
        seedbox_server::serve(listener, manager, execution, capacity, shutdown)
            .await
            .map_err(setup)
    })?;
    // Catalogue ownership lasted through shutdown; the socket cannot belong to
    // a replacement server until the execution service has released its lock.
    // Leave the inode for the next owner's stale-socket check to avoid that race.
    Ok(0)
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
