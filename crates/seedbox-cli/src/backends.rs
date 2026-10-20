//! Platform selection stays at the command-line boundary.
use seedbox_application::{
    config::Simulation,
    execution::{
        CancellationToken, ExecutionBackend, ExecutionProfile, ExecutionReport, ExecutionRequest,
        PreparationBackend, PreparedInputs,
    },
    preparation::{OciPreparer, PreparationContext},
    store::ContentStore,
    Result,
};
use std::{
    collections::BTreeMap,
    ops::{Deref, DerefMut},
    path::Path,
};

pub struct Executor(pub Box<seedbox_driver_execution::driver_world::DriverExecutor>);
impl Executor {
    fn backend(&self) -> &dyn ExecutionBackend {
        self.0.as_ref()
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
            execution_backend_label(&self.backend().profile())?
        );
        self.backend().execute(request)
    }
}

fn execution_backend_label(
    profile: &seedbox_application::execution::ExecutionProfile,
) -> Result<&'static str> {
    Ok(
        if profile
            .driver_backend()
            .map_err(seedbox_application::Error::Setup)?
            == Some(seedbox_spec::backend::CpuBackend::Tcg)
        {
            "TCG"
        } else if cfg!(target_os = "macos") {
            "HVF"
        } else {
            "KVM"
        },
    )
}

pub struct Preparer {
    pub inputs: OciPreparer,
    pub cancellation: CancellationToken,
    pub driver_inputs: seedbox_driver_execution::runtime_assets::Inputs,
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
        seedbox_application::preparation::validate_oci_runtime(&self.inputs, &self.driver_inputs)
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
            export_cache: Some(store.put_file(&self.inputs.engine)?),
        };
        let mut prepared = self.inputs.prepare_driver_with_transport(
            simulation,
            store,
            context,
            &transport,
            &self.driver_inputs,
        )?;
        seedbox_driver_execution::driver_world::preparation::retain_oci_prepared(
            &mut prepared,
            simulation,
            &self.driver_inputs,
            store,
            &|| context.check(),
        )?;
        Ok(prepared)
    }
}

/// Complete Driver-package lookup beside the executable. Explicit asset
/// directories retain the host-architecture subdirectory layout.
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
/// Resolve a complete Driver runtime package at the CLI boundary. Explicit
/// archive wins, then SEEDBOX_ASSET_DIR, then embedded bytes, then the package
/// beside the executable. The asset directory uses its host-architecture
/// subdirectory; the package selects the guest ISA independently. The application
/// library performs no environment lookup and no runtime build.
pub fn driver_runtime(
    explicit: Option<std::path::PathBuf>,
    scratch: &Path,
) -> Result<seedbox_driver_execution::runtime_assets::Runtime> {
    if explicit.is_some() {
        // Retained acquisition passes an exact archive. It must not consult
        // startup/default locations, even while choosing resolver arguments.
        return resolve_driver_runtime(explicit, None, scratch, scratch);
    }
    resolve_driver_runtime(
        None,
        std::env::var_os("SEEDBOX_ASSET_DIR")
            .is_none()
            .then_some(seedbox_driver_execution::runtime_assets::EMBEDDED)
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
) -> Result<seedbox_driver_execution::runtime_assets::Runtime> {
    match (explicit, embedded) {
        (Some(path), _) => {
            seedbox_driver_execution::runtime_assets::Runtime::from_archive(&path, scratch)
        }
        (None, Some(bytes)) => {
            seedbox_driver_execution::runtime_assets::Runtime::from_bytes(bytes, scratch)
        }
        (None, None) => seedbox_driver_execution::runtime_assets::Runtime::from_archive(
            &directory.join("driver-runtime.tar.gz"),
            scratch,
        ),
    }
}

/// Match the default service's Driver PV inputs. Keep their private extraction
/// alive until the caller finishes probing. No legacy bootstrap lookup exists.
pub fn doctor_kernel(
    scratch: &Path,
) -> Result<(
    std::path::PathBuf,
    seedbox_driver_execution::runtime_assets::Inputs,
)> {
    let architecture = seedbox_driver_execution::driver_world::platform::Host::current()
        .guest(seedbox_driver_execution::driver_world::DriverBackend::Pv)
        .ok_or_else(|| {
            seedbox_application::Error::Setup(
                "Driver readiness requires Linux x86-64 or macOS aarch64".into(),
            )
        })?;
    let inputs = driver_runtime(None, scratch)?.select(
        seedbox_driver_execution::runtime_assets::Backend::Pv,
        architecture,
    )?;
    Ok((inputs.guest_assets.join("kernel"), inputs))
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
    use super::{assets_beside, execution_backend_label};
    use seedbox_test::process::Guarded;
    use std::path::Path;

    #[test]
    fn backend_progress_names_tcg_from_profile_not_host() {
        use seedbox_application::execution::{ExecutionProfile, SemanticParameters, WitnessMode};
        use seedbox_driver_execution::driver_world::{DriverBackend, DriverExecutor};
        for (cpu, tag) in [
            (DriverBackend::Pv, "driver-pv"),
            (DriverBackend::Tcg, "driver-tcg"),
        ] {
            let mut semantic_parameters = SemanticParameters::new();
            semantic_parameters.engine = Some(DriverExecutor::descriptor("x86_64"));
            let mut profile = ExecutionProfile {
                backend: tag.into(),
                witness_mode: WitnessMode::Exits,
                semantic_parameters,
            };
            let expected = if cpu == DriverBackend::Tcg {
                "TCG"
            } else if cfg!(target_os = "macos") {
                "HVF"
            } else {
                "KVM"
            };
            assert_eq!(execution_backend_label(&profile).unwrap(), expected);
            profile.backend = "driver-unknown".into();
            assert!(execution_backend_label(&profile)
                .unwrap_err()
                .to_string()
                .contains("unknown Driver backend"));
        }
    }

    #[test]
    fn default_driver_selection_depends_only_on_the_supported_platform() {
        let supported =
            seedbox_driver_execution::driver_world::platform::Host::current().runs_driver();
        for full_state in [false, true] {
            assert_eq!(crate::RuntimeInputs { full_state }.use_driver(), supported);
        }
    }

    #[test]
    fn tcg_refusal_names_the_tcg_hosts_from_the_host_mapping() {
        // Reached only where Host::guest(Tcg) is None (see platform tests).
        assert_eq!(
            crate::tcg_refusal().to_string(),
            "--backend tcg requires Linux x86-64 or macOS on Apple Silicon, and the normal Driver runtime"
        );
    }

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
            .guarded_status()
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
