//! Linux aarch64 KVM + in-kernel GICv3. No PMU or host-time policy.
use super::*;
use crate::{GIC_DIST, GIC_REDIST};
use kvm_bindings::*;
use kvm_ioctls::{DeviceFd, Kvm as KvmApi, VcpuExit, VcpuFd, VmFd};
use std::collections::BTreeMap;

const CORE: u64 = 0x6030_0000_0010_0000;
const SYS: u64 = 0x6030_0000_0013_0000;
fn core_id(reg: CoreReg) -> Result<u64> {
    // user_pt_regs.sp is SP_EL0; kvm_regs.sp_el1 is the following word.
    let index = if matches!(reg, CoreReg::Sp) {
        34
    } else {
        core_index(reg)?
    };
    Ok(CORE | (index as u64 * 2))
}
fn sys_id(reg: SysReg) -> u64 {
    match reg.0 {
        0xe208 => CORE | (34 * 2), // SP_EL1
        0xc208 => CORE | (31 * 2), // SP_EL0
        0xc201 => CORE | (35 * 2), // ELR_EL1
        0xc200 => CORE | (36 * 2), // SPSR_EL1
        // Linux UAPI accidentally swapped these encodings; preserve its ABI.
        0xdf1a => SYS | 0xdf02, // CNTV_CVAL_EL0
        0xdf02 => SYS | 0xdf1a, // CNTVCT_EL0
        encoding => SYS | encoding as u64,
    }
}
// fd fields must be dropped before memory; vCPU/device fds retain the VM.
pub struct Kvm {
    cpu: Option<VcpuFd>,
    gic: DeviceFd,
    vm: VmFd,
    mappings: BTreeMap<u32, (u64, Rc<Memory>)>,
    pending: Option<Exit>,
}
fn context<T>(op: &str, r: std::result::Result<T, kvm_ioctls::Error>) -> Result<T> {
    r.map_err(|e| format!("{op}: {e}"))
}
impl Kvm {
    fn cpu(&self) -> Result<&VcpuFd> {
        self.cpu.as_ref().ok_or("KVM vCPU not created".into())
    }
    fn cpu_mut(&mut self) -> Result<&mut VcpuFd> {
        self.cpu.as_mut().ok_or("KVM vCPU not created".into())
    }
    fn get(&self, id: u64) -> Result<u64> {
        let mut b = [0; 8];
        context("KVM_GET_ONE_REG", self.cpu()?.get_one_reg(id, &mut b))?;
        Ok(u64::from_le_bytes(b))
    }
    fn set(&self, id: u64, value: u64) -> Result<()> {
        context(
            "KVM_SET_ONE_REG",
            self.cpu()?.set_one_reg(id, &value.to_le_bytes()),
        )?;
        Ok(())
    }
}
impl Backend for Kvm {
    fn create() -> Result<Self> {
        let kvm = context(
            "open /dev/kvm (requires Linux arm64 and KVM access)",
            KvmApi::new(),
        )?;
        let vm = context("KVM_CREATE_VM", kvm.create_vm())?;
        // Mandatory: otherwise PSCI calls disappear inside the host kernel and
        // do not share the HVF exit/time/witness semantics. Available since 6.4.
        #[repr(C)]
        struct Filter {
            base: u32,
            count: u32,
            action: u8,
            pad: [u8; 15],
        }
        for base in [0x8400_0000, 0xc400_0000] {
            let filter = Filter {
                base,
                count: 32,
                action: 2,
                pad: [0; 15],
            };
            context(
                "KVM_ARM_VM_SMCCC_FILTER (PSCI forwarding required)",
                vm.set_device_attr(&kvm_device_attr {
                    group: KVM_ARM_VM_SMCCC_CTRL,
                    attr: KVM_ARM_VM_SMCCC_FILTER as u64,
                    addr: &filter as *const _ as u64,
                    flags: 0,
                }),
            )?;
        }
        let mut dev = kvm_create_device {
            type_: kvm_device_type_KVM_DEV_TYPE_ARM_VGIC_V3,
            fd: 0,
            flags: 0,
        };
        let gic = context("KVM_CREATE_DEVICE VGIC_V3", vm.create_device(&mut dev))?;
        for (attr, value) in [
            (KVM_VGIC_V3_ADDR_TYPE_DIST, GIC_DIST),
            (KVM_VGIC_V3_ADDR_TYPE_REDIST, GIC_REDIST),
        ] {
            context(
                "VGIC address",
                gic.set_device_attr(&kvm_device_attr {
                    group: KVM_DEV_ARM_VGIC_GRP_ADDR,
                    attr: attr as u64,
                    addr: &value as *const _ as u64,
                    flags: 0,
                }),
            )?;
        }
        let mut nr_irqs = crate::GIC_NR_IRQS;
        let mut attr = kvm_device_attr {
            group: KVM_DEV_ARM_VGIC_GRP_NR_IRQS,
            attr: 0,
            addr: &mut nr_irqs as *mut u32 as u64,
            flags: 0,
        };
        context("set VGIC NR_IRQS", gic.set_device_attr(&attr))?;
        nr_irqs = 0;
        context("read VGIC NR_IRQS", unsafe {
            gic.get_device_attr(&mut attr)
        })?;
        if nr_irqs != crate::GIC_NR_IRQS {
            return Err(format!(
                "VGIC NR_IRQS readback: expected {}, got {nr_irqs}",
                crate::GIC_NR_IRQS
            ));
        }
        Ok(Self {
            cpu: None,
            gic,
            vm,
            mappings: BTreeMap::new(),
            pending: None,
        })
    }
    fn map_memory(&mut self, slot: u32, ipa: u64, memory: Rc<Memory>) -> Result<()> {
        if self.mappings.contains_key(&slot) {
            return Err("memory slot already mapped".into());
        }
        unsafe {
            context(
                "KVM_SET_USER_MEMORY_REGION",
                self.vm.set_user_memory_region(kvm_userspace_memory_region {
                    slot,
                    flags: KVM_MEM_LOG_DIRTY_PAGES,
                    guest_phys_addr: ipa,
                    memory_size: memory.len() as u64,
                    userspace_addr: memory.as_ptr() as u64,
                }),
            )?;
        }
        self.mappings.insert(slot, (ipa, memory));
        Ok(())
    }
    fn unmap_memory(&mut self, slot: u32) -> Result<()> {
        let (ipa, _) = self.mappings.get(&slot).ok_or("unknown memory slot")?;
        unsafe {
            context(
                "unmap KVM slot",
                self.vm.set_user_memory_region(kvm_userspace_memory_region {
                    slot,
                    guest_phys_addr: *ipa,
                    ..Default::default()
                }),
            )?;
        }
        self.mappings.remove(&slot);
        Ok(())
    }
    fn create_vcpu(&mut self) -> Result<()> {
        if self.cpu.is_some() {
            return Err("only one vCPU supported".into());
        }
        let cpu = context("KVM_CREATE_VCPU", self.vm.create_vcpu(0))?;
        let mut init = kvm_vcpu_init::default();
        context(
            "KVM_ARM_PREFERRED_TARGET",
            self.vm.get_preferred_target(&mut init),
        )?;
        init.features[0] |= 1 << KVM_ARM_VCPU_PSCI_0_2;
        context("KVM_ARM_VCPU_INIT", cpu.vcpu_init(&init))?;
        self.cpu = Some(cpu);
        context(
            "VGIC_CTRL_INIT",
            self.gic.set_device_attr(&kvm_device_attr {
                group: KVM_DEV_ARM_VGIC_GRP_CTRL,
                attr: KVM_DEV_ARM_VGIC_CTRL_INIT as u64,
                ..Default::default()
            }),
        )?;
        Ok(())
    }
    fn get_reg(&self, reg: CoreReg) -> Result<u64> {
        self.get(core_id(reg)?)
    }
    fn set_reg(&mut self, reg: CoreReg, value: u64) -> Result<()> {
        self.set(core_id(reg)?, value)
    }
    fn get_sys_reg(&self, reg: SysReg) -> Result<u64> {
        self.get(sys_id(reg))
    }
    fn set_sys_reg(&mut self, reg: SysReg, value: u64) -> Result<()> {
        self.set(sys_id(reg), value)
    }
    fn get_gic_dist(&self, offset: u16) -> Result<u64> {
        let mut value = 0u32;
        context(
            "read VGIC distributor",
            // The stopped device writes exactly one u32 into live stack storage.
            unsafe {
                self.gic.get_device_attr(&mut kvm_device_attr {
                    group: KVM_DEV_ARM_VGIC_GRP_DIST_REGS,
                    attr: offset as u64,
                    addr: &mut value as *mut u32 as u64,
                    flags: 0,
                })
            },
        )?;
        Ok(value as u64)
    }
    fn run(&mut self) -> Result<Stop> {
        if self.pending.is_some() {
            return Err("run before completing held KVM exit".into());
        }
        let exit = loop {
            match self.cpu_mut()?.run() {
                Ok(VcpuExit::MmioRead(addr, b)) => {
                    break Exit::Mmio {
                        addr,
                        len: b.len() as u8,
                        write: false,
                        data: 0,
                        reg: None,
                    }
                }
                Ok(VcpuExit::MmioWrite(addr, b)) => {
                    let mut data = [0; 8];
                    if b.len() > 8 {
                        return Err("KVM MMIO exceeds 8 bytes".into());
                    }
                    data[..b.len()].copy_from_slice(b);
                    break Exit::Mmio {
                        addr,
                        len: b.len() as u8,
                        write: true,
                        data: u64::from_le_bytes(data),
                        reg: None,
                    };
                }
                Ok(VcpuExit::Hypercall(h)) => {
                    // On arm64 this union member contains SMCCC flags: bit 0
                    // means SMC, bit 1 means a 16-bit instruction. Our ABI is HVC64.
                    if h.longmode != 0 {
                        return Err(format!("unexpected KVM SMCCC flags {:#x}", h.longmode));
                    }
                    break Exit::Hvc { imm: 0 };
                }
                Ok(VcpuExit::Intr) => continue,
                Err(e) if e.errno() == libc::EINTR => continue,
                Err(e) => return Err(format!("KVM_RUN: {e}")),
                Ok(other) => return Err(format!("unexpected KVM exit {other:?}")),
            }
        };
        let mut pc = self.get_reg(CoreReg::Pc)?;
        if matches!(exit, Exit::Hvc { .. }) {
            pc = pc.checked_sub(4).ok_or("invalid HVC PC")?;
        }
        self.pending = Some(exit.clone());
        Ok(Stop { pc, exit })
    }
    fn complete(&mut self, read: Option<u64>) -> Result<()> {
        let exit = self.pending.take().ok_or("no KVM exit to complete")?;
        if let Exit::Mmio {
            write: false, len, ..
        } = exit
        {
            let b = read.ok_or("missing MMIO read response")?.to_le_bytes();
            // kvm_run remains owned by this vCPU. The prior run's slice borrow
            // has ended; the next KVM_RUN consumes exactly these response bytes.
            unsafe {
                self.cpu_mut()?.get_kvm_run().__bindgen_anon_1.mmio.data[..len as usize]
                    .copy_from_slice(&b[..len as usize]);
            }
        }
        // arm64 forwarded hypercalls use SET_ONE_REG, not hypercall.ret.
        Ok(())
    }
    fn inject_spi(&mut self, intid: u32, level: bool) -> Result<()> {
        context(
            "KVM_IRQ_LINE",
            self.vm.set_irq_line(
                (KVM_ARM_IRQ_TYPE_SPI << KVM_ARM_IRQ_TYPE_SHIFT) | intid,
                level,
            ),
        )
    }
    fn dirty_pages(&self, slot: u32) -> Result<DirtyPages> {
        let (_, m) = self.mappings.get(&slot).ok_or("unknown memory slot")?;
        let words = context("KVM_GET_DIRTY_LOG", self.vm.get_dirty_log(slot, m.len()))?;
        let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
        if page_size <= 0 {
            return Err("sysconf(_SC_PAGESIZE) failed".into());
        }
        Ok(DirtyPages::Bitmap {
            page_size: page_size as usize,
            words,
        })
    }
}
