//! One process owns one Hypervisor.framework VM. GIC state requires macOS 15.
use super::{Architecture, Backend, Exit, Memory, Result};
use crate::machine::pv::worker::evidence::arm;
use std::{
    collections::BTreeMap,
    ffi::{c_void, CStr},
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
struct Landing {
    reason: u32,
    exception: Exception,
}
#[repr(C, align(16))]
struct Vector([u8; 16]);
#[link(name = "Hypervisor", kind = "framework")]
unsafe extern "C" {
    fn hv_vm_create(config: *mut c_void) -> i32;
    fn hv_vm_destroy() -> i32;
    fn hv_vm_map(pointer: *const c_void, address: u64, length: usize, flags: u64) -> i32;
    fn hv_vcpu_create(cpu: *mut u64, exit: *mut *const Landing, config: *mut c_void) -> i32;
    fn hv_vcpu_destroy(cpu: u64) -> i32;
    fn hv_vcpu_run(cpu: u64) -> i32;
    fn hv_vcpu_get_reg(cpu: u64, register: u32, value: *mut u64) -> i32;
    fn hv_vcpu_set_reg(cpu: u64, register: u32, value: u64) -> i32;
    fn hv_vcpu_get_sys_reg(cpu: u64, register: u16, value: *mut u64) -> i32;
    fn hv_vcpu_set_sys_reg(cpu: u64, register: u16, value: u64) -> i32;
    fn hv_vcpu_get_simd_fp_reg(cpu: u64, register: u32, value: *mut Vector) -> i32;

    fn hv_vcpu_set_vtimer_mask(cpu: u64, masked: bool) -> i32;
    fn os_release(pointer: *mut c_void);
}
#[link(name = "seedbox_pv_vector", kind = "static")]
unsafe extern "C" {
    fn seedbox_pv_set_vector(cpu: u64, register: u32, value: *const u8) -> i32;
}
fn checked(operation: &str, status: i32) -> Result<()> {
    if status == 0 {
        Ok(())
    } else {
        Err(format!(
            "{operation}: HVF {:#x}; signed hypervisor entitlement and macOS 15 GIC required",
            status as u32
        ))
    }
}
unsafe fn api(name: &CStr) -> Result<*mut c_void> {
    let pointer = libc::dlsym(libc::RTLD_DEFAULT, name.as_ptr());
    if pointer.is_null() {
        Err(format!("HVF API {} unavailable", name.to_string_lossy()))
    } else {
        Ok(pointer)
    }
}
struct Held {
    pc: u64,
    destination: Option<u8>,
    width: u8,
    signed: bool,
    wide: bool,
    call: bool,
}
// The native HVF core bank is x0..x30, PC, FPCR, FPSR, CPSR. Keep that
// existing snapshot layout separate from the architectural evidence projection.
struct Registers {
    core: Vec<u64>,
    system: BTreeMap<u16, u64>,
    simd: Vec<[u8; 16]>,
}
pub(crate) struct Native {
    cpu: Option<u64>,
    landing: *const Landing,
    held: Option<Held>,
    entered: bool,
    poisoned: bool,
    // The VM is destroyed in Drop before the last RAM mapping references die.
    _ram: Rc<Memory>,
    _page: Rc<Memory>,
}
impl Native {
    fn cpu(&self) -> Result<u64> {
        self.cpu.ok_or("PV vCPU absent".into())
    }
    fn raw_core(&self, register: u32) -> Result<u64> {
        let mut value = 0;
        unsafe {
            checked(
                "read HVF core",
                hv_vcpu_get_reg(self.cpu()?, register, &mut value),
            )?;
        }
        Ok(value)
    }
    fn raw_set(&self, register: u32, value: u64) -> Result<()> {
        unsafe {
            checked(
                "write HVF core",
                hv_vcpu_set_reg(self.cpu()?, register, value),
            )
        }
    }
    unsafe fn create_gic() -> Result<()> {
        let create: unsafe extern "C" fn() -> *mut c_void =
            std::mem::transmute(api(c"hv_gic_config_create")?);
        let dist: unsafe extern "C" fn(*mut c_void, u64) -> i32 =
            std::mem::transmute(api(c"hv_gic_config_set_distributor_base")?);
        let redist: unsafe extern "C" fn(*mut c_void, u64) -> i32 =
            std::mem::transmute(api(c"hv_gic_config_set_redistributor_base")?);
        let apply: unsafe extern "C" fn(*mut c_void) -> i32 =
            std::mem::transmute(api(c"hv_gic_create")?);
        let config = create();
        if config.is_null() {
            return Err("HVF GIC config absent".into());
        }
        let result = (|| {
            checked("HVF GIC distributor", dist(config, 0x0800_0000))?;
            checked("HVF GIC redistributor", redist(config, 0x080a_0000))?;
            checked("HVF GIC create", apply(config))
        })();
        os_release(config);
        result
    }
    fn cpu_inventory(&self) -> Result<Registers> {
        let mut core = Vec::new();
        for register in 0..35 {
            core.push(self.raw_core(register)?);
        }
        let mut simd = Vec::new();
        for register in 0..32 {
            let mut value = Vector([0; 16]);
            unsafe {
                checked(
                    "capture HVF vector",
                    hv_vcpu_get_simd_fp_reg(self.cpu()?, register, &mut value),
                )?;
            }
            simd.push(value.0);
        }
        let mut system = BTreeMap::new();
        // Public HVF EL0/EL1 register inventory. Counter TVAL is host-derived;
        // architectural timer execution is prohibited by the PV guest profile.
        for register in [
            0x8010, 0x8012, 0xc000, 0xc005, 0xc020, 0xc021, 0xc028, 0xc029, 0xc030, 0xc031, 0xc038,
            0xc039, 0xc03a, 0xc080, 0xc081, 0xc082, 0xc100, 0xc101, 0xc102, 0xc108, 0xc109, 0xc10a,
            0xc10b, 0xc110, 0xc111, 0xc112, 0xc113, 0xc118, 0xc119, 0xc200, 0xc201, 0xc208, 0xc288,
            0xc289, 0xc290, 0xc300, 0xc3a0, 0xc510, 0xc518, 0xc600, 0xc681, 0xc684, 0xc708, 0xd000,
            0xde82, 0xde83, 0xdf19, 0xdf1a, 0xe208, 0xdf11, 0xdf12,
        ] {
            system.insert(register, self.system(register)?);
        }
        for (shift, base) in [(12, 0x8004), (20, 0x8006)] {
            for index in 0..=((system[&0xc028] >> shift) & 15) {
                for register in [base + index as u16 * 8, base + index as u16 * 8 + 1] {
                    system.insert(register, self.system(register)?);
                }
            }
        }
        Ok(Registers { core, system, simd })
    }
    fn icc_inventory(&self) -> Result<BTreeMap<u16, u64>> {
        unsafe {
            let get: unsafe extern "C" fn(u64, u16, *mut u64) -> i32 =
                std::mem::transmute(api(c"hv_gic_get_icc_reg")?);
            let mut icc = BTreeMap::new();
            for register in arm::ICC {
                let mut value = 0;
                checked("capture HVF ICC", get(self.cpu()?, register, &mut value))?;
                icc.insert(register, value);
            }
            Ok(icc)
        }
    }
    fn inventory(&self) -> Result<Architecture> {
        let Registers { core, system, simd } = self.cpu_inventory()?;
        let icc = self.icc_inventory()?;
        unsafe {
            let create: unsafe extern "C" fn() -> *mut c_void =
                std::mem::transmute(api(c"hv_gic_state_create")?);
            let size: unsafe extern "C" fn(*mut c_void, *mut usize) -> i32 =
                std::mem::transmute(api(c"hv_gic_state_get_size")?);
            let read: unsafe extern "C" fn(*mut c_void, *mut c_void) -> i32 =
                std::mem::transmute(api(c"hv_gic_state_get_data")?);
            let state = create();
            if state.is_null() {
                return Err("HVF GIC state absent".into());
            }
            let result = (|| {
                let mut length = 0;
                checked("capture HVF GIC size", size(state, &mut length))?;
                if length == 0 || length > 4 << 20 {
                    return Err("HVF GIC state size".into());
                }
                let mut gic = vec![0; length];
                checked("capture HVF GIC", read(state, gic.as_mut_ptr().cast()))?;
                Ok(Architecture {
                    core,
                    system,
                    simd,
                    icc,
                    gic,
                })
            })();
            os_release(state);
            result
        }
    }
}
impl Backend for Native {
    fn create(ram: Rc<Memory>, page: Rc<Memory>) -> Result<Self> {
        unsafe {
            checked("HVF VM create", hv_vm_create(std::ptr::null_mut()))?;
        }
        let mut native = Self {
            cpu: None,
            landing: std::ptr::null(),
            held: None,
            entered: false,
            poisoned: false,
            _ram: ram,
            _page: page,
        };
        unsafe {
            Self::create_gic()?;
            checked(
                "HVF map RAM",
                hv_vm_map(native._ram.ptr().cast(), 0x4000_0000, native._ram.len(), 7),
            )?;
            checked(
                "HVF map PV page",
                hv_vm_map(
                    native._page.ptr().cast(),
                    0x0a00_0000,
                    native._page.len(),
                    7,
                ),
            )?;
            let mut cpu = 0;
            checked(
                "HVF vCPU create",
                hv_vcpu_create(&mut cpu, &mut native.landing, std::ptr::null_mut()),
            )?;
            native.cpu = Some(cpu);
            checked(
                "HVF virtual timer mask",
                hv_vcpu_set_vtimer_mask(cpu, false),
            )?;
        }
        Ok(native)
    }
    fn core(&self, register: u32) -> Result<u64> {
        match register {
            0..=30 => self.raw_core(register),
            31 => self.system(0xe208),
            32 => self.raw_core(31),
            33 => self.raw_core(34),
            _ => Err("invalid PV core register".into()),
        }
    }
    fn set_core(&mut self, register: u32, value: u64) -> Result<()> {
        match register {
            0..=30 => self.raw_set(register, value),
            31 => self.set_system(0xe208, value),
            32 => self.raw_set(31, value),
            33 => self.raw_set(34, value),
            _ => Err("invalid PV core register".into()),
        }
    }
    fn system(&self, register: u16) -> Result<u64> {
        let mut value = 0;
        unsafe {
            checked(
                "read HVF system",
                hv_vcpu_get_sys_reg(self.cpu()?, register, &mut value),
            )?;
        }
        Ok(value)
    }
    fn set_system(&mut self, register: u16, value: u64) -> Result<()> {
        unsafe {
            checked(
                "write HVF system",
                hv_vcpu_set_sys_reg(self.cpu()?, register, value),
            )
        }
    }
    fn enter(&mut self) -> Result<Exit> {
        if self.poisoned || self.held.is_some() {
            return Err("HVF entry with poisoned or incomplete state".into());
        }
        self.entered = true;
        unsafe {
            checked("HVF run", hv_vcpu_run(self.cpu()?))?;
        }
        let landing = unsafe { *self.landing };
        if landing.reason == 0 {
            return Ok(Exit::Interrupted);
        }
        if landing.reason != 1 {
            return Err(format!(
                "PV unexpected HVF landing {} (hardware timer forbidden)",
                landing.reason
            ));
        }
        let syndrome = landing.exception.syndrome;
        let pc = self.core(32)?;
        let mut held = Held {
            pc,
            destination: None,
            width: 8,
            signed: false,
            wide: true,
            call: false,
        };
        let exit = match syndrome >> 26 {
            0x24 if syndrome & (1 << 24) != 0 => {
                let register = ((syndrome >> 16) & 31) as u8;
                let write = syndrome & (1 << 6) != 0;
                held.width = 1 << ((syndrome >> 22) & 3);
                held.signed = syndrome & (1 << 21) != 0;
                held.wide = syndrome & (1 << 15) != 0;
                if !write && register != 31 {
                    held.destination = Some(register);
                }
                let value = if write && register != 31 {
                    self.core(register as u32)? & (u64::MAX >> (64 - held.width as u32 * 8))
                } else {
                    0
                };
                Exit::Mmio {
                    address: landing.exception.physical_address,
                    width: held.width,
                    write,
                    value,
                }
            }
            0x16 => {
                held.call = true;
                Exit::Call
            }
            1 => Exit::Wait {
                event: syndrome & 1 != 0,
            },
            0x18 => {
                let rt = ((syndrome >> 5) & 31) as u8;
                let write = syndrome & 1 == 0;
                let register = (((syndrome >> 20) & 3) << 14
                    | ((syndrome >> 14) & 7) << 11
                    | ((syndrome >> 10) & 15) << 7
                    | ((syndrome >> 1) & 15) << 3
                    | ((syndrome >> 17) & 7)) as u16;
                if !write && rt != 31 {
                    held.destination = Some(rt);
                }
                Exit::System {
                    register,
                    write,
                    value: if write && rt != 31 {
                        self.core(rt as u32)?
                    } else {
                        0
                    },
                }
            }
            _ => {
                return Err(format!(
                    "PV unsupported HVF exception {syndrome:#x} at {pc:#x}"
                ))
            }
        };
        self.held = Some(held);
        Ok(exit)
    }
    fn canonical_pending_io(&self) -> Result<Vec<u8>> {
        if self.held.is_some() {
            return Err("native witness before IO completion".into());
        }
        Ok(vec![0]) // Explicit no owed operation, not a default/unsupported witness.
    }
    fn complete(&mut self, response: Option<u64>) -> Result<()> {
        let held = self.held.take().ok_or("no held HVF instruction")?;
        if let Some(register) = held.destination {
            let mut value = response.ok_or("missing HVF read response")?;
            if held.signed {
                let shift = 64 - held.width as u32 * 8;
                value = ((value << shift) as i64 >> shift) as u64;
            }
            if !held.wide {
                value &= u32::MAX as u64;
            }
            self.set_core(register as u32, value)?;
        }
        if !held.call {
            self.set_core(32, held.pc.checked_add(4).ok_or("PV PC overflow")?)?;
        }
        Ok(())
    }
    fn irq(&mut self, number: u32, level: bool) -> Result<()> {
        unsafe {
            let set: unsafe extern "C" fn(u32, bool) -> i32 =
                std::mem::transmute(api(c"hv_gic_set_spi")?);
            checked("HVF SPI", set(number, level))
        }
    }
    fn stopped_parts(&mut self) -> Result<Vec<(String, String)>> {
        if self.held.is_some() || self.poisoned {
            return Err("HVF evidence needs healthy completed stop".into());
        }
        let Registers { core, system, simd } = self.cpu_inventory()?;
        let cpu = arm::hvf_cpu(core, system, simd)?;
        let mut gic = unsafe {
            let dist: unsafe extern "C" fn(u16, *mut u64) -> i32 =
                std::mem::transmute(api(c"hv_gic_get_distributor_reg")?);
            // Unlike distributor offsets, redistributor offsets exceed u16.
            let redist: unsafe extern "C" fn(u64, u32, *mut u64) -> i32 =
                std::mem::transmute(api(c"hv_gic_get_redistributor_reg")?);
            arm::gic(
                |offset, _wide| {
                    let mut value = 0;
                    checked(
                        "evidence HVF GIC distributor",
                        dist(offset as u16, &mut value),
                    )?;
                    Ok(value)
                },
                |offset, _wide| {
                    let mut value = 0;
                    checked(
                        "evidence HVF GIC redistributor",
                        redist(self.cpu()?, offset, &mut value),
                    )?;
                    Ok(value)
                },
            )?
        };
        gic.icc = self.icc_inventory()?;
        arm::parts(&cpu, &gic)
    }
    fn capture(&self) -> Result<Architecture> {
        if self.held.is_some() || self.poisoned {
            return Err("HVF capture needs healthy completed stop".into());
        }
        self.inventory()
    }
    fn restore(&mut self, state: &Architecture) -> Result<()> {
        if self.entered || self.held.is_some() || self.poisoned {
            return Err("HVF restore needs fresh VM".into());
        }
        let initial = self.inventory()?;
        if state.core.len() != 35
            || state.simd.len() != 32
            || state.system.keys().ne(initial.system.keys())
            || state.icc.keys().ne(initial.icc.keys())
            || state.gic.is_empty()
            || state.gic.len() > 4 << 20
        {
            return Err("PV HVF snapshot inventory differs".into());
        }
        let immutable = |register: u16| register == 0xc000 || (0xc020..=0xc03a).contains(&register);
        for (&register, &value) in &state.system {
            if immutable(register) && initial.system[&register] != value {
                return Err("PV HVF snapshot architecture differs".into());
            }
        }
        self.poisoned = true;
        unsafe {
            let set: unsafe extern "C" fn(*const c_void, usize) -> i32 =
                std::mem::transmute(api(c"hv_gic_set_state")?);
            checked(
                "restore HVF GIC",
                set(state.gic.as_ptr().cast(), state.gic.len()),
            )?;
            for (&register, &value) in &state.system {
                if !immutable(register) && initial.system[&register] != value {
                    self.set_system(register, value)?;
                }
            }
            for (register, value) in state.simd.iter().enumerate() {
                checked(
                    "restore HVF vector",
                    seedbox_pv_set_vector(self.cpu()?, register as u32, value.as_ptr()),
                )?;
            }
            for (register, &value) in state.core.iter().enumerate() {
                self.raw_set(register as u32, value)?;
            }
            let set_icc: unsafe extern "C" fn(u64, u16, u64) -> i32 =
                std::mem::transmute(api(c"hv_gic_set_icc_reg")?);
            for (&register, &value) in &state.icc {
                if register != 0xc65b {
                    checked("restore HVF ICC", set_icc(self.cpu()?, register, value))?;
                }
            }
        }
        if self.inventory()? != *state {
            return Err("PV HVF restored state readback differs".into());
        }
        self.poisoned = false;
        Ok(())
    }
}
impl Drop for Native {
    fn drop(&mut self) {
        unsafe {
            if let Some(cpu) = self.cpu.take() {
                let code = hv_vcpu_destroy(cpu);
                if code != 0 {
                    eprintln!("PV HVF vCPU destroy {code:#x}");
                }
            }
            let code = hv_vm_destroy();
            if code != 0 {
                eprintln!("PV HVF VM destroy {code:#x}");
            }
        }
    }
}
