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
#[derive(Clone, Copy, Debug)]
pub struct SysReg(pub u16);
impl SysReg {
    pub const SCTLR: Self = Self(0xc080);
    pub const MPIDR: Self = Self(0xc005);
    pub const CNTKCTL: Self = Self(0xc708);
    pub const CNTV_CTL: Self = Self(0xdf19);
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
#[derive(Debug, PartialEq, Eq)]
pub enum DirtyPages {
    Unsupported,
    Bitmap { page_size: usize, words: Vec<u64> },
}

/// One VM and one vCPU, confined to the creating thread. `run` is synchronous;
/// every other operation is performed while stopped. Backends retain mappings
/// until unmap/destruction. `complete` retires the held operation exactly once;
/// on KVM the kernel finishes MMIO on the next entry, before executing guest code.
/// Neither host interruptions nor backend maintenance may advance guest time.
pub trait Backend: Sized {
    fn create() -> Result<Self>;
    fn map_memory(&mut self, slot: u32, ipa: u64, memory: Rc<Memory>) -> Result<()>;
    fn unmap_memory(&mut self, slot: u32) -> Result<()>;
    fn create_vcpu(&mut self) -> Result<()>;
    fn get_reg(&self, reg: CoreReg) -> Result<u64>;
    fn set_reg(&mut self, reg: CoreReg, value: u64) -> Result<()>;
    fn get_sys_reg(&self, reg: SysReg) -> Result<u64>;
    fn set_sys_reg(&mut self, reg: SysReg, value: u64) -> Result<()>;
    fn run(&mut self) -> Result<Stop>;
    fn complete(&mut self, read: Option<u64>) -> Result<()>;
    fn inject_spi(&mut self, intid: u32, level: bool) -> Result<()>;
    fn dirty_pages(&self, slot: u32) -> Result<DirtyPages>;
}

pub(crate) fn core_index(reg: CoreReg) -> Result<u32> {
    match reg {
        CoreReg::X(n @ 0..=30) => Ok(n as u32),
        CoreReg::X(n) => Err(format!("invalid core register x{n}")),
        CoreReg::Pc => Ok(32),
        CoreReg::Pstate => Ok(33),
        CoreReg::Sp => Ok(31),
    }
}
