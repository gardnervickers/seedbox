//! Apple's public C ABI. GIC functions are loaded dynamically for SDK 14 builds;
//! missing macOS 15 APIs are errors, never a fallback to a different GIC.
use super::*;
use crate::{GIC_DIST, GIC_REDIST};
use std::{
    collections::BTreeMap,
    ffi::{c_void, CStr},
    marker::PhantomData,
    rc::Rc,
};

#[repr(C)]
#[derive(Clone, Copy)]
struct Exception {
    syndrome: u64,
    virtual_address: u64,
    physical_address: u64,
}
#[repr(C)]
#[derive(Clone, Copy)]
struct HvExit {
    reason: u32,
    exception: Exception,
}
#[link(name = "Hypervisor", kind = "framework")]
unsafe extern "C" {
    fn hv_vm_create(config: *mut c_void) -> i32;
    fn hv_vm_destroy() -> i32;
    fn hv_vm_map(addr: *const c_void, ipa: u64, size: usize, flags: u64) -> i32;
    fn hv_vm_unmap(ipa: u64, size: usize) -> i32;
    fn hv_vcpu_create(cpu: *mut u64, exit: *mut *const HvExit, config: *mut c_void) -> i32;
    fn hv_vcpu_destroy(cpu: u64) -> i32;
    fn hv_vcpu_run(cpu: u64) -> i32;
    fn hv_vcpu_get_reg(cpu: u64, reg: u32, value: *mut u64) -> i32;
    fn hv_vcpu_set_reg(cpu: u64, reg: u32, value: u64) -> i32;
    fn hv_vcpu_get_sys_reg(cpu: u64, reg: u16, value: *mut u64) -> i32;
    fn hv_vcpu_set_sys_reg(cpu: u64, reg: u16, value: u64) -> i32;
    fn hv_vcpu_set_vtimer_mask(cpu: u64, masked: bool) -> i32;
    fn os_release(object: *mut c_void);
}
fn check(op: &str, status: i32) -> Result<()> {
    if status == 0 {
        Ok(())
    } else {
        Err(format!("{op}: Hypervisor.framework error {:#010x}; requires arm64 macOS 15+, hv_gic, and a hypervisor-entitled binary", status as u32))
    }
}
unsafe fn symbol(name: &CStr) -> Result<*mut c_void> {
    let p = libc::dlsym(libc::RTLD_DEFAULT, name.as_ptr());
    if p.is_null() {
        Err(format!(
            "{} unavailable; macOS 15+ hv_gic is required",
            name.to_string_lossy()
        ))
    } else {
        Ok(p)
    }
}
type SetSpi = unsafe extern "C" fn(u32, bool) -> i32;
struct Gic {
    set_spi: SetSpi,
}
impl Gic {
    unsafe fn create() -> Result<Self> {
        let create_config: unsafe extern "C" fn() -> *mut c_void =
            std::mem::transmute(symbol(c"hv_gic_config_create")?);
        let set_dist: unsafe extern "C" fn(*mut c_void, u64) -> i32 =
            std::mem::transmute(symbol(c"hv_gic_config_set_distributor_base")?);
        let set_redist: unsafe extern "C" fn(*mut c_void, u64) -> i32 =
            std::mem::transmute(symbol(c"hv_gic_config_set_redistributor_base")?);
        let create: unsafe extern "C" fn(*mut c_void) -> i32 =
            std::mem::transmute(symbol(c"hv_gic_create")?);
        let set_spi: SetSpi = std::mem::transmute(symbol(c"hv_gic_set_spi")?);
        let get_range: unsafe extern "C" fn(*mut u32, *mut u32) -> i32 =
            std::mem::transmute(symbol(c"hv_gic_get_spi_interrupt_range")?);
        let mut base = 0;
        let mut count = 0;
        check(
            "hv_gic_get_spi_interrupt_range",
            get_range(&mut base, &mut count),
        )?;
        if crate::TIMER_SPI < base || crate::TIMER_SPI >= base.saturating_add(count) {
            return Err(format!(
                "hv_gic SPI range {base}..{} excludes timer SPI {}",
                base.saturating_add(count),
                crate::TIMER_SPI
            ));
        }
        let config = create_config();
        if config.is_null() {
            return Err("hv_gic_config_create returned null".into());
        }
        let result = (|| {
            check(
                "hv_gic_config_set_distributor_base",
                set_dist(config, GIC_DIST),
            )?;
            check(
                "hv_gic_config_set_redistributor_base",
                set_redist(config, GIC_REDIST),
            )?;
            check("hv_gic_create", create(config))
        })();
        os_release(config);
        result?;
        Ok(Self { set_spi })
    }
}
struct Pending {
    exit: Exit,
    pc: u64,
    sign_extend: bool,
    wide: bool,
}
pub struct Hvf {
    cpu: Option<u64>,
    exit: *const HvExit,
    gic: Gic,
    mappings: BTreeMap<u32, (u64, Rc<Memory>)>,
    pending: Option<Pending>,
    _thread: PhantomData<Rc<()>>,
}
impl Hvf {
    fn cpu(&self) -> Result<u64> {
        self.cpu.ok_or("HVF vCPU has not been created".into())
    }
}
impl Backend for Hvf {
    fn create() -> Result<Self> {
        unsafe {
            check("hv_vm_create", hv_vm_create(std::ptr::null_mut()))?;
            let gic = match Gic::create() {
                Ok(gic) => gic,
                Err(e) => {
                    hv_vm_destroy();
                    return Err(e);
                }
            };
            Ok(Self {
                cpu: None,
                exit: std::ptr::null(),
                gic,
                mappings: BTreeMap::new(),
                pending: None,
                _thread: PhantomData,
            })
        }
    }
    fn map_memory(&mut self, slot: u32, ipa: u64, memory: Rc<Memory>) -> Result<()> {
        if self.mappings.contains_key(&slot) {
            return Err(format!("memory slot {slot} already mapped"));
        }
        unsafe {
            check(
                "hv_vm_map",
                hv_vm_map(memory.as_ptr().cast(), ipa, memory.len(), 7),
            )?;
        }
        self.mappings.insert(slot, (ipa, memory));
        Ok(())
    }
    fn unmap_memory(&mut self, slot: u32) -> Result<()> {
        let (ipa, memory) = self.mappings.get(&slot).ok_or("unknown memory slot")?;
        unsafe {
            check("hv_vm_unmap", hv_vm_unmap(*ipa, memory.len()))?;
        }
        self.mappings.remove(&slot);
        Ok(())
    }
    fn create_vcpu(&mut self) -> Result<()> {
        if self.cpu.is_some() {
            return Err("only one vCPU is supported".into());
        }
        let mut cpu = 0;
        unsafe {
            check(
                "hv_vcpu_create",
                hv_vcpu_create(&mut cpu, &mut self.exit, std::ptr::null_mut()),
            )?;
        }
        self.cpu = Some(cpu);
        self.set_sys_reg(SysReg::MPIDR, 0x8000_0000)?;
        unsafe {
            let get_base: unsafe extern "C" fn(u64, *mut u64) -> i32 =
                std::mem::transmute(symbol(c"hv_gic_get_redistributor_base")?);
            let mut base = 0;
            check("hv_gic_get_redistributor_base", get_base(cpu, &mut base))?;
            if base != GIC_REDIST {
                return Err(format!(
                    "hv_gic redistributor {base:#x} differs from FDT {GIC_REDIST:#x}"
                ));
            }
        }
        // Leave delivery visible. Any activated architectural timer is a leak.
        unsafe {
            check(
                "hv_vcpu_set_vtimer_mask",
                hv_vcpu_set_vtimer_mask(cpu, false),
            )
        }
    }
    fn get_reg(&self, reg: CoreReg) -> Result<u64> {
        core_index(reg)?;
        if matches!(reg, CoreReg::Sp) {
            return self.get_sys_reg(SysReg(0xe208));
        }
        let id = match reg {
            CoreReg::X(n) => n as u32,
            CoreReg::Pc => 31,
            CoreReg::Pstate => 34,
            CoreReg::Sp => unreachable!(),
        };
        let mut value = 0;
        unsafe {
            check(
                "hv_vcpu_get_reg",
                hv_vcpu_get_reg(self.cpu()?, id, &mut value),
            )?;
        }
        Ok(value)
    }
    fn set_reg(&mut self, reg: CoreReg, value: u64) -> Result<()> {
        core_index(reg)?;
        if matches!(reg, CoreReg::Sp) {
            return self.set_sys_reg(SysReg(0xe208), value);
        }
        let id = match reg {
            CoreReg::X(n) => n as u32,
            CoreReg::Pc => 31,
            CoreReg::Pstate => 34,
            CoreReg::Sp => unreachable!(),
        };
        unsafe { check("hv_vcpu_set_reg", hv_vcpu_set_reg(self.cpu()?, id, value)) }
    }
    fn get_sys_reg(&self, reg: SysReg) -> Result<u64> {
        let mut value = 0;
        unsafe {
            check(
                "hv_vcpu_get_sys_reg",
                hv_vcpu_get_sys_reg(self.cpu()?, reg.0, &mut value),
            )?;
        }
        Ok(value)
    }
    fn set_sys_reg(&mut self, reg: SysReg, value: u64) -> Result<()> {
        unsafe {
            check(
                "hv_vcpu_set_sys_reg",
                hv_vcpu_set_sys_reg(self.cpu()?, reg.0, value),
            )
        }
    }
    fn run(&mut self) -> Result<Stop> {
        if self.pending.is_some() {
            return Err("run before completing held HVF exit".into());
        }
        let raw = loop {
            unsafe {
                check("hv_vcpu_run", hv_vcpu_run(self.cpu()?))?;
            }
            let raw = unsafe { *self.exit };
            if raw.reason != 0 {
                break raw;
            } // canceled: no semantic event
        };
        if raw.reason == 2 {
            return Err("VTIMER_ACTIVATED: hardware timer leaked into PV guest".into());
        }
        let mut pc = self.get_reg(CoreReg::Pc)?;
        let esr = raw.exception.syndrome;
        let mut sign_extend = false;
        let mut wide = true;
        let exit = if raw.reason != 1 {
            Exit::Other(raw.reason as u64)
        } else {
            match esr >> 26 {
                0x24 if esr & (1 << 24) != 0 => {
                    let len = 1u8 << ((esr >> 22) & 3);
                    let rt = ((esr >> 16) & 31) as u8;
                    let write = esr & (1 << 6) != 0;
                    sign_extend = esr & (1 << 21) != 0;
                    wide = esr & (1 << 15) != 0;
                    let data = if write && rt != 31 {
                        self.get_reg(CoreReg::X(rt))? & (u64::MAX >> (64 - len as u32 * 8))
                    } else {
                        0
                    };
                    Exit::Mmio {
                        addr: raw.exception.physical_address,
                        len,
                        write,
                        data,
                        reg: Some(rt),
                    }
                }
                0x16 => {
                    pc = pc.checked_sub(4).ok_or("invalid HVC PC")?;
                    Exit::Hvc { imm: esr as u16 }
                }
                0x01 if esr & 1 == 0 => Exit::Wfi,
                0x18 => {
                    let rt = ((esr >> 5) & 31) as u8;
                    let write = esr & 1 == 0;
                    let reg = (((esr >> 20) & 3) << 14
                        | ((esr >> 14) & 7) << 11
                        | ((esr >> 10) & 15) << 7
                        | ((esr >> 1) & 15) << 3
                        | ((esr >> 17) & 7)) as u16;
                    Exit::SysReg {
                        reg,
                        write,
                        rt,
                        data: if write && rt != 31 {
                            self.get_reg(CoreReg::X(rt))?
                        } else {
                            0
                        },
                    }
                }
                _ => Exit::Other(esr),
            }
        };
        self.pending = Some(Pending {
            exit: exit.clone(),
            pc,
            sign_extend,
            wide,
        });
        Ok(Stop { pc, exit })
    }
    fn complete(&mut self, read: Option<u64>) -> Result<()> {
        let p = self.pending.take().ok_or("no HVF exit to complete")?;
        match p.exit {
            Exit::Mmio {
                len, write, reg, ..
            } => {
                if !write {
                    let mut v = read.ok_or("missing MMIO read response")?;
                    if p.sign_extend {
                        let shift = 64 - len as u32 * 8;
                        v = ((v << shift) as i64 >> shift) as u64;
                    }
                    if !p.wide {
                        v &= u32::MAX as u64;
                    }
                    if let Some(rt @ 0..=30) = reg {
                        self.set_reg(CoreReg::X(rt), v)?;
                    }
                }
                self.set_reg(CoreReg::Pc, p.pc + 4)
            }
            Exit::Wfi => self.set_reg(CoreReg::Pc, p.pc + 4),
            Exit::SysReg { write, rt, .. } => {
                if !write {
                    let value = read.ok_or("missing sysreg read response")?;
                    if rt != 31 {
                        self.set_reg(CoreReg::X(rt), value)?;
                    }
                }
                self.set_reg(CoreReg::Pc, p.pc + 4)
            }
            Exit::Hvc { .. } => Ok(()), // HVC's architectural PC already advanced
            _ => Err("unsupported HVF exit completion".into()),
        }
    }
    fn inject_spi(&mut self, intid: u32, level: bool) -> Result<()> {
        unsafe { check("hv_gic_set_spi", (self.gic.set_spi)(intid, level)) }
    }
    fn dirty_pages(&self, _: u32) -> Result<DirtyPages> {
        Ok(DirtyPages::Unsupported)
    }
}
impl Drop for Hvf {
    fn drop(&mut self) {
        unsafe {
            if let Some(cpu) = self.cpu.take() {
                let rc = hv_vcpu_destroy(cpu);
                if rc != 0 {
                    eprintln!("hv_vcpu_destroy: {rc:#x}");
                }
            }
            let rc = hv_vm_destroy();
            if rc != 0 {
                eprintln!("hv_vm_destroy: {rc:#x}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn public_c_exit_layout() {
        assert_eq!(std::mem::size_of::<HvExit>(), 32);
        assert_eq!(std::mem::offset_of!(HvExit, exception), 8);
        assert_eq!(std::mem::size_of::<Exception>(), 24);
    }
}
