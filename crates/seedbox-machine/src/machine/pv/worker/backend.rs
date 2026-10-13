//! Native ABI adapter. It holds one unfinished instruction at most; host
//! cancellations hold none. All deterministic scheduling belongs to Machine.
use super::{memory::Memory, Result};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, rc::Rc};
#[cfg(target_os = "macos")]
#[path = "hvf.rs"]
mod implementation;
#[cfg(target_os = "linux")]
#[path = "kvm.rs"]
mod implementation;
pub(in super::super) use implementation::Native;
#[derive(Clone, Debug)]
pub(in super::super) enum Exit {
    Mmio {
        address: u64,
        width: u8,
        write: bool,
        value: u64,
    },
    Call,
    Wait {
        event: bool,
    },
    System {
        register: u16,
        write: bool,
        value: u64,
    },
    Interrupted,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(in super::super) struct Architecture {
    pub(super) core: Vec<u64>,
    pub(super) system: BTreeMap<u16, u64>,
    pub(super) simd: Vec<[u8; 16]>,
    pub(super) icc: BTreeMap<u16, u64>,
    pub(super) gic: Vec<u8>,
}
// Core numbers are x0..x30, SP_EL1=31, PC=32, PSTATE=33.
pub(in super::super) trait Backend: Sized {
    fn create(ram: Rc<Memory>, page: Rc<Memory>) -> Result<Self>;
    fn core(&self, register: u32) -> Result<u64>;
    fn set_core(&mut self, register: u32, value: u64) -> Result<()>;
    fn system(&self, register: u16) -> Result<u64>;
    fn set_system(&mut self, register: u16, value: u64) -> Result<()>;
    fn enter(&mut self) -> Result<Exit>;
    fn complete(&mut self, response: Option<u64>) -> Result<()>;
    fn irq(&mut self, number: u32, level: bool) -> Result<()>;
    fn capture(&self) -> Result<Architecture>;
    fn restore(&mut self, state: &Architecture) -> Result<()>;
}
