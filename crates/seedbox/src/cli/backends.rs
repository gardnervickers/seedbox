//! Platform selection stays at the command-line boundary.
use seedbox_application::{
    config::Simulation,
    execution::{
        CancellationToken, ExecutionBackend, ExecutionProfile, ExecutionReport, ExecutionRequest,
        PreparationBackend, PreparedInputs,
    },
    preparation::{OciPreparer, PreparationContext},
    store::ContentStore,
    vmm::VmmExecutor,
    Result,
};
use std::{
    collections::BTreeMap,
    ops::{Deref, DerefMut},
    path::Path,
};

pub enum Executor {
    Linux(VmmExecutor),
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    Mac(seedbox_application::mac::MacExecutor),
}
impl Executor {
    fn backend(&self) -> &dyn ExecutionBackend {
        match self {
            Self::Linux(e) => e,
            #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
            Self::Mac(e) => e,
        }
    }
}
impl ExecutionBackend for Executor {
    fn profile(&self) -> ExecutionProfile {
        self.backend().profile()
    }
    fn performance_parameters(&self) -> BTreeMap<String, String> {
        self.backend().performance_parameters()
    }
    fn preflight(&self, out: Option<&Path>) -> Result<()> {
        self.backend().preflight(out)
    }
    fn preflight_workers(&self, n: u32) -> Result<()> {
        self.backend().preflight_workers(n)
    }
    fn validate_retained(&self, inputs: &PreparedInputs) -> Result<()> {
        self.backend().validate_retained(inputs)
    }
    fn runtime_inputs<'a>(
        &self,
        inputs: &'a PreparedInputs,
    ) -> Vec<&'a seedbox_application::store::ContentDigest> {
        self.backend().runtime_inputs(inputs)
    }
    fn execute(&self, request: ExecutionRequest<'_>) -> Result<ExecutionReport> {
        eprintln!(
            "running: starting {} machines through {}",
            request.prepared.guests.len(),
            if cfg!(target_os = "macos") {
                "HVF"
            } else {
                "KVM"
            }
        );
        self.backend().execute(request)
    }
}

pub struct Preparer {
    pub inputs: OciPreparer,
    // Package extraction must outlive preparation and retention of its bytes.
    pub _helpers: Option<seedbox_application::guest_assets::Runtime>,
    pub cancellation: CancellationToken,
}
impl Deref for Preparer {
    type Target = OciPreparer;
    fn deref(&self) -> &OciPreparer {
        &self.inputs
    }
}
impl DerefMut for Preparer {
    fn deref_mut(&mut self) -> &mut OciPreparer {
        &mut self.inputs
    }
}
impl PreparationBackend for Preparer {
    fn validate_runtime_inputs(&self) -> Result<()> {
        self.inputs.validate_runtime_inputs()
    }
    fn prepare(&self, simulation: &Simulation, store: &ContentStore) -> Result<PreparedInputs> {
        let directory = tempfile::tempdir()?;
        let context = PreparationContext::new(
            directory.path().join("preparation"),
            self.cancellation.clone(),
        )?;
        self.prepare_cancellable(simulation, store, &context)
    }
    fn prepare_cancellable(
        &self,
        simulation: &Simulation,
        store: &ContentStore,
        context: &PreparationContext,
    ) -> Result<PreparedInputs> {
        self.validate_runtime_inputs()?;
        self.prepare_with_transport(
            simulation,
            store,
            context,
            &seedbox_application::container_images::ContainerTransport {
                docker: self.inputs.docker.clone(),
                cancellation: context.cancellation.clone(),
                progress: preparation_progress,
                export_cache: Some(self.inputs.profile.engine.clone()),
            },
        )
    }
}

/// Complete builds keep the kernel beside the executable. Explicit asset
/// directories retain the architecture subdirectory layout.
pub fn asset_directory() -> std::path::PathBuf {
    assets_beside(
        std::env::var_os("SEEDBOX_ASSET_DIR"),
        std::env::current_exe().ok(),
    )
}
fn assets_beside(
    root: Option<std::ffi::OsString>,
    executable: Option<std::path::PathBuf>,
) -> std::path::PathBuf {
    if let Some(root) = root {
        return std::path::PathBuf::from(root).join(std::env::consts::ARCH);
    }
    executable
        .and_then(|path| path.parent().map(Path::to_path_buf))
        .unwrap_or_default()
}
pub fn guest_assets(
    explicit: Option<std::path::PathBuf>,
) -> Result<seedbox_application::guest_assets::Runtime> {
    if explicit.is_none() {
        if let Some(bytes) = seedbox_application::guest_assets::EMBEDDED {
            return seedbox_application::guest_assets::Runtime::from_bytes(bytes);
        }
    }
    let path = explicit.unwrap_or_else(|| asset_directory().join("runtime.tar.gz"));
    let path = local_asset("--runtime-assets", path)?;
    seedbox_application::guest_assets::Runtime::from_archive(&path).map_err(|error| seedbox_application::Error::Setup(format!("runtime assets {}: {error}; rebuild with `python3 tools/build.py` or select a valid --runtime-assets archive", path.display())))
}
fn local_asset(flag: &str, path: std::path::PathBuf) -> Result<std::path::PathBuf> {
    if path.is_file() {
        Ok(path)
    } else {
        Err(seedbox_application::Error::Setup(format!("{flag} {}: not found; from the Seedbox checkout run `python3 tools/build.py`, or pass {flag} PATH", path.display())))
    }
}
pub fn guest_kernel(explicit: Option<std::path::PathBuf>) -> Result<std::path::PathBuf> {
    if let Some(path) = explicit {
        return super::defaults::existing("--kernel", path);
    }
    local_asset("--kernel", asset_directory().join("kernel"))
}

pub fn preparation_progress(message: &str) {
    eprintln!("preparing: {message}");
}

/// Resolve the user-owned engine at the CLI boundary; library code reads no environment.
pub fn container_engine() -> Result<bollard::Docker> {
    let connect = || -> std::result::Result<bollard::Docker, bollard::errors::Error> {
        if std::env::var_os("DOCKER_HOST").is_some() {
            return bollard::Docker::connect_with_defaults();
        }
        #[cfg(target_os = "macos")]
        {
            use std::os::unix::fs::FileTypeExt;
            let mut sockets = vec![std::path::PathBuf::from("/var/run/docker.sock")];
            if let Some(home) = std::env::var_os("HOME") {
                sockets.push(std::path::PathBuf::from(home).join(".docker/run/docker.sock"));
            }
            for socket in sockets {
                if std::fs::metadata(&socket).is_ok_and(|m| m.file_type().is_socket()) {
                    return bollard::Docker::connect_with_socket(
                        &socket.to_string_lossy(),
                        120,
                        bollard::API_DEFAULT_VERSION,
                    );
                }
            }
            // Use macOS's user temporary directory even inside a Nix shell,
            // which gives TMPDIR a different, per-shell value.
            let mut buffer = vec![0u8; 4096];
            // SAFETY: confstr writes at most `buffer.len()` bytes into `buffer`.
            let length = unsafe {
                libc::confstr(
                    libc::_CS_DARWIN_USER_TEMP_DIR,
                    buffer.as_mut_ptr().cast(),
                    buffer.len(),
                )
            };
            if length > 0 && length <= buffer.len() {
                buffer.truncate(length - 1);
                use std::os::unix::ffi::OsStringExt;
                let path =
                    std::path::PathBuf::from(std::ffi::OsString::from_vec(buffer)).join("podman");
                let mut sockets: Vec<_> = std::fs::read_dir(path)
                    .into_iter()
                    .flatten()
                    .filter_map(|entry| entry.ok())
                    .map(|entry| entry.path())
                    .filter(|path| {
                        path.to_string_lossy().ends_with("-api.sock")
                            && std::fs::metadata(path).is_ok_and(|m| m.file_type().is_socket())
                    })
                    .collect();
                sockets.sort();
                if sockets.len() == 1 {
                    return bollard::Docker::connect_with_socket(
                        &sockets[0].to_string_lossy(),
                        120,
                        bollard::API_DEFAULT_VERSION,
                    );
                }
            }
        }
        bollard::Docker::connect_with_podman_defaults()
    };
    connect().map_err(|error| seedbox_application::Error::Setup(format!(
        "cannot configure a Docker-compatible engine: {error}; start Docker or Podman and set DOCKER_HOST to its socket")))
}

#[cfg(test)]
mod tests {
    use super::assets_beside;
    use std::path::Path;

    #[test]
    fn bundled_kernel_follows_executable_and_explicit_assets_override_it() {
        assert_eq!(
            assets_beside(None, Some("/build/complete/seedbox".into())),
            Path::new("/build/complete")
        );
        assert_eq!(
            assets_beside(Some("/custom".into()), Some("/build/seedbox".into())),
            Path::new("/custom").join(std::env::consts::ARCH)
        );
    }
}
