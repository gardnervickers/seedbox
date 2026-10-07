use seedbox_arm64::{backend::*, machine::Machine, memory::Memory, *};
use std::{collections::BTreeMap, rc::Rc};
struct Fake<
    const DEBUG_VALUE: u64 = 0,
    const COUNTER_CONTROL: u64 = 0,
    const TIMER_CONTROL: u64 = 0,
    const MASKED_IDLE: bool = false,
    const HIDE_DEBUG: bool = false,
    const DEBUG_READ: bool = false,
    const UNKNOWN_DEBUG: bool = false,
> {
    step: usize,
    regs: [u64; 34],
    mapped: BTreeMap<u32, Rc<Memory>>,
    held: bool,
    irq: bool,
    mmfr0: u64,
    mpidr: u64,
    dfr0: u64,
    ignore_feature_write: bool,
    reject_feature_write: bool,
}
impl<
        const D: u64,
        const C: u64,
        const T: u64,
        const I: bool,
        const H: bool,
        const R: bool,
        const U: bool,
    > Backend for Fake<D, C, T, I, H, R, U>
{
    fn create() -> Result<Self> {
        Ok(Self {
            step: 0,
            mpidr: 0,
            dfr0: 0x10305606,
            regs: [0; 34],
            mapped: BTreeMap::new(),
            held: false,
            irq: false,
            mmfr0: 0x0100_1000_0000_0023,
            ignore_feature_write: false,
            reject_feature_write: false,
        })
    }
    fn map_memory(&mut self, slot: u32, _: u64, m: Rc<Memory>) -> Result<()> {
        self.mapped.insert(slot, m);
        Ok(())
    }
    fn unmap_memory(&mut self, slot: u32) -> Result<()> {
        self.mapped.remove(&slot);
        Ok(())
    }
    fn create_vcpu(&mut self) -> Result<()> {
        Ok(())
    }
    fn get_reg(&self, r: CoreReg) -> Result<u64> {
        Ok(self.regs[match r {
            CoreReg::X(n) => n as usize,
            CoreReg::Pc => 32,
            CoreReg::Sp => 31,
            CoreReg::Pstate => 33,
        }])
    }
    fn set_reg(&mut self, r: CoreReg, v: u64) -> Result<()> {
        self.regs[match r {
            CoreReg::X(n) => n as usize,
            CoreReg::Pc => 32,
            CoreReg::Sp => 31,
            CoreReg::Pstate => 33,
        }] = v;
        Ok(())
    }
    fn get_sys_reg(&self, reg: SysReg) -> Result<u64> {
        Ok(if reg == SysReg::CNTKCTL {
            C
        } else if reg == SysReg::CNTV_CTL {
            T
        } else if reg == SysReg::ID_AA64MMFR0 {
            self.mmfr0
        } else if reg == SysReg::MPIDR {
            self.mpidr
        } else if reg == SysReg::ID_AA64DFR0 {
            self.dfr0
        } else {
            0
        })
    }
    fn set_sys_reg(&mut self, reg: SysReg, value: u64) -> Result<()> {
        if matches!(reg, SysReg::MPIDR | SysReg::ID_AA64DFR0) {
            if self.reject_feature_write {
                return Err("feature register write rejected".into());
            }
            if !self.ignore_feature_write {
                if reg == SysReg::MPIDR {
                    self.mpidr = value;
                } else {
                    self.dfr0 = value;
                }
            }
        }
        if reg == SysReg::ID_AA64MMFR0 {
            assert_eq!(self.step, 0, "feature policy must precede guest execution");
            if self.reject_feature_write {
                return Err("feature register write rejected".into());
            }
            if !self.ignore_feature_write {
                self.mmfr0 = value;
            }
        }
        Ok(())
    }
    fn run(&mut self) -> Result<Stop> {
        assert_eq!(self.mmfr0, 0x0000_1000_0000_0023);
        assert_eq!(self.mpidr, 0x8000_0000);
        assert_eq!(self.dfr0, 0x10305006);
        assert!(!self.held);
        self.held = true;
        let mmio = |addr, data| Exit::Mmio {
            addr,
            len: 8,
            write: true,
            data,
            reg: None,
        };
        // The guest reaches its deadline immediately before debug unlocks.
        // A backend may expose both writes or execute both internally. Neither
        // may publish last, assert the IRQ, or change the witnessed PC sequence.
        if self.step == 2 {
            unsafe {
                self.mapped[&1].set_word(8, 1_000_000_000)?;
            }
            if H {
                self.step = 4;
            }
        }
        let exit = match self.step {
            0 => {
                unsafe {
                    self.mapped[&1].set_word(8, 7)?;
                }
                mmio(EVENT_ADDR + 32, 1)
            }
            1 => {
                assert_eq!(unsafe { self.mapped[&1].word(0)? }, 7);
                mmio(EVENT_ADDR, 1_000_000_000)
            }
            2 | 3 => Exit::SysReg {
                reg: if U {
                    0x808c
                } else {
                    [SysReg::OSDLR, SysReg::OSLAR][self.step - 2].0
                },
                write: !R,
                rt: 31,
                data: D,
            },
            4 => {
                assert!(!self.irq, "debug completion must not admit a timer");
                assert_eq!(unsafe { self.mapped[&1].word(0)? }, 7);
                if !I {
                    self.regs[33] &= !(1 << 7);
                }
                mmio(EVENT_ADDR + 16, 1)
            }
            5 => {
                assert!(self.irq);
                assert_eq!(unsafe { self.mapped[&1].word(0)? }, 1_000_000_000);
                mmio(EVENT_ADDR + 8, 1)
            }
            n if n - 6 < b"SEEDBOX_INIT_PASS\r\n".len() => {
                assert!(!self.irq);
                Exit::Mmio {
                    addr: UART_ADDR,
                    len: 1,
                    write: true,
                    data: b"SEEDBOX_INIT_PASS\r\n"[n - 6] as u64,
                    reg: None,
                }
            }
            _ => {
                self.regs[0] = 0x84000008;
                Exit::Hvc { imm: 0 }
            }
        };
        self.step += 1;
        Ok(Stop {
            pc: 0x40080000 + self.step as u64 * 4,
            exit,
        })
    }
    fn complete(&mut self, _: Option<u64>) -> Result<()> {
        assert!(self.held);
        self.held = false;
        Ok(())
    }
    fn inject_spi(&mut self, intid: u32, level: bool) -> Result<()> {
        assert!(self.held);
        assert_eq!(intid, TIMER_SPI);
        assert!(!level || self.regs[33] & (1 << 7) == 0);
        self.irq = level;
        Ok(())
    }
    fn dirty_pages(&self, _: u32) -> Result<DirtyPages> {
        Ok(DirtyPages::Unsupported)
    }
}
#[test]
fn machine_reconciles_reads_completes_once_and_injects_while_stopped() {
    let mut kernel = [0; 64];
    kernel[56..60].copy_from_slice(b"ARM\x64");
    kernel[16..24].copy_from_slice(&4096u64.to_le_bytes());
    let a = Machine::<Fake>::new(&kernel, b"cpio", 1)
        .unwrap()
        .run(|| {})
        .unwrap();
    let mut pages = Vec::new();
    let b = Machine::<Fake>::new(&kernel, b"cpio", 1)
        .unwrap()
        .run_with_diagnostics(|| {}, Some(&mut pages))
        .unwrap();
    let diagnostic: serde_json::Value = serde_json::from_slice(&pages).unwrap();
    assert_eq!(diagnostic["witness"], b.witness);
    assert_eq!(diagnostic["ram_hash"], b.ram_hash);
    assert_eq!(
        diagnostic["regions"][0]["sha256"].as_array().unwrap().len(),
        RAM_SIZE / 4096
    );
    assert_eq!(
        diagnostic["regions"][1]["sha256"].as_array().unwrap().len(),
        PAGE_SIZE / 4096
    );
    let c = Machine::<Fake>::new(&kernel, b"cpio", 2)
        .unwrap()
        .run(|| {})
        .unwrap();
    assert_eq!(a.witness, b.witness);
    assert_ne!(a.witness, c.witness);
    assert_eq!(a.virtual_ns, 1_000_000_000);
    assert_eq!(a.trace[0].time, 7);
    assert_eq!(a.trace[2].time, 1_000_000_000);
    let hidden = Machine::<Fake<0, 0, 0, false, true>>::new(&kernel, b"cpio", 1)
        .unwrap()
        .run(|| {})
        .unwrap();
    assert_eq!(a.trace, hidden.trace);
    assert_eq!(a.witness, hidden.witness);
    assert_eq!(a.exits, hidden.exits);
    assert_eq!(a.virtual_ns, hidden.virtual_ns);
    let json = serde_json::to_value(&a).unwrap();
    assert_eq!(json["backend"], "mock");
}

#[test]
fn rejects_debug_locking_and_hardware_time_leaks() {
    let mut kernel = [0; 64];
    kernel[56..60].copy_from_slice(b"ARM\x64");
    kernel[16..24].copy_from_slice(&4096u64.to_le_bytes());
    let debug = Machine::<Fake<1>>::new(&kernel, b"cpio", 1)
        .unwrap()
        .run(|| {})
        .unwrap_err();
    assert!(debug.contains("unsupported exit SysReg"), "{debug}");
    let read = Machine::<Fake<0, 0, 0, false, false, true>>::new(&kernel, b"cpio", 1)
        .unwrap()
        .run(|| {})
        .unwrap_err();
    assert!(read.contains("unsupported exit SysReg"), "{read}");
    let unknown = Machine::<Fake<0, 0, 0, false, false, false, true>>::new(&kernel, b"cpio", 1)
        .unwrap()
        .run(|| {})
        .unwrap_err();
    assert!(unknown.contains("unsupported exit SysReg"), "{unknown}");
    let counter = Machine::<Fake<0, 2>>::new(&kernel, b"cpio", 1)
        .unwrap()
        .run(|| {})
        .unwrap_err();
    assert!(
        counter.contains("CNTKCTL_EL1=0x2 after PV handshake"),
        "{counter}"
    );
    let timer = Machine::<Fake<0, 0, 1>>::new(&kernel, b"cpio", 1)
        .unwrap()
        .run(|| {})
        .unwrap_err();
    assert!(
        timer.contains("architectural virtual timer enabled"),
        "{timer}"
    );
}

#[test]
fn cpu_feature_policy_preserves_other_fields_and_rejects_failed_readback() {
    use seedbox_arm64::machine::configure_cpu_features;
    // All FGT revisions become absent; every other nibble is preserved.
    for fgt in 0..16 {
        let mut backend = Fake::<0>::create().unwrap();
        backend.mmfr0 = 0xa0bc_def0_1234_5678 | (fgt << 56);
        configure_cpu_features(&mut backend).unwrap();
        assert_eq!(backend.mmfr0, 0xa0bc_def0_1234_5678);
    }
    let mut backend = Fake::<0>::create().unwrap();
    backend.ignore_feature_write = true;
    assert!(configure_cpu_features(&mut backend)
        .unwrap_err()
        .contains("feature policy readback"));
    backend.ignore_feature_write = false;
    backend.reject_feature_write = true;
    assert_eq!(
        configure_cpu_features(&mut backend).unwrap_err(),
        "feature register write rejected"
    );
    // An already absent feature must work even with a read-only register.
    backend.mmfr0 &= !(0xf << 56);
    backend.mpidr = 0x8000_0000;
    backend.dfr0 &= !(0xf << 8);
    configure_cpu_features(&mut backend).unwrap();
}

#[test]
fn topology_and_pmu_require_exact_readback() {
    use seedbox_arm64::machine::configure_cpu_features;
    for reg in [SysReg::MPIDR, SysReg::ID_AA64DFR0] {
        let mut backend = Fake::<0>::create().unwrap();
        configure_cpu_features(&mut backend).unwrap();
        assert_eq!(backend.mpidr, 0x8000_0000);
        assert_eq!(backend.dfr0, 0x10305006);
        if reg == SysReg::MPIDR {
            backend.mpidr = 7;
        } else {
            backend.dfr0 |= 6 << 8;
        }
        backend.ignore_feature_write = true;
        assert!(configure_cpu_features(&mut backend)
            .unwrap_err()
            .contains("feature policy readback"));
        backend.ignore_feature_write = false;
        backend.reject_feature_write = true;
        assert!(configure_cpu_features(&mut backend).is_err());
    }
}
