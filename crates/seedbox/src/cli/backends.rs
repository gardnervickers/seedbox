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
            Self::Mac(_) => Err(seedbox::Error::Setup(
                "--checkpoint-at-ns and --branch-seed require Linux KVM; omit them on Mac".into(),
            )),
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

pub enum Preparer {
    #[cfg_attr(all(target_os = "macos", target_arch = "aarch64"), allow(dead_code))]
    Linux(Box<LinuxPreparer>),
    Container(Box<seedbox::container_runtime::ContainerPreparer>),
}
pub struct LinuxPreparer {
    pub inputs: OciPreparer,
    // Package extraction must outlive preparation and retention of its bytes.
    pub _assets: Option<seedbox::runtime_assets::RuntimeAssets>,
}
impl Preparer {
    fn backend(&self) -> &dyn PreparationBackend {
        match self {
            Self::Linux(e) => &e.inputs,
            Self::Container(e) => e.as_ref(),
        }
    }
}
impl Deref for Preparer {
    type Target = OciPreparer;
    fn deref(&self) -> &OciPreparer {
        match self {
            Self::Linux(e) => &e.inputs,
            Self::Container(e) => &e.inputs,
        }
    }
}
impl DerefMut for Preparer {
    fn deref_mut(&mut self) -> &mut OciPreparer {
        match self {
            Self::Linux(e) => &mut e.inputs,
            Self::Container(e) => &mut e.inputs,
        }
    }
}
impl PreparationBackend for Preparer {
    fn validate_runtime_inputs(&self) -> Result<()> {
        self.backend().validate_runtime_inputs()
    }
    fn prepare(&self, simulation: &Simulation, store: &ContentStore) -> Result<PreparedInputs> {
        self.backend().prepare(simulation, store)
    }
    fn prepare_cancellable(
        &self,
        simulation: &Simulation,
        store: &ContentStore,
        context: &PreparationContext,
    ) -> Result<PreparedInputs> {
        self.backend()
            .prepare_cancellable(simulation, store, context)
    }
}

fn cache_home() -> Result<std::path::PathBuf> {
    let cache = match std::env::var_os("XDG_CACHE_HOME").filter(|s| !s.is_empty()) {
        Some(path) => std::path::PathBuf::from(path),
        None => {
            let home = std::env::var_os("HOME").ok_or_else(|| {
                seedbox::Error::Setup(
                    "HOME is unset; set it or set XDG_CACHE_HOME to a writable cache directory"
                        .into(),
                )
            })?;
            std::path::PathBuf::from(home).join(if cfg!(target_os = "macos") {
                "Library/Caches"
            } else {
                ".cache"
            })
        }
    };
    Ok(cache.join("seedbox"))
}

/// `explicit_init` skips the supervisor check: the archive's copy is unused.
pub fn container_runtime(
    explicit: Option<std::path::PathBuf>,
    explicit_init: bool,
    cancellation: &CancellationToken,
) -> Result<seedbox::container_runtime::Runtime> {
    use seedbox::release_download::ReleaseAsset;
    let explicit_assets = explicit.is_some();
    let path = match explicit {
        Some(path) => super::defaults::existing("--runtime-assets", path)?,
        None => release_asset(
            seedbox::release_download::ReleaseAsset::Runtime,
            "assets",
            cancellation,
            true,
        )?,
    };
    let runtime =
        seedbox::container_runtime::Runtime::from_archive(&path, &cache_home()?.join("runtime"))
            .map_err(|error| match error {
                seedbox::Error::Io(error) => seedbox::Error::Setup(format!(
                    "runtime assets {}: {error}; check archive and cache permissions",
                    path.display()
                )),
                other => other,
            })?;
    if !explicit_init && !speaks_launch_format(&runtime.input("compose-init"))? {
        let magic = String::from_utf8_lossy(seedbox::guest::LAUNCH_MAGIC);
        if explicit_assets {
            return Err(seedbox::Error::Setup(format!(
                "--runtime-assets {}: its guest supervisor predates {magic}; rebuild the assets from this checkout",
                path.display()
            )));
        }
        // The latest release predates this build. Drop it and its kernel so
        // the next run downloads again once newer assets are published.
        let architecture = std::env::consts::ARCH;
        for asset in [ReleaseAsset::Runtime, ReleaseAsset::Kernel] {
            let ready = release_cache(asset)?.join(architecture).join("ready");
            match std::fs::remove_dir_all(&ready) {
                Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
                    return Err(seedbox::Error::Setup(format!(
                        "remove stale cache {}: {error}; delete it and retry",
                        ready.display()
                    )))
                }
                _ => (),
            }
        }
        return Err(seedbox::Error::Setup(format!(
            "the latest published guest assets predate {magic}; wait for the Guest assets workflow on main, or pass --runtime-assets, --guest-init and --kernel built from this checkout"
        )));
    }
    Ok(runtime)
}

fn speaks_launch_format(init: &Path) -> Result<bool> {
    let bytes = std::fs::read(init)?;
    let magic = seedbox::guest::LAUNCH_MAGIC;
    Ok(bytes.windows(magic.len()).any(|window| window == magic))
}

/// Caches are keyed by the boot recipe format, so a build that writes a new
/// format never reuses guest assets downloaded for an older one.
fn release_cache(asset: seedbox::release_download::ReleaseAsset) -> Result<std::path::PathBuf> {
    use seedbox::release_download::ReleaseAsset;
    Ok(cache_home()?
        .join(match asset {
            ReleaseAsset::Kernel => "kernels",
            ReleaseAsset::Runtime => "assets",
        })
        .join(String::from_utf8_lossy(seedbox::guest::LAUNCH_MAGIC).to_lowercase()))
}

pub fn guest_kernel(
    explicit: Option<std::path::PathBuf>,
    cancellation: &CancellationToken,
    download: bool,
) -> Result<std::path::PathBuf> {
    if let Some(path) = explicit {
        return super::defaults::existing("--kernel", path);
    }
    release_asset(
        seedbox::release_download::ReleaseAsset::Kernel,
        "kernel",
        cancellation,
        download,
    )
}

fn release_asset(
    asset: seedbox::release_download::ReleaseAsset,
    prefix: &str,
    cancellation: &CancellationToken,
    download: bool,
) -> Result<std::path::PathBuf> {
    use seedbox::release_download::ReleaseDownload;
    let cache = release_cache(asset)?;
    let architecture = std::env::consts::ARCH;
    if !download {
        return ReleaseDownload::cached(asset, &cache, architecture);
    }
    if let Some(path) = ReleaseDownload::lookup(asset, &cache, architecture)? {
        eprintln!(
            "{prefix}: using cached {} {}",
            asset.description(),
            path.display()
        );
        return Ok(path);
    }
    #[cfg(target_os = "linux")]
    seedbox::admission::check_kvm_access()?;
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    seedbox::mac::check_host_access()?;
    let token = std::env::var("GH_TOKEN")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| std::env::var("GITHUB_TOKEN").ok().filter(|s| !s.is_empty()));
    // All process-global HTTP configuration stays at this CLI boundary.
    let agent = ureq::Agent::config_builder()
        .https_only(true)
        .user_agent(concat!("seedbox/", env!("CARGO_PKG_VERSION")))
        .timeout_global(Some(std::time::Duration::from_secs(600)))
        .timeout_connect(Some(std::time::Duration::from_secs(10)))
        .timeout_recv_response(Some(std::time::Duration::from_secs(15)))
        .build()
        .new_agent();
    ReleaseDownload::new(asset, agent, token.as_deref())?.ensure(
        &cache,
        architecture,
        cancellation,
        |message| eprintln!("{prefix}: {message}"),
    )
}

pub fn container_client(
    requested: &Path,
    cancellation: &CancellationToken,
) -> Result<std::path::PathBuf> {
    if requested != Path::new("auto") {
        return super::defaults::preparation_tool(requested);
    }
    let mut found = Vec::new();
    for name in ["docker", "podman"] {
        if let Ok(client) = super::defaults::preparation_tool(Path::new(name)) {
            match seedbox::container_runtime::ContainerPreparer::check_client(
                &client,
                cancellation.clone(),
            ) {
                Ok(()) => return Ok(client),
                Err(seedbox::Error::PreparationCancelled) => {
                    return Err(seedbox::Error::PreparationCancelled)
                }
                Err(_) => (),
            }
            found.push(name);
        }
    }
    Err(seedbox::Error::Setup(if found.is_empty() {
        "no Docker/Podman client found; install Docker or Podman (or enter nix develop), then start its Linux engine".into()
    } else {
        format!("container engine unavailable; start Docker Desktop or your Podman machine (Linux: start its service), and check DOCKER_HOST or CONTAINER_HOST; tried {}", found.join(", "))
    }))
}

pub fn preparation_progress(message: &str) {
    eprintln!("preparing: {}", message.replace('-', " "));
}

pub fn released_assets() -> bool {
    env!("SEEDBOX_RELEASE_ASSETS") == "1"
}

#[cfg(test)]
mod launch_format_tests {
    #[test]
    fn supervisor_must_carry_the_current_launch_magic() {
        let directory = tempfile::tempdir().unwrap();
        let init = directory.path().join("compose-init");
        std::fs::write(&init, b"\x7fELF..SBXLAUNCH3..").unwrap();
        assert!(!super::speaks_launch_format(&init).unwrap());
        let mut current = b"\x7fELF..".to_vec();
        current.extend_from_slice(seedbox::guest::LAUNCH_MAGIC);
        std::fs::write(&init, current).unwrap();
        assert!(super::speaks_launch_format(&init).unwrap());
    }
}
