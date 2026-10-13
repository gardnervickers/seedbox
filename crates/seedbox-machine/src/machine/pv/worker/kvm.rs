//! Linux arm64 KVM adapter, with forwarded PSCI and an in-kernel GICv3.
use super::{Architecture, Backend, Exit, Memory, Result};
use kvm_bindings::*;
use kvm_ioctls::{DeviceFd, Kvm, VcpuExit, VcpuFd, VmFd};
use std::rc::Rc;
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
    fn capture(&self) -> Result<Architecture> {
        Err("PV KVM architectural snapshot is not implemented".into())
    }
    fn restore(&mut self, _state: &Architecture) -> Result<()> {
        Err("PV KVM architectural restore is not implemented".into())
    }
}
