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
    pub fn with_boot_seed(self, seed: u64) -> Result<Self> {
        match self {
            Self::Linux(executor) => Ok(Self::Linux(executor.with_boot_seed(seed))),
            #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
            Self::Mac(_) => Err(seedbox::Error::Setup(
                "--fork requires Linux KVM; omit it on Mac".into(),
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
    fn execute_forked(&self, requests: Vec<ExecutionRequest<'_>>) -> Vec<Result<ExecutionReport>> {
        if let Some(request) = requests.first() {
            eprintln!(
                "running: starting {} machines through {} for {} seeds",
                request.prepared.guests.len(),
                if cfg!(target_os = "macos") {
                    "HVF"
                } else {
                    "KVM"
                },
                requests.len()
            );
        }
        self.backend().execute_forked(requests)
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
    let _hand_over = CacheHandOver::for_sudo_user();
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

/// Under sudo, the cache lives in the invoking user's home; leave nothing there
/// that only root can remove, whether the download or extraction succeeded,
/// failed or was cancelled. Best effort: a failure only affects cleanup.
struct CacheHandOver {
    home: std::path::PathBuf,
    cache: std::path::PathBuf,
    uid: u32,
    gid: u32,
}

impl CacheHandOver {
    fn for_sudo_user() -> Option<Self> {
        if unsafe { libc::geteuid() } != 0 {
            return None;
        }
        let ids = |name| std::env::var(name).ok()?.parse::<u32>().ok();
        Some(Self {
            uid: ids("SUDO_UID")?,
            gid: ids("SUDO_GID")?,
            home: super::defaults::sudo_home()?,
            cache: cache_home().ok()?,
        })
    }
}

impl Drop for CacheHandOver {
    fn drop(&mut self) {
        hand_over(&self.home, &self.cache, self.uid, self.gid);
    }
}

fn hand_over(home: &Path, cache: &Path, uid: u32, gid: u32) {
    use std::os::unix::fs::MetadataExt;
    let Ok(relative) = cache.strip_prefix(home) else {
        return;
    };
    // Directories root created between the home and the cache, then the cache.
    let mut path = home.to_path_buf();
    let mut paths = Vec::new();
    for component in relative.components() {
        path.push(component);
        paths.push(path.clone());
    }
    paths.extend(
        walkdir::WalkDir::new(cache)
            .min_depth(1)
            .follow_links(false)
            .into_iter()
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.into_path()),
    );
    for path in paths {
        if std::fs::symlink_metadata(&path).is_ok_and(|metadata| metadata.uid() == 0) {
            let _ = std::os::unix::fs::lchown(&path, Some(uid), Some(gid));
        }
    }
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
    let _hand_over = CacheHandOver::for_sudo_user();
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

#[cfg(test)]
mod sudo_cache_tests {
    use std::os::unix::fs::MetadataExt;

    #[test]
    fn root_created_cache_entries_go_to_the_sudo_user() {
        if unsafe { libc::geteuid() } != 0 {
            return; // Changing ownership needs root.
        }
        let home = tempfile::tempdir().unwrap();
        let cache = home.path().join(".cache/seedbox");
        std::fs::create_dir_all(cache.join("kernels/x86_64/ready")).unwrap();
        std::fs::write(cache.join("kernels/x86_64/ready/kernel"), b"k").unwrap();
        let outside = tempfile::tempdir().unwrap();
        super::hand_over(home.path(), &cache, 4242, 4343);
        for path in [
            home.path().join(".cache"),
            cache.clone(),
            cache.join("kernels/x86_64/ready/kernel"),
        ] {
            let metadata = std::fs::metadata(&path).unwrap();
            assert_eq!((metadata.uid(), metadata.gid()), (4242, 4343), "{path:?}");
        }
        assert_eq!(std::fs::metadata(home.path()).unwrap().uid(), 0);
        super::hand_over(home.path(), outside.path(), 4242, 4343);
        assert_eq!(std::fs::metadata(outside.path()).unwrap().uid(), 0);
    }

    #[test]
    fn a_failed_download_still_returns_the_cache() {
        if unsafe { libc::geteuid() } != 0 {
            return; // Changing ownership needs root.
        }
        let home = tempfile::tempdir().unwrap();
        let cache = home.path().join(".cache/seedbox");
        let failed_download = || -> seedbox::Result<()> {
            let _hand_over = super::CacheHandOver {
                home: home.path().to_path_buf(),
                cache: cache.clone(),
                uid: 4242,
                gid: 4343,
            };
            std::fs::create_dir_all(cache.join("assets/x86_64")).unwrap();
            std::fs::write(cache.join("assets/x86_64/.lock"), b"").unwrap();
            Err(seedbox::Error::Setup("HTTP 404".into()))
        };
        assert!(failed_download().is_err());
        for path in [
            home.path().join(".cache"),
            cache.clone(),
            cache.join("assets/x86_64"),
            cache.join("assets/x86_64/.lock"),
        ] {
            let metadata = std::fs::metadata(&path).unwrap();
            assert_eq!((metadata.uid(), metadata.gid()), (4242, 4343), "{path:?}");
        }
    }
}
