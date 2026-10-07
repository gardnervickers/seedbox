//! Review repro (#120 @ 66cf845): after a masked yield defers the timer SPI,
//! the published page must make the guest's next clock read exit again, or an
//! IRQ-enabled compute loop that only reads the clock never receives the tick.
use seedbox_arm64::{backend::*, machine::Machine, memory::Memory, *};
use std::{cell::Cell, collections::BTreeMap, rc::Rc};
thread_local!(static SEEN: Cell<Option<(u64, u64, u64)>> = const { Cell::new(None) });
struct G<const DELIVER: bool = false, const REARM: bool = false> {
    assertions: usize,
    rearmed: bool,
    step: usize,
    regs: [u64; 34],
    m: BTreeMap<u32, Rc<Memory>>,
    sys: BTreeMap<u16, u64>,
}
impl<const DELIVER: bool, const REARM: bool> Backend for G<DELIVER, REARM> {
    fn create() -> Result<Self> {
        Ok(Self {
            assertions: 0,
            rearmed: false,
            step: 0,
            regs: [0; 34],
            m: BTreeMap::new(),
            sys: BTreeMap::new(),
        })
    }
    fn map_memory(&mut self, s: u32, _: u64, m: Rc<Memory>) -> Result<()> {
        self.m.insert(s, m);
        Ok(())
    }
    fn unmap_memory(&mut self, _: u32) -> Result<()> {
        Ok(())
    }
    fn create_vcpu(&mut self) -> Result<()> {
        Ok(())
    }
    fn get_reg(&self, r: CoreReg) -> Result<u64> {
        Ok(self.regs[match r {
            CoreReg::X(n) => n as usize,
            CoreReg::Sp => 31,
            CoreReg::Pc => 32,
            CoreReg::Pstate => 33,
        }])
    }
    fn set_reg(&mut self, r: CoreReg, v: u64) -> Result<()> {
        self.regs[match r {
            CoreReg::X(n) => n as usize,
            CoreReg::Sp => 31,
            CoreReg::Pc => 32,
            CoreReg::Pstate => 33,
        }] = v;
        Ok(())
    }
    fn get_sys_reg(&self, r: SysReg) -> Result<u64> {
        Ok(*self
            .sys
            .get(&r.0)
            .unwrap_or(&if r == SysReg::MPIDR { 0x8000_0000 } else { 0 }))
    }
    fn set_sys_reg(&mut self, r: SysReg, v: u64) -> Result<()> {
        self.sys.insert(r.0, v);
        Ok(())
    }
    fn run(&mut self) -> Result<Stop> {
        let mmio = |addr, data| Exit::Mmio {
            addr,
            len: 8,
            write: true,
            data,
            reg: None,
        };
        if REARM && self.step == 3 && !self.rearmed {
            self.rearmed = true;
            return Ok(Stop {
                pc: 0x4008_000c,
                exit: mmio(EVENT_ADDR, 200),
            });
        }
        let exit = match self.step {
            0 => mmio(EVENT_ADDR + 32, ABI),
            1 => mmio(EVENT_ADDR, 100),
            2 => {
                // sched_clock() under local_irq_save crosses the bound: masked yield.
                self.regs[33] |= 1 << 7;
                unsafe {
                    self.m[&1].set_word(8, 100)?;
                }
                mmio(EVENT_ADDR + 24, 1)
            }
            3 => {
                // Back in an IRQ-enabled loop that only reads the clock. It exits
                // again if it crosses bound or sees a deferred IRQ (ABI 2).
                let p = &self.m[&1];
                SEEN.with(|s| {
                    s.set(Some(unsafe {
                        (p.word(0).unwrap(), p.word(16).unwrap(), p.word(32).unwrap())
                    }))
                });
                assert_eq!(self.assertions, 0);
                self.regs[33] &= !(1 << 7);
                mmio(EVENT_ADDR + if DELIVER { 24 } else { 8 }, 1)
            }
            n if n - 4 < b"SEEDBOX_INIT_PASS\n".len() => {
                assert_eq!(self.assertions, usize::from(DELIVER));
                assert_eq!(unsafe { self.m[&1].word(32)? }, 0);
                Exit::Mmio {
                    addr: UART_ADDR,
                    len: 1,
                    write: true,
                    data: b"SEEDBOX_INIT_PASS\n"[n - 4] as u64,
                    reg: None,
                }
            }
            _ => {
                self.regs[0] = 0x8400_0008;
                Exit::Hvc { imm: 0 }
            }
        };
        self.step += 1;
        Ok(Stop {
            pc: 0x4008_0000 + 4 * self.step as u64,
            exit,
        })
    }
    fn complete(&mut self, _: Option<u64>) -> Result<()> {
        Ok(())
    }
    fn inject_spi(&mut self, intid: u32, level: bool) -> Result<()> {
        assert_eq!(intid, TIMER_SPI);
        assert!(
            !level || self.regs[33] & (1 << 7) == 0,
            "masked backend assertion"
        );
        self.assertions += usize::from(level);
        Ok(())
    }
    fn dirty_pages(&self, _: u32) -> Result<DirtyPages> {
        Ok(DirtyPages::Unsupported)
    }
}
#[test]
fn deferred_spi_keeps_an_unmasked_read_rendezvous_published() {
    let mut k = [0; 64];
    k[56..60].copy_from_slice(b"ARM\x64");
    k[16..24].copy_from_slice(&4096u64.to_le_bytes());
    Machine::<G>::new(&k, b"cpio", 1)
        .unwrap()
        .run(|| {})
        .unwrap();
    let (now, bound, pending) = SEEN.with(|s| s.get()).unwrap();
    assert!(
        now + 1 > bound || pending == 1,
        "deferred timer SPI but published now={now} bound={bound:#x}: unmasked reads never exit"
    );
}

#[test]
fn unmasked_clock_read_delivers_deferred_tick_without_an_armed_deadline() {
    let mut k = [0; 64];
    k[56..60].copy_from_slice(b"ARM\x64");
    k[16..24].copy_from_slice(&4096u64.to_le_bytes());
    let report = Machine::<G<true>>::new(&k, b"cpio", 1)
        .unwrap()
        .run(|| {})
        .unwrap();
    assert_eq!(report.virtual_ns, 100);
    assert_eq!(report.trace[3].addr, EVENT_ADDR + 24);
}

#[test]
fn deferred_irq_rendezvous_does_not_jump_to_a_rearmed_future_deadline() {
    let mut k = [0; 64];
    k[56..60].copy_from_slice(b"ARM\x64");
    k[16..24].copy_from_slice(&4096u64.to_le_bytes());
    let report = Machine::<G<true, true>>::new(&k, b"cpio", 1)
        .unwrap()
        .run(|| {})
        .unwrap();
    assert_eq!(report.virtual_ns, 100);
    assert_eq!(SEEN.with(|s| s.get()).unwrap(), (100, 200, 1));
}
