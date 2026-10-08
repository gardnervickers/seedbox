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
        self.backend().execute(request)
    }
}

pub enum Preparer {
    #[cfg_attr(all(target_os = "macos", target_arch = "aarch64"), allow(dead_code))]
    Linux(Box<LinuxPreparer>),
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    Mac(Box<seedbox::mac_runtime::MacPreparer>),
}
pub struct LinuxPreparer {
    pub inputs: OciPreparer,
    // Package extraction must outlive preparation and retention of its bytes.
    pub _assets: Option<seedbox::runtime_assets::RuntimeAssets>,
}
impl Preparer {
    pub fn cancellation(&mut self, token: CancellationToken) {
        #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
        if let Self::Mac(preparer) = self {
            preparer.cancellation = token;
        }
        #[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
        let _ = token;
    }
    fn backend(&self) -> &dyn PreparationBackend {
        match self {
            Self::Linux(e) => &e.inputs,
            #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
            Self::Mac(e) => e.as_ref(),
        }
    }
}
impl Deref for Preparer {
    type Target = OciPreparer;
    fn deref(&self) -> &OciPreparer {
        match self {
            Self::Linux(e) => &e.inputs,
            #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
            Self::Mac(e) => &e.inputs,
        }
    }
}
impl DerefMut for Preparer {
    fn deref_mut(&mut self) -> &mut OciPreparer {
        match self {
            Self::Linux(e) => &mut e.inputs,
            #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
            Self::Mac(e) => &mut e.inputs,
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

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
pub fn mac_runtime() -> Result<seedbox::mac_runtime::Runtime> {
    let home = std::env::var_os("HOME").ok_or_else(|| {
        seedbox::Error::Setup("HOME is unset; set it to your home directory".into())
    })?;
    let cache = std::env::var_os("XDG_CACHE_HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from(home).join("Library/Caches"));
    let lima = super::defaults::preparation_tool(Path::new("limactl"))?;
    // Lima's UNIX sockets need a short path. The runtime enforces private
    // ownership and a Seedbox marker before ever starting or stopping a VM.
    let vm_home =
        std::path::PathBuf::from(format!("/tmp/seedbox-prep-{}", unsafe { libc::geteuid() }));
    seedbox::mac_runtime::Runtime::embedded(&cache.join("seedbox/mac-runtime"), lima, vm_home)
}
