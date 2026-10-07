use seedbox_arm64::{backend::*, machine::Machine, memory::Memory, *};
use std::{collections::BTreeMap, rc::Rc};
struct Fake<
    const DEBUG_VALUE: u64 = 0,
    const COUNTER_CONTROL: u64 = 0,
    const TIMER_CONTROL: u64 = 0,
> {
    step: usize,
    regs: [u64; 34],
    mapped: BTreeMap<u32, Rc<Memory>>,
    held: bool,
    irq: bool,
}
impl<const D: u64, const C: u64, const T: u64> Backend for Fake<D, C, T> {
    fn create() -> Result<Self> {
        Ok(Self {
            step: 0,
            regs: [0; 34],
            mapped: BTreeMap::new(),
            held: false,
            irq: false,
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
        } else {
            0
        })
    }
    fn set_sys_reg(&mut self, _: SysReg, _: u64) -> Result<()> {
        Ok(())
    }
    fn run(&mut self) -> Result<Stop> {
        assert!(!self.held);
        self.held = true;
        let mmio = |addr, data| Exit::Mmio {
            addr,
            len: 8,
            write: true,
            data,
            reg: None,
        };
        let exit = if self.step < 2 {
            Exit::SysReg {
                reg: [SysReg::OSDLR, SysReg::OSLAR][self.step].0,
                write: true,
                rt: 31,
                data: D,
            }
        } else {
            match self.step - 2 {
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
                2 => mmio(EVENT_ADDR + 16, 1),
                3 => {
                    assert!(self.irq);
                    assert_eq!(unsafe { self.mapped[&1].word(0)? }, 1_000_000_000);
                    mmio(EVENT_ADDR + 8, 1)
                }
                n if n - 4 < b"SEEDBOX_INIT_PASS\r\n".len() => {
                    assert!(!self.irq);
                    Exit::Mmio {
                        addr: UART_ADDR,
                        len: 1,
                        write: true,
                        data: b"SEEDBOX_INIT_PASS\r\n"[n - 4] as u64,
                        reg: None,
                    }
                }
                _ => {
                    self.regs[0] = 0x84000008;
                    Exit::Hvc { imm: 0 }
                }
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
    let b = Machine::<Fake>::new(&kernel, b"cpio", 1)
        .unwrap()
        .run(|| {})
        .unwrap();
    let c = Machine::<Fake>::new(&kernel, b"cpio", 2)
        .unwrap()
        .run(|| {})
        .unwrap();
    assert_eq!(a.witness, b.witness);
    assert_ne!(a.witness, c.witness);
    assert_eq!(a.virtual_ns, 1_000_000_000);
    assert_eq!(a.trace[2].time, 7);
    assert_eq!(a.trace[4].time, 1_000_000_000);
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
