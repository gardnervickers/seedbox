//! Platform selection stays at the command-line boundary.
use seedbox::{
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
    Mac(seedbox::mac::MacExecutor),
}
impl Executor {
    pub fn with_checkpoint(self, at_ns: u64, branch_seed: u64) -> Result<Self> {
        match self {
            Self::Linux(executor) => Ok(Self::Linux(executor.with_checkpoint(at_ns, branch_seed))),
            #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
            Self::Mac(_) => Err(seedbox::Error::Setup(super::CHECKPOINT_REQUIRES_KVM.into())),
        }
    }
    pub fn performance(self, margin: &str, undo: u64) -> Result<Self> {
        match self {
            Self::Linux(e) => Ok(Self::Linux(e.with_margin(margin)?.with_undo(undo)?)),
            #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
            Self::Mac(e) => Ok(Self::Mac(e.with_performance(margin, undo))),
        }
    }
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
    ) -> Vec<&'a seedbox::store::ContentDigest> {
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
    pub _helpers: Option<seedbox::guest_assets::Runtime>,
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
            &seedbox::container_images::ContainerTransport {
                docker: self.inputs.docker.clone(),
                cancellation: context.cancellation.clone(),
                progress: preparation_progress,
            },
        )
    }
}

/// Shared with tools/build-assets.sh. The OS may reclaim this directory.
pub fn asset_directory() -> std::path::PathBuf {
    std::env::var_os("SEEDBOX_ASSET_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            default_asset_root(unsafe { libc::geteuid() }, std::env::var_os("SUDO_UID"))
        })
        .join(std::env::consts::ARCH)
}
/// Assets are built as the user, so `sudo seedbox` reads the invoking user's slot.
fn default_asset_root(euid: u32, sudo_uid: Option<std::ffi::OsString>) -> std::path::PathBuf {
    let uid = sudo_uid
        .filter(|_| euid == 0)
        .and_then(|uid| uid.to_str()?.parse::<u32>().ok())
        .unwrap_or(euid);
    std::path::PathBuf::from(format!("/tmp/seedbox-{uid}"))
}
pub fn guest_assets(
    explicit: Option<std::path::PathBuf>,
) -> Result<seedbox::guest_assets::Runtime> {
    if explicit.is_none() {
        if let Some(bytes) = seedbox::guest_assets::EMBEDDED {
            return seedbox::guest_assets::Runtime::from_bytes(bytes);
        }
    }
    let path = explicit.unwrap_or_else(|| asset_directory().join("runtime.tar.gz"));
    let path = local_asset("--runtime-assets", path)?;
    seedbox::guest_assets::Runtime::from_archive(&path).map_err(|error| seedbox::Error::Setup(format!("runtime assets {}: {error}; rebuild with `bash tools/build-assets.sh` or select a valid --runtime-assets archive", path.display())))
}
fn local_asset(flag: &str, path: std::path::PathBuf) -> Result<std::path::PathBuf> {
    if path.is_file() {
        Ok(path)
    } else {
        Err(seedbox::Error::Setup(format!("{flag} {}: not found; from the Seedbox checkout run `bash tools/build-assets.sh`, or pass {flag} PATH", path.display())))
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
    connect().map_err(|error| seedbox::Error::Setup(format!(
        "cannot configure a Docker-compatible engine: {error}; start Docker or Podman and set DOCKER_HOST to its socket")))
}

#[cfg(test)]
mod tests {
    use super::default_asset_root;
    use std::path::Path;

    #[test]
    fn sudo_reads_the_invoking_users_asset_slot() {
        let root = |euid, sudo: Option<&str>| default_asset_root(euid, sudo.map(Into::into));
        assert_eq!(root(1000, None), Path::new("/tmp/seedbox-1000"));
        assert_eq!(root(0, None), Path::new("/tmp/seedbox-0"));
        assert_eq!(root(0, Some("1000")), Path::new("/tmp/seedbox-1000"));
        // Only root follows SUDO_UID; a malformed value keeps the effective user.
        assert_eq!(root(1001, Some("1000")), Path::new("/tmp/seedbox-1001"));
        assert_eq!(root(0, Some("bob")), Path::new("/tmp/seedbox-0"));
    }
}
