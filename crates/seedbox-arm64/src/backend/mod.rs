//! Host APIs stop at this boundary. Registers are architectural encodings.
use crate::{memory::Memory, Result};
use serde::{Deserialize, Serialize};
use std::rc::Rc;

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
pub mod hvf;
#[cfg(all(target_os = "linux", target_arch = "aarch64"))]
pub mod kvm;

#[derive(Clone, Copy, Debug)]
pub enum CoreReg {
    X(u8),
    Pc,
    Pstate,
    /// EL1 stack pointer (SP_EL1), independent of the current PSTATE.SP.
    Sp,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SysReg(pub u16);
impl SysReg {
    /// ARM ARM: S2_0_C1_C3_4, OS Double Lock Register.
    pub const OSDLR: Self = Self(0x809c);
    /// ARM ARM: S2_0_C1_C0_4, OS Lock Access Register.
    pub const OSLAR: Self = Self(0x8084);
    /// AArch64 Memory Model Feature Register 0 (S3_0_C0_C7_0).
    pub const ID_AA64MMFR0: Self = Self(0xc038);
    pub const ID_AA64DFR0: Self = Self(0xc028);
    pub const SCTLR: Self = Self(0xc080);
    pub const MPIDR: Self = Self(0xc005);
    pub const CNTKCTL: Self = Self(0xc708);
    pub const CNTV_CTL: Self = Self(0xdf19);
    /// Read-only EL0 thread ID register (S3_3_C13_C0_3).
    pub const TPIDRRO_EL0: Self = Self(0xde83);
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Exit {
    Mmio {
        addr: u64,
        len: u8,
        write: bool,
        data: u64,
        reg: Option<u8>,
    },
    Hvc {
        imm: u16,
    },
    Wfi,
    /// A trapped WFE (or WFET). The guest gains nothing from waiting: no other
    /// CPU exists and interrupts arrive at deterministic rendezvous points, so
    /// the VMM skips the instruction without idling.
    Wfe,
    SysReg {
        reg: u16,
        write: bool,
        rt: u8,
        data: u64,
    },
    Other(u64),
}
#[derive(Clone, Debug)]
pub struct Stop {
    /// Address of the trapping instruction, even when the API reports PC+4.
    pub pc: u64,
    pub exit: Exit,
}
/// A host interruption is not a guest exit: it owns no held instruction and
/// must bypass clock reconciliation, IRQ delivery, semantic budgets and witness.
/// The cause is deliberately unspecified until the backend can authenticate it.
#[derive(Clone, Debug)]
pub enum RunStop {
    Guest(Stop),
    Interrupted,
}
impl From<Stop> for RunStop {
    fn from(stop: Stop) -> Self {
        Self::Guest(stop)
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum DirtyPages {
    Unsupported,
    Bitmap { page_size: usize, words: Vec<u64> },
}

/// Explicit stopped architectural inventory, with no host handles or raw exit
/// structures. GIC bytes use the backend's documented stable state format.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackendState {
    pub core: Vec<u64>,
    pub system: std::collections::BTreeMap<u16, u64>,
    pub simd: Vec<[u8; 16]>,
    pub gic_cpu: std::collections::BTreeMap<u16, u64>,
    pub gic: Vec<u8>,
}

/// One VM and one vCPU, confined to the creating thread. `run` is synchronous;
/// every other operation is performed while stopped. Backends retain mappings
/// until unmap/destruction. `complete` retires the held operation exactly once;
/// on KVM the kernel finishes MMIO on the next entry, before executing guest code.
/// Neither host interruptions nor backend maintenance may advance guest time.
pub trait Backend: Sized {
    /// Report metadata only; excluded from the canonical witness.
    const NAME: &'static str = "mock";
    fn create() -> Result<Self>;
    fn map_memory(&mut self, slot: u32, ipa: u64, memory: Rc<Memory>) -> Result<()>;
    fn unmap_memory(&mut self, slot: u32) -> Result<()>;
    fn create_vcpu(&mut self) -> Result<()>;
    fn get_reg(&self, reg: CoreReg) -> Result<u64>;
    fn set_reg(&mut self, reg: CoreReg, value: u64) -> Result<()>;
    fn get_sys_reg(&self, reg: SysReg) -> Result<u64>;
    fn set_sys_reg(&mut self, reg: SysReg, value: u64) -> Result<()>;
    /// Read a stopped GIC distributor register for architectural inventory.
    fn get_gic_dist(&self, _offset: u16) -> Result<u64> {
        Err("GIC register inventory unsupported".into())
    }
    /// Enter once. Interrupted stops hold no operation; do not call complete().
    fn run(&mut self) -> Result<RunStop>;
    fn complete(&mut self, read: Option<u64>) -> Result<()>;
    fn inject_spi(&mut self, intid: u32, level: bool) -> Result<()>;
    fn dirty_pages(&self, slot: u32) -> Result<DirtyPages>;
    /// Capture restorable state with no held native operation. Evidence-only
    /// register inspection does not by itself promise a restorable snapshot.
    fn snapshot_state(&self) -> Result<BackendState> {
        Err("backend architectural snapshot is unavailable".into())
    }
    /// Restore architectural state into a fresh, never-entered VM. The caller
    /// restores mapped RAM and machine/device policy separately before execution.
    fn restore_state(&mut self, _state: &BackendState) -> Result<()> {
        Err("backend architectural restore is unavailable".into())
    }
    fn state(&self) -> Result<BackendState> {
        Err("complete stopped backend state is unavailable".into())
    }
}

// Native adapters and the portable register mocks share this encoding.
#[cfg(any(
    test,
    all(target_arch = "aarch64", any(target_os = "linux", target_os = "macos"))
))]
pub(crate) fn core_index(reg: CoreReg) -> Result<u32> {
    match reg {
        CoreReg::X(n @ 0..=30) => Ok(n as u32),
        CoreReg::X(n) => Err(format!("invalid core register x{n}")),
        CoreReg::Pc => Ok(32),
        CoreReg::Pstate => Ok(33),
        CoreReg::Sp => Ok(31),
    }
}
