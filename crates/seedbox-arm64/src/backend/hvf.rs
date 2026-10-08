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
    fn hv_vcpu_get_simd_fp_reg(cpu: u64, reg: u32, value: *mut SimdValue) -> i32;
    fn os_release(object: *mut c_void);
}
#[repr(C, align(16))]
struct SimdValue([u8; 16]);
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
        // HVF has no SPI-count configuration API. Require its advertised
        // interrupt range to match the HVF profile rather than assuming it.
        let typer = gic_dist(4)?;
        if (typer & 31) != (crate::HVF_GIC_NR_IRQS / 32 - 1) as u64 {
            return Err(format!(
                "HVF GICD_TYPER={typer:#x}: expected {} interrupt IDs",
                crate::HVF_GIC_NR_IRQS
            ));
        }
        Ok(Self { set_spi })
    }
}
fn gic_dist(offset: u16) -> Result<u64> {
    unsafe {
        let get: unsafe extern "C" fn(u16, *mut u64) -> i32 =
            std::mem::transmute(symbol(c"hv_gic_get_distributor_reg")?);
        let mut value = 0;
        check("hv_gic_get_distributor_reg", get(offset, &mut value))?;
        Ok(value)
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
    const NAME: &'static str = "hvf";
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
    fn get_gic_dist(&self, offset: u16) -> Result<u64> {
        gic_dist(offset)
    }
    fn run(&mut self) -> Result<RunStop> {
        if self.pending.is_some() {
            return Err("run before completing held HVF exit".into());
        }
        unsafe {
            check("hv_vcpu_run", hv_vcpu_run(self.cpu()?))?;
        }
        let raw = unsafe { *self.exit };
        if raw.reason == 0 {
            // No architectural instruction is held. In particular a queued
            // cancel may return without entering at all; never complete PC+4.
            return Ok(RunStop::Interrupted);
        }
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
        Ok(Stop { pc, exit }.into())
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
    fn state(&self) -> Result<BackendState> {
        let cpu = self.cpu()?;
        let mut core = Vec::new();
        for reg in 0..=34 {
            let mut value = 0;
            unsafe {
                check(
                    "state hv_vcpu_get_reg",
                    hv_vcpu_get_reg(cpu, reg, &mut value),
                )?;
            }
            core.push(value);
        }
        let mut simd = Vec::new();
        for reg in 0..32 {
            let mut value = SimdValue([0; 16]);
            unsafe {
                check(
                    "state hv_vcpu_get_simd_fp_reg",
                    hv_vcpu_get_simd_fp_reg(cpu, reg, &mut value),
                )?;
            }
            simd.push(value.0);
        }
        let mut system = BTreeMap::new();
        // EL1/EL0 state supported by the macOS 15 API. EL2, SVE, SME, PMU and
        // architectural timer execution are not exposed by this guest profile.
        // TVAL is derived from host counter time and is deliberately not read.
        for reg in [
            0x8010, 0x8012, 0xc000, 0xc005, 0xc020, 0xc021, 0xc028, 0xc029, 0xc030, 0xc031, 0xc038,
            0xc039, 0xc03a, 0xc080, 0xc081, 0xc082, 0xc100, 0xc101, 0xc102, 0xc108, 0xc109, 0xc10a,
            0xc10b, 0xc110, 0xc111, 0xc112, 0xc113, 0xc118, 0xc119, 0xc200, 0xc201, 0xc208, 0xc288,
            0xc289, 0xc290, 0xc300, 0xc3a0, 0xc510, 0xc518, 0xc600, 0xc681, 0xc684, 0xc708, 0xd000,
            0xde82, 0xde83, 0xdf19, 0xdf1a, 0xe208, 0xdf11, 0xdf12,
        ] {
            system.insert(reg, self.get_sys_reg(SysReg(reg))?);
        }
        let dfr = system[&0xc028];
        for (field, base) in [(12, 0x8004u16), (20, 0x8006u16)] {
            let count = ((dfr >> field) & 15) + 1;
            for index in 0..count {
                for reg in [base + index as u16 * 8, base + index as u16 * 8 + 1] {
                    system.insert(reg, self.get_sys_reg(SysReg(reg))?);
                }
            }
        }
        let mut gic_cpu = BTreeMap::new();
        let mut gic = Vec::new();
        unsafe {
            let get_icc: unsafe extern "C" fn(u64, u16, *mut u64) -> i32 =
                std::mem::transmute(symbol(c"hv_gic_get_icc_reg")?);
            for reg in [
                0xc230, 0xc643, 0xc644, 0xc648, 0xc65b, 0xc663, 0xc664, 0xc665, 0xc666, 0xc667,
            ] {
                let mut value = 0;
                check("state hv_gic_get_icc_reg", get_icc(cpu, reg, &mut value))?;
                gic_cpu.insert(reg, value);
            }
            let create: unsafe extern "C" fn() -> *mut c_void =
                std::mem::transmute(symbol(c"hv_gic_state_create")?);
            let size: unsafe extern "C" fn(*mut c_void, *mut usize) -> i32 =
                std::mem::transmute(symbol(c"hv_gic_state_get_size")?);
            let data: unsafe extern "C" fn(*mut c_void, *mut c_void) -> i32 =
                std::mem::transmute(symbol(c"hv_gic_state_get_data")?);
            let state = create();
            if state.is_null() {
                return Err("hv_gic_state_create returned null".into());
            }
            let result = (|| {
                let mut len = 0;
                check("state hv_gic_state_get_size", size(state, &mut len))?;
                if len == 0 || len > 4 << 20 {
                    return Err("invalid serialized GIC state size".into());
                }
                gic.resize(len, 0);
                check(
                    "state hv_gic_state_get_data",
                    data(state, gic.as_mut_ptr().cast()),
                )
            })();
            os_release(state);
            result?;
        }
        Ok(BackendState {
            core,
            system,
            simd,
            gic_cpu,
            gic,
        })
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
    // Selected and signed explicitly by the native primitive check, never
    // executed by portable tests or unsigned Cargo test discovery.
    #[test]
    #[ignore = "requires signed Apple Silicon hypervisor entitlement"]
    fn canceled_entry_holds_no_instruction_and_preserves_pc() {
        unsafe extern "C" {
            fn hv_vcpus_exit(cpus: *const u64, count: u32) -> i32;
        }
        let mut backend = Hvf::create().unwrap();
        let memory = Rc::new(Memory::new(crate::PAGE_SIZE).unwrap());
        // HVC #0 is a guaranteed finite natural exit once the queued cancel
        // has returned. It must still execute at the original PC.
        unsafe {
            memory.write(0, &0xd400_0002u32.to_le_bytes()).unwrap();
        }
        backend.map_memory(0, crate::RAM_BASE, memory).unwrap();
        backend.create_vcpu().unwrap();
        backend.set_reg(CoreReg::Pstate, 0x3c5).unwrap();
        backend.set_reg(CoreReg::Pc, crate::RAM_BASE).unwrap();
        let cpu = backend.cpu().unwrap();
        for _ in 0..3 {
            unsafe {
                check("test hv_vcpus_exit", hv_vcpus_exit(&cpu, 1)).unwrap();
            }
            assert!(matches!(backend.run().unwrap(), RunStop::Interrupted));
            assert_eq!(backend.get_reg(CoreReg::Pc).unwrap(), crate::RAM_BASE);
            assert_eq!(
                backend.complete(None).unwrap_err(),
                "no HVF exit to complete"
            );
        }
        let RunStop::Guest(stop) = backend.run().unwrap() else {
            panic!("stale cancellation survived its observed stop");
        };
        assert_eq!(stop.pc, crate::RAM_BASE);
        assert!(matches!(stop.exit, Exit::Hvc { imm: 0 }));
        assert_eq!(backend.get_reg(CoreReg::Pc).unwrap(), crate::RAM_BASE + 4);
        assert!(backend
            .run()
            .unwrap_err()
            .contains("before completing held HVF exit"));
        backend.complete(None).unwrap();
        assert_eq!(backend.get_reg(CoreReg::Pc).unwrap(), crate::RAM_BASE + 4);
    }
}
