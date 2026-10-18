//! Bounded stopped HVF/GIC readiness; never loads or enters a guest.
use crate::{store::ContentDigest, Error, Result};
use std::{
    collections::BTreeMap,
    ffi::{c_void, CStr},
};

#[link(name = "Hypervisor", kind = "framework")]
unsafe extern "C" {
    fn hv_vm_create(config: *mut c_void) -> i32;
    fn hv_vm_destroy() -> i32;
    fn hv_vcpu_create(cpu: *mut u64, exit: *mut *const c_void, config: *mut c_void) -> i32;
    fn hv_vcpu_destroy(cpu: u64) -> i32;
    fn hv_vcpu_get_sys_reg(cpu: u64, register: u16, value: *mut u64) -> i32;
    fn os_release(pointer: *mut c_void);
}
fn checked(operation: &str, code: i32) -> Result<()> {
    if code == 0 {
        Ok(())
    } else {
        Err(Error::Setup(format!(
            "{operation}: HVF {:#x}; signed hypervisor entitlement and macOS 15 GIC required",
            code as u32
        )))
    }
}
unsafe fn api(name: &CStr) -> Result<*mut c_void> {
    let pointer = libc::dlsym(libc::RTLD_DEFAULT, name.as_ptr());
    if pointer.is_null() {
        Err(Error::Setup(format!(
            "HVF API {} unavailable",
            name.to_string_lossy()
        )))
    } else {
        Ok(pointer)
    }
}
struct Probe {
    vm: bool,
    cpu: Option<u64>,
}
impl Probe {
    fn close(&mut self) -> Result<()> {
        if let Some(cpu) = self.cpu {
            checked("HVF readiness vCPU destroy", unsafe {
                hv_vcpu_destroy(cpu)
            })?;
            self.cpu = None;
        }
        if self.vm {
            checked("HVF readiness VM destroy", unsafe { hv_vm_destroy() })?;
            self.vm = false;
        }
        Ok(())
    }
}
impl Drop for Probe {
    fn drop(&mut self) {
        // The explicit close reports cleanup failures to the caller. Retry on
        // unwind/failed close without printing from the library boundary.
        let _ = self.close();
    }
}
// Hypervisor.framework permits one VM in this process. The ordinary Driver
// creates VMs only in isolated workers; concurrent readiness calls serialize here.
static PROBE: std::sync::Mutex<()> = std::sync::Mutex::new(());
pub(super) fn observe() -> Result<BTreeMap<String, String>> {
    let _lock = PROBE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    checked("HVF readiness VM create", unsafe {
        hv_vm_create(std::ptr::null_mut())
    })?;
    let mut probe = Probe {
        vm: true,
        cpu: None,
    };
    let observations = (|| {
        // Resolve the same macOS 15 GIC APIs used by the native PV leaf. Older
        // HVF worlds without GIC support are not an alternative execution path.
        unsafe {
            let create: unsafe extern "C" fn() -> *mut c_void =
                std::mem::transmute(api(c"hv_gic_config_create")?);
            let distributor: unsafe extern "C" fn(*mut c_void, u64) -> i32 =
                std::mem::transmute(api(c"hv_gic_config_set_distributor_base")?);
            let redistributor: unsafe extern "C" fn(*mut c_void, u64) -> i32 =
                std::mem::transmute(api(c"hv_gic_config_set_redistributor_base")?);
            let apply: unsafe extern "C" fn(*mut c_void) -> i32 =
                std::mem::transmute(api(c"hv_gic_create")?);
            let config = create();
            if config.is_null() {
                return Err(Error::Setup("HVF GIC config absent".into()));
            }
            let result = (|| {
                checked(
                    "HVF readiness GIC distributor",
                    distributor(config, 0x0800_0000),
                )?;
                checked(
                    "HVF readiness GIC redistributor",
                    redistributor(config, 0x080a_0000),
                )?;
                checked("HVF readiness GIC create", apply(config))
            })();
            os_release(config);
            result?;
            let mut cpu = 0;
            let mut exit = std::ptr::null();
            checked(
                "HVF readiness vCPU create",
                hv_vcpu_create(&mut cpu, &mut exit, std::ptr::null_mut()),
            )?;
            probe.cpu = Some(cpu);
            let mut features = BTreeMap::new();
            for register in [
                0xc000, 0xc020, 0xc021, 0xc028, 0xc029, 0xc030, 0xc031, 0xc038, 0xc039, 0xc03a,
            ] {
                let mut value = 0;
                checked(
                    "HVF readiness feature register",
                    hv_vcpu_get_sys_reg(cpu, register, &mut value),
                )?;
                features.insert(format!("{register:04x}"), value);
            }
            // State support is required by the actual PV backend, not inferred
            // from successful VM creation alone. No snapshot bytes are retained.
            api(c"hv_gic_state_create")?;
            api(c"hv_gic_state_get_size")?;
            api(c"hv_gic_state_get_data")?;
            api(c"hv_gic_set_state")?;
            let mut uname: libc::utsname = std::mem::zeroed();
            if libc::uname(&mut uname) != 0 {
                return Err(Error::Setup(format!(
                    "HVF host inventory: {}",
                    std::io::Error::last_os_error()
                )));
            }
            let release = CStr::from_ptr(uname.release.as_ptr())
                .to_str()
                .map_err(|_| Error::Setup("HVF host release is not UTF-8".into()))?;
            Ok(BTreeMap::from([
                ("hvf_host_release".into(), release.into()),
                (
                    "hvf_cpu_features".into(),
                    ContentDigest::of(&serde_json::to_vec(&features)?)
                        .hex()
                        .into(),
                ),
            ]))
        }
    })();
    // A successful report is published only after checked native cleanup.
    let cleanup = probe.close();
    match (observations, cleanup) {
        (Ok(observations), Ok(())) => Ok(observations),
        (Err(error), Ok(())) | (Ok(_), Err(error)) => Err(error),
        (Err(error), Err(cleanup)) => {
            Err(Error::Setup(format!("{error}; cleanup failed: {cleanup}")))
        }
    }
}
