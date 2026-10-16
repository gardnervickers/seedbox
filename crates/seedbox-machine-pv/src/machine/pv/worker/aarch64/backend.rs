//! Native ABI adapter. It holds one unfinished instruction at most; host
//! cancellations hold none. All deterministic scheduling belongs to Machine.
use super::{memory::Memory, Result};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, rc::Rc};
#[cfg(target_os = "macos")]
mod hvf;
#[cfg(target_os = "linux")]
mod kvm;
#[cfg(target_os = "macos")]
pub(in super::super) use hvf::Native;
#[cfg(target_os = "linux")]
pub(in super::super) use kvm::Native;
#[derive(Clone, Debug)]
pub(in super::super) enum Exit {
    Mmio {
        address: u64,
        width: u8,
        write: bool,
        value: u64,
    },
    Call,
    // Linux KVM handles WFx/system traps in-kernel; only HVF (and the
    // architecture-neutral scripted model) forwards these rendezvous.
    #[cfg(any(target_os = "macos", test))]
    Wait {
        event: bool,
    },
    #[cfg(any(target_os = "macos", test))]
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
    /// Canonical owed native IO only; never settle/enter during observation.
    fn canonical_pending_io(&self) -> Result<Vec<u8>> {
        Err("native pending IO observation unavailable".into())
    }
    fn irq(&mut self, number: u32, level: bool) -> Result<()>;
    fn capture(&self) -> Result<Architecture>;
    /// Never substitute the opaque GIC snapshot blob for actual register state.
    fn stopped_parts(&mut self) -> Result<Vec<(String, String)>> {
        Err("PV ARM actual stopped GIC/register evidence is not yet available; no snapshot-byte fallback".into())
    }
    fn finish_stop(&mut self) -> Result<()> {
        Ok(())
    }
    fn restore(&mut self, state: &Architecture) -> Result<()>;
}

// Closed persisted record: exhaustive destructuring prevents silently omitting new fields.
impl crate::machine::pv::codec::record::Record for Architecture {
    const MIN: u64 = <Vec<u64> as crate::machine::pv::codec::record::Record>::MIN
        + <BTreeMap<u16, u64> as crate::machine::pv::codec::record::Record>::MIN
        + <Vec<[u8; 16]> as crate::machine::pv::codec::record::Record>::MIN
        + <BTreeMap<u16, u64> as crate::machine::pv::codec::record::Record>::MIN
        + <Vec<u8> as crate::machine::pv::codec::record::Record>::MIN;
    fn encode<W: std::io::Write>(
        &self,
        w: &mut seedbox_machine::snapshot::Writer<'_, W>,
    ) -> seedbox_machine::snapshot::Result<()> {
        let Self {
            core,
            system,
            simd,
            icc,
            gic,
        } = self;
        core.encode(w)?;
        system.encode(w)?;
        simd.encode(w)?;
        icc.encode(w)?;
        gic.encode(w)?;
        Ok(())
    }
    fn decode<R: std::io::Read>(
        r: &mut seedbox_machine::snapshot::Reader<'_, R>,
    ) -> seedbox_machine::snapshot::Result<Self> {
        Ok(Self {
            core: crate::machine::pv::codec::record::Record::decode(r)?,
            system: crate::machine::pv::codec::record::Record::decode(r)?,
            simd: crate::machine::pv::codec::record::Record::decode(r)?,
            icc: crate::machine::pv::codec::record::Record::decode(r)?,
            gic: crate::machine::pv::codec::record::Record::decode(r)?,
        })
    }
}

impl Architecture {
    pub(in super::super) fn validate_snapshot(&self) -> Result<()> {
        if self.core.len() != 35
            || self.simd.len() != 32
            || self.gic.is_empty()
            || self.gic.len() > 4 << 20
        {
            return Err("PV snapshot HVF register shape".into());
        }
        Ok(())
    }
}
