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
    pub fn with_unqualified_cpu(self, allow: bool) -> Self {
        match self {
            Self::Linux(e) => Self::Linux(e.with_unqualified_cpu(allow)),
            #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
            Self::Mac(e) => Self::Mac(e),
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

pub fn container_runtime(kernel_only: bool) -> Result<seedbox::container_runtime::Runtime> {
    let home = std::env::var_os("HOME").ok_or_else(|| {
        seedbox::Error::Setup("HOME is unset; set it to your home directory".into())
    })?;
    let cache = std::env::var_os("XDG_CACHE_HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            std::path::PathBuf::from(home).join(if cfg!(target_os = "macos") {
                "Library/Caches"
            } else {
                ".cache"
            })
        });
    seedbox::container_runtime::Runtime::embedded(&cache.join("seedbox/runtime"), kernel_only)
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

pub fn container_runtime_available() -> bool {
    seedbox::container_runtime::Runtime::available()
}
