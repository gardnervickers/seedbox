//! Linux arm64 KVM adapter, with forwarded PSCI and an in-kernel GICv3.
use super::{Architecture, Backend, Exit, Memory, Result};
use crate::machine::pv::worker::evidence::arm;
use kvm_bindings::*;
use kvm_ioctls::{DeviceFd, Kvm, VcpuExit, VcpuFd, VmFd};
use std::{collections::BTreeMap, mem::offset_of, rc::Rc};
pub(crate) struct Native {
    cpu: VcpuFd,
    _gic: DeviceFd,
    vm: VmFd,
    _ram: Rc<Memory>,
    _page: Rc<Memory>,
    held: bool,
}
fn checked<T>(operation: &str, result: std::result::Result<T, kvm_ioctls::Error>) -> Result<T> {
    result.map_err(|e| format!("PV {operation}: {e}"))
}
const CORE: u64 = 0x6030_0000_0010_0000;
const SYSTEM: u64 = 0x6030_0000_0013_0000;
fn system_id(register: u16) -> u64 {
    match register {
        0xe208 => CORE | 68,
        0xc208 => CORE | 62,
        0xc201 => CORE | 70,
        0xc200 => CORE | 72,
        0xdf1a => SYSTEM | 0xdf02,
        0xdf02 => SYSTEM | 0xdf1a,
        value => SYSTEM | value as u64,
    }
}
impl Native {
    fn get(&self, id: u64) -> Result<u64> {
        let mut bytes = [0; 8];
        checked("KVM_GET_ONE_REG", self.cpu.get_one_reg(id, &mut bytes))?;
        Ok(u64::from_le_bytes(bytes))
    }
    fn gic_word(&self, group: u32, attr: u64) -> Result<u32> {
        let mut value = 0u32;
        checked(
            "evidence KVM VGIC field",
            // VGIC register/level groups write exactly one u32 to this live,
            // aligned stack slot; the ioctl completes before it goes out of scope.
            unsafe {
                self._gic.get_device_attr(&mut kvm_device_attr {
                    group,
                    attr,
                    addr: &mut value as *mut _ as u64,
                    flags: 0,
                })
            },
        )?;
        Ok(value)
    }
    fn gic_register(&self, group: u32, mpidr: u64, offset: u32, wide: bool) -> Result<u64> {
        let low = u64::from(self.gic_word(group, mpidr | u64::from(offset))?);
        if wide {
            Ok(low | (u64::from(self.gic_word(group, mpidr | u64::from(offset + 4))?) << 32))
        } else {
            Ok(low)
        }
    }
    fn icc_register(&self, mpidr: u64, register: u16) -> Result<u64> {
        let mut value = 0u64;
        checked(
            "evidence KVM ICC field",
            // The CPU_SYSREGS group writes exactly one u64 to this live slot.
            unsafe {
                self._gic.get_device_attr(&mut kvm_device_attr {
                    group: KVM_DEV_ARM_VGIC_GRP_CPU_SYSREGS,
                    attr: mpidr | u64::from(register),
                    addr: &mut value as *mut _ as u64,
                    flags: 0,
                })
            },
        )?;
        Ok(value)
    }
    fn cpu_inventory(&self) -> Result<arm::Cpu> {
        // Stopped evidence must recheck the same actual-register invariant as
        // Machine::maintenance, not infer disabled timers from boot settings.
        let virtual_timer = self.system(0xdf19)?;
        arm::virtual_timer_disabled(virtual_timer)?;
        // Offsets are UAPI u32 words in kvm_regs, not Rust/host struct bytes.
        let core = (0..31)
            .map(|n| self.get(CORE | n * 2))
            .chain([68, 64, 66, 62].into_iter().map(|n| self.get(CORE | n)))
            .collect::<Result<Vec<_>>>()?;
        let mut simd = Vec::new();
        for n in 0..32 {
            let word =
                (offset_of!(kvm_regs, fp_regs) + offset_of!(user_fpsimd_state, vregs) + n * 16) / 4;
            let id = (CORE & !(0xf << 52)) | KVM_REG_SIZE_U128 | word as u64;
            let mut bytes = [0; 16];
            checked("evidence KVM SIMD", self.cpu.get_one_reg(id, &mut bytes))?;
            simd.push(bytes);
        }
        // Enumerate the actually exposed system bank, then explicitly project
        // architectural IDs/values. No kernel padding, handles or migration data.
        // The bounded allocation fails explicitly if the native ABI grows past it.
        let mut list =
            RegList::new(4096).map_err(|e| format!("PV evidence KVM register inventory: {e}"))?;
        checked(
            "evidence KVM register inventory",
            self.cpu.get_reg_list(&mut list),
        )?;
        let mut system = BTreeMap::new();
        for &id in list.as_slice() {
            if id & !0xffff == SYSTEM {
                // KVM's historical TIMER_CNT/CVAL encodings are swapped:
                // df1a is the live virtual counter, df02 the stored deadline.
                // Physical/virtual TVAL are also host-counter derived. Never
                // read them, even with timers disabled by the PV guest profile.
                if arm::kvm_system_field(id as u16) && id as u16 != 0xdf19 {
                    system.insert(id as u16, self.get(id)?);
                }
            }
        }
        // Retain exactly the control value whose ENABLE bit was verified.
        system.insert(0xdf19, virtual_timer);
        system.insert(
            0xc201,
            self.get(CORE | (offset_of!(kvm_regs, elr_el1) / 4) as u64)?,
        );
        system.insert(
            0xc200,
            self.get(CORE | (offset_of!(kvm_regs, spsr) / 4) as u64)?,
        );
        for (register, offset) in [
            (0xda20, offset_of!(user_fpsimd_state, fpcr)),
            (0xda21, offset_of!(user_fpsimd_state, fpsr)),
        ] {
            let id = (CORE & !(0xf << 52))
                | KVM_REG_SIZE_U32
                | ((offset_of!(kvm_regs, fp_regs) + offset) / 4) as u64;
            let mut bytes = [0; 4];
            checked(
                "evidence KVM FP control",
                self.cpu.get_one_reg(id, &mut bytes),
            )?;
            system.insert(register, u64::from(u32::from_le_bytes(bytes)));
        }
        let saved_psrs = (0..5)
            .map(|n| self.get(CORE | ((offset_of!(kvm_regs, spsr) + n * 8) / 4) as u64))
            .collect::<Result<Vec<_>>>()?;
        Ok(arm::Cpu {
            core,
            system,
            simd,
            saved_psrs,
        })
    }
    fn set(&self, id: u64, value: u64) -> Result<()> {
        checked(
            "KVM_SET_ONE_REG",
            self.cpu.set_one_reg(id, &value.to_le_bytes()),
        )?;
        Ok(())
    }
}
impl Backend for Native {
    fn create(ram: Rc<Memory>, page: Rc<Memory>) -> Result<Self> {
        let kvm = checked("open /dev/kvm", Kvm::new())?;
        let vm = checked("KVM_CREATE_VM", kvm.create_vm())?;
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
            checked(
                "forward PSCI",
                vm.set_device_attr(&kvm_device_attr {
                    group: KVM_ARM_VM_SMCCC_CTRL,
                    attr: KVM_ARM_VM_SMCCC_FILTER as u64,
                    addr: &filter as *const _ as u64,
                    flags: 0,
                }),
            )?;
        }
        let mut device = kvm_create_device {
            type_: kvm_device_type_KVM_DEV_TYPE_ARM_VGIC_V3,
            fd: 0,
            flags: 0,
        };
        let gic = checked("create VGICv3", vm.create_device(&mut device))?;
        for (kind, address) in [
            (KVM_VGIC_V3_ADDR_TYPE_DIST, 0x0800_0000u64),
            (KVM_VGIC_V3_ADDR_TYPE_REDIST, 0x080a_0000u64),
        ] {
            checked(
                "VGIC address",
                gic.set_device_attr(&kvm_device_attr {
                    group: KVM_DEV_ARM_VGIC_GRP_ADDR,
                    attr: kind as u64,
                    addr: &address as *const _ as u64,
                    flags: 0,
                }),
            )?;
        }
        let count = 992u32;
        checked(
            "VGIC interrupt count",
            gic.set_device_attr(&kvm_device_attr {
                group: KVM_DEV_ARM_VGIC_GRP_NR_IRQS,
                attr: 0,
                addr: &count as *const _ as u64,
                flags: 0,
            }),
        )?;
        for (slot, address, memory) in [(0, 0x4000_0000, &ram), (1, 0x0a00_0000, &page)] {
            unsafe {
                checked(
                    "map guest memory",
                    vm.set_user_memory_region(kvm_userspace_memory_region {
                        slot,
                        flags: 0,
                        guest_phys_addr: address,
                        memory_size: memory.len() as u64,
                        userspace_addr: memory.ptr() as u64,
                    }),
                )?;
            }
        }
        let cpu = checked("KVM_CREATE_VCPU", vm.create_vcpu(0))?;
        let mut initial = kvm_vcpu_init::default();
        checked(
            "KVM preferred target",
            vm.get_preferred_target(&mut initial),
        )?;
        initial.features[0] |= 1 << KVM_ARM_VCPU_PSCI_0_2;
        checked("KVM vCPU init", cpu.vcpu_init(&initial))?;
        checked(
            "VGIC init",
            gic.set_device_attr(&kvm_device_attr {
                group: KVM_DEV_ARM_VGIC_GRP_CTRL,
                attr: KVM_DEV_ARM_VGIC_CTRL_INIT as u64,
                ..Default::default()
            }),
        )?;
        Ok(Self {
            cpu,
            _gic: gic,
            vm,
            _ram: ram,
            _page: page,
            held: false,
        })
    }
    fn core(&self, register: u32) -> Result<u64> {
        if register > 33 {
            return Err("PV invalid core register".into());
        }
        self.get(
            CORE | if register == 31 {
                68
            } else {
                register as u64 * 2
            },
        )
    }
    fn set_core(&mut self, register: u32, value: u64) -> Result<()> {
        if register > 33 {
            return Err("PV invalid core register".into());
        }
        self.set(
            CORE | if register == 31 {
                68
            } else {
                register as u64 * 2
            },
            value,
        )
    }
    fn system(&self, register: u16) -> Result<u64> {
        self.get(system_id(register))
    }
    fn set_system(&mut self, register: u16, value: u64) -> Result<()> {
        self.set(system_id(register), value)
    }
    fn enter(&mut self) -> Result<Exit> {
        if self.held {
            return Err("PV KVM entry before completion".into());
        }
        let exit = match self.cpu.run() {
            Ok(VcpuExit::MmioRead(address, bytes)) => Exit::Mmio {
                address,
                width: bytes.len() as u8,
                write: false,
                value: 0,
            },
            Ok(VcpuExit::MmioWrite(address, bytes)) => {
                if bytes.len() > 8 {
                    return Err("PV KVM oversized MMIO".into());
                }
                let mut value = [0; 8];
                value[..bytes.len()].copy_from_slice(bytes);
                Exit::Mmio {
                    address,
                    width: bytes.len() as u8,
                    write: true,
                    value: u64::from_le_bytes(value),
                }
            }
            Ok(VcpuExit::Hypercall(call)) => {
                if call.longmode != 0 {
                    return Err("PV KVM requires HVC64".into());
                }
                Exit::Call
            }
            Ok(VcpuExit::Intr) => return Ok(Exit::Interrupted),
            Err(e) if e.errno() == libc::EINTR => return Ok(Exit::Interrupted),
            Err(e) => return Err(format!("PV KVM_RUN: {e}")),
            Ok(exit) => return Err(format!("PV unexpected KVM exit {exit:?}")),
        };
        self.held = true;
        Ok(exit)
    }
    fn canonical_pending_io(&self) -> Result<Vec<u8>> {
        if self.held {
            return Err("native witness before IO completion".into());
        }
        Ok(vec![0]) // Explicit no owed operation, not a default/unsupported witness.
    }
    fn complete(&mut self, response: Option<u64>) -> Result<()> {
        if !self.held {
            return Err("PV no held KVM exit".into());
        }
        if let Some(value) = response {
            unsafe {
                let mmio = &mut self.cpu.get_kvm_run().__bindgen_anon_1.mmio;
                let length = mmio.len as usize;
                if length > 8 {
                    return Err("PV KVM response width".into());
                }
                mmio.data[..length].copy_from_slice(&value.to_le_bytes()[..length]);
            }
        }
        self.held = false;
        Ok(())
    }
    fn irq(&mut self, number: u32, level: bool) -> Result<()> {
        checked(
            "KVM IRQ",
            self.vm.set_irq_line(
                (KVM_ARM_IRQ_TYPE_SPI << KVM_ARM_IRQ_TYPE_SHIFT) | number,
                level,
            ),
        )
    }
    fn stopped_parts(&mut self) -> Result<Vec<(String, String)>> {
        if self.held {
            return Err("PV KVM evidence needs a completed stop".into());
        }
        let cpu = self.cpu_inventory()?;
        let affinity = self.system(0xc005)?;
        let mpidr = ((affinity & 0xff_0000_0000) >> 8 | (affinity & 0xff_ffff)) << 32;
        let mut gic = arm::gic(
            |offset, wide| self.gic_register(KVM_DEV_ARM_VGIC_GRP_DIST_REGS, 0, offset, wide),
            |offset, wide| self.gic_register(KVM_DEV_ARM_VGIC_GRP_REDIST_REGS, mpidr, offset, wide),
        )?;
        gic.kvm_redist(
            self.gic_register(KVM_DEV_ARM_VGIC_GRP_REDIST_REGS, mpidr, 0, false)?,
            self.gic_register(KVM_DEV_ARM_VGIC_GRP_REDIST_REGS, mpidr, 0x14, false)?,
        );
        // ICC_RPR is derived and is not exposed by the KVM device ABI. Read all
        // actually implemented active-priority banks, not unsupported APR slots.
        for register in arm::ICC.into_iter().filter(|r| *r != 0xc65b) {
            gic.icc
                .insert(register, self.icc_register(mpidr, register)?);
        }
        let aprs = 1 << (((gic.icc[&0xc664] >> 8) & 7).saturating_sub(4).min(2));
        for n in 1..aprs {
            for register in [0xc644 + n as u16, 0xc648 + n as u16] {
                gic.icc
                    .insert(register, self.icc_register(mpidr, register)?);
            }
        }
        let count = arm::interrupt_count(self.gic_register(
            KVM_DEV_ARM_VGIC_GRP_DIST_REGS,
            0,
            4,
            false,
        )?)?;
        gic.line_levels = Some(
            (0..count)
                .step_by(32)
                .map(|irq| {
                    Ok((
                        irq,
                        self.gic_word(KVM_DEV_ARM_VGIC_GRP_LEVEL_INFO, mpidr | u64::from(irq))?,
                    ))
                })
                .collect::<Result<BTreeMap<_, _>>>()?,
        );
        arm::parts(&cpu, &gic)
    }
    fn capture(&self) -> Result<Architecture> {
        Err("PV KVM architectural snapshot is not implemented".into())
    }
    fn restore(&mut self, _state: &Architecture) -> Result<()> {
        Err("PV KVM architectural restore is not implemented".into())
    }
}
