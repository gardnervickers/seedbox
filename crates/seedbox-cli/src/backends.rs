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
    Linux(Box<VmmExecutor>),
    Driver(Box<seedbox_application::driver_world::DriverExecutor>),
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    Mac(Box<seedbox_application::mac::MacExecutor>),
}
impl Executor {
    fn backend(&self) -> &dyn ExecutionBackend {
        match self {
            Self::Linux(e) => e.as_ref(),
            Self::Driver(e) => e.as_ref(),
            #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
            Self::Mac(e) => e.as_ref(),
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
    fn preflight_resume(
        &self,
        checkpoint: Option<&Path>,
        control: Option<&seedbox_application::config::Control>,
    ) -> Result<()> {
        self.backend().preflight_resume(checkpoint, control)
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
    pub driver_inputs: Option<seedbox_application::runtime_assets::Inputs>,
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
        match &self.driver_inputs {
            Some(runtime) => {
                seedbox_application::driver_world::validate_oci_runtime(&self.inputs, runtime)
            }
            None => self.inputs.validate_runtime_inputs(),
        }
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
        let transport = seedbox_application::container_images::ContainerTransport {
            docker: self.inputs.docker.clone(),
            cancellation: context.cancellation.clone(),
            progress: preparation_progress,
            export_cache: Some(self.inputs.profile.engine.clone()),
        };
        let mut prepared = match &self.driver_inputs {
            Some(runtime) => self
                .inputs
                .prepare_driver_with_transport(simulation, store, context, &transport, runtime)?,
            None => self
                .inputs
                .prepare_with_transport(simulation, store, context, &transport)?,
        };
        if let Some(runtime) = &self.driver_inputs {
            seedbox_application::driver_world::retain_oci_prepared(
                &mut prepared,
                simulation,
                runtime,
                store,
                context,
            )?;
        }
        Ok(prepared)
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
    match super::defaults::helper_archive(
        explicit,
        seedbox_application::guest_assets::EMBEDDED,
        asset_directory().join("runtime.tar.gz"),
    )? {
        super::defaults::HelperArchive::Embedded(bytes) => {
            seedbox_application::guest_assets::Runtime::from_bytes(bytes)
        }
        super::defaults::HelperArchive::File(path) => {
            seedbox_application::guest_assets::Runtime::from_archive(&path).map_err(|error| seedbox_application::Error::Setup(format!("runtime assets {}: {error}; rebuild with `cargo xtask build-assets` or select a valid --runtime-assets archive", path.display())))
        }
    }
}
pub fn guest_kernel(explicit: Option<std::path::PathBuf>) -> Result<std::path::PathBuf> {
    if let Some(path) = explicit {
        return super::defaults::existing("--kernel", path);
    }
    if std::env::var_os("SEEDBOX_ASSET_DIR").is_none() {
        if let Some(bytes) = seedbox_application::guest_assets::EMBEDDED_KERNEL {
            let cache = std::env::var_os("XDG_CACHE_HOME")
                .map(std::path::PathBuf::from)
                .or_else(|| {
                    std::env::var_os("HOME")
                        .map(|home| std::path::PathBuf::from(home).join(".cache"))
                })
                .ok_or_else(|| {
                    seedbox_application::Error::Setup(
                        "kernel cache unavailable; set XDG_CACHE_HOME or HOME".into(),
                    )
                })?;
            return cached_kernel(bytes, &cache.join("seedbox/kernels"));
        }
    }
    super::defaults::local_asset("--kernel", asset_directory().join("kernel"))
}

fn cached_kernel(bytes: &[u8], root: &Path) -> Result<std::path::PathBuf> {
    use std::io::Write;
    let path = root.join(seedbox_application::store::ContentDigest::of(bytes).hex());
    let extract = || -> std::io::Result<()> {
        if std::fs::read(&path).is_ok_and(|cached| cached == bytes) {
            return Ok(());
        }
        std::fs::create_dir_all(root)?;
        let mut temporary = tempfile::NamedTempFile::new_in(root)?;
        temporary.write_all(bytes)?;
        temporary.persist(&path).map_err(|error| error.error)?;
        Ok(())
    };
    extract().map_err(|error| {
        seedbox_application::Error::Setup(format!("kernel cache {}: {error}", root.display()))
    })?;
    Ok(path)
}

/// Resolve a complete Driver runtime package at the CLI boundary. Explicit
/// archive wins, then SEEDBOX_ASSET_DIR, then embedded bytes, then the package
/// beside the executable. The asset directory uses its host-architecture
/// subdirectory; the package selects the guest ISA independently. The application
/// library performs no environment lookup and no runtime build.
pub fn driver_runtime(
    explicit: Option<std::path::PathBuf>,
    scratch: &Path,
) -> Result<seedbox_application::runtime_assets::Runtime> {
    if explicit.is_some() {
        // Retained acquisition passes an exact archive. It must not consult
        // startup/default locations, even while choosing resolver arguments.
        return resolve_driver_runtime(explicit, None, scratch, scratch);
    }
    resolve_driver_runtime(
        None,
        std::env::var_os("SEEDBOX_ASSET_DIR")
            .is_none()
            .then_some(seedbox_application::runtime_assets::EMBEDDED)
            .flatten(),
        &asset_directory(),
        scratch,
    )
}
fn resolve_driver_runtime(
    explicit: Option<std::path::PathBuf>,
    embedded: Option<&[u8]>,
    directory: &Path,
    scratch: &Path,
) -> Result<seedbox_application::runtime_assets::Runtime> {
    match (explicit, embedded) {
        (Some(path), _) => {
            seedbox_application::runtime_assets::Runtime::from_archive(&path, scratch)
        }
        (None, Some(bytes)) => {
            seedbox_application::runtime_assets::Runtime::from_bytes(bytes, scratch)
        }
        (None, None) => seedbox_application::runtime_assets::Runtime::from_archive(
            &directory.join("driver-runtime.tar.gz"),
            scratch,
        ),
    }
}

/// Match the default service's Driver PV inputs. Keep their private extraction
/// alive until the caller finishes probing. Explicit legacy overrides retain
/// their existing lookup and error behavior.
pub fn doctor_kernel(
    explicit: Option<std::path::PathBuf>,
    internal: bool,
    scratch: &Path,
) -> Result<(
    std::path::PathBuf,
    Option<seedbox_application::runtime_assets::Inputs>,
)> {
    if explicit.is_some() || !internal {
        return guest_kernel(explicit).map(|path| (path, None));
    }
    use seedbox_spec::definition::Architecture;
    let architecture = match std::env::consts::ARCH {
        "x86_64" => Architecture::X86_64,
        "aarch64" => Architecture::Aarch64,
        architecture => {
            return Err(seedbox_application::Error::Setup(format!(
                "Driver PV does not support {architecture} guests"
            )))
        }
    };
    let inputs = driver_runtime(None, scratch)?.select(
        seedbox_application::runtime_assets::Backend::Pv,
        architecture,
    )?;
    Ok((inputs.guest_assets.join("kernel"), Some(inputs)))
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
#[allow(
    clippy::disallowed_methods,
    reason = "test code: spawns here never race a Seedbox process writing an executable"
)]
mod tests {
    use super::{assets_beside, cached_kernel};

    #[test]
    fn default_driver_selection_preserves_explicit_legacy_inputs() {
        fn defaults() -> crate::RuntimeInputs {
            crate::RuntimeInputs {
                kvm: crate::KvmInputs {
                    kernel: None,
                    vmm: None,
                },
                runtime_assets: None,
                guest_init: None,
                mke2fs: None,
                full_state: false,
            }
        }
        let supported = cfg!(all(target_os = "linux", target_arch = "x86_64"));
        assert_eq!(defaults().use_driver(false), supported);
        assert!(!defaults().use_driver(true));
        let mut full = defaults();
        full.full_state = true;
        assert_eq!(full.use_driver(false), supported);
        for option in 0..5 {
            let mut inputs = defaults();
            let destination = match option {
                0 => &mut inputs.kvm.vmm,
                1 => &mut inputs.kvm.kernel,
                2 => &mut inputs.runtime_assets,
                3 => &mut inputs.guest_init,
                _ => &mut inputs.mke2fs,
            };
            *destination = Some("/explicit-legacy-input".into());
            assert!(!inputs.use_driver(false));
        }
    }

    #[test]
    fn embedded_kernel_cache_reuses_bytes_and_separates_versions() {
        let directory = tempfile::tempdir().unwrap();
        let first = cached_kernel(b"kernel one", directory.path()).unwrap();
        assert_eq!(std::fs::read(&first).unwrap(), b"kernel one");
        assert_eq!(
            cached_kernel(b"kernel one", directory.path()).unwrap(),
            first
        );
        let second = cached_kernel(b"kernel two", directory.path()).unwrap();
        assert_ne!(first, second);
        assert_eq!(std::fs::read(second).unwrap(), b"kernel two");
    }

    use std::path::Path;

    #[test]
    fn driver_package_resolution_prefers_explicit_then_embedded_then_local_archive() {
        let _scope = crate::daemon::session_test_scope();
        let directory = tempfile::tempdir().unwrap();
        let host = format!(
            "{}-{}",
            std::env::consts::ARCH,
            if cfg!(target_os = "macos") {
                "darwin"
            } else {
                "linux"
            }
        );
        std::fs::write(directory.path().join("host-system"), host).unwrap();
        let archive = directory.path().join("driver-runtime.tar.gz");
        assert!(std::process::Command::new("tar")
            .arg("-czf")
            .arg(&archive)
            .arg("-C")
            .arg(directory.path())
            .arg("host-system")
            .status()
            .unwrap()
            .success());
        let bytes = std::fs::read(&archive).unwrap();
        let absent = directory.path().join("explicit-missing.tar.gz");
        let error = super::resolve_driver_runtime(
            Some(absent.clone()),
            Some(&bytes),
            directory.path(),
            directory.path(),
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains(&absent.display().to_string()));
        let embedded = super::resolve_driver_runtime(
            None,
            Some(&bytes),
            Path::new("/not-the-default-path"),
            directory.path(),
        )
        .unwrap();
        assert_eq!(std::fs::read(embedded.archive()).unwrap(), bytes);
        let local =
            super::resolve_driver_runtime(None, None, directory.path(), directory.path()).unwrap();
        assert_eq!(std::fs::read(local.archive()).unwrap(), bytes);
        let retained = super::driver_runtime(Some(archive.clone()), directory.path()).unwrap();
        assert_eq!(std::fs::read(retained.archive()).unwrap(), bytes);
        let explicit = super::resolve_driver_runtime(
            Some(archive),
            Some(b"invalid embedded"),
            Path::new("/absent"),
            directory.path(),
        )
        .unwrap();
        assert_eq!(std::fs::read(explicit.archive()).unwrap(), bytes);
    }

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
