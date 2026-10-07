//! A guest clock poll that crosses the published bound yields from inside
//! seedbox_read()'s local_irq_save(), so PSTATE.I is set.
use seedbox_arm64::{backend::*, machine::Machine, memory::Memory, *};
use std::{collections::BTreeMap, rc::Rc};
struct G<const EXTENDED: bool = false, const REARM: bool = false> {
    assertions: BTreeMap<u32, usize>,
    levels: BTreeMap<u32, bool>,
    step: usize,
    regs: [u64; 34],
    m: BTreeMap<u32, Rc<Memory>>,
    sys: BTreeMap<u16, u64>,
}
impl<const EXTENDED: bool, const REARM: bool> Backend for G<EXTENDED, REARM> {
    fn create() -> Result<Self> {
        Ok(Self {
            assertions: BTreeMap::new(),
            levels: BTreeMap::new(),
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
        let uart = |offset, data| Exit::Mmio {
            addr: UART_ADDR + offset,
            len: 4,
            write: true,
            data,
            reg: None,
        };
        let extra = if EXTENDED && self.step >= 3 {
            Some(match self.step {
                3 => uart(0x30, 0x101),
                4 => uart(0x38, 0x20), // queue UART as well as timer while masked
                5 => {
                    assert!(self.assertions.is_empty());
                    uart(0x44, 0)
                }
                6 => {
                    assert!(self.assertions.is_empty());
                    self.regs[33] &= !(1 << 7);
                    unsafe {
                        self.m[&1].set_word(8, 101)?;
                    }
                    Exit::SysReg {
                        reg: SysReg::OSLAR.0,
                        write: true,
                        rt: 31,
                        data: 0,
                    }
                }
                7 => {
                    assert!(self.assertions.is_empty(), "debug completion delivered IRQ");
                    assert_eq!(unsafe { self.m[&1].word(0)? }, 100);
                    // Re-arming an already queued timer must not assert twice.
                    if REARM {
                        mmio(EVENT_ADDR, 101)
                    } else {
                        uart(0x44, 0)
                    }
                    // First unmasked semantic exit delivers both lines.
                }
                8 => {
                    assert_eq!(
                        self.assertions,
                        BTreeMap::from([(TIMER_SPI, 1), (UART_SPI, 1)])
                    );
                    assert!(self.levels[&TIMER_SPI] && self.levels[&UART_SPI]);
                    uart(0x44, 0) // unchanged levels must not reassert
                }
                9 => {
                    self.regs[33] |= 1 << 7;
                    mmio(EVENT_ADDR + 8, 1)
                }
                10 => uart(0x38, 0), // deassertions are allowed while masked
                _ => {
                    assert_eq!(
                        self.assertions,
                        BTreeMap::from([(TIMER_SPI, 1), (UART_SPI, 1)])
                    );
                    assert!(!self.levels[&TIMER_SPI] && !self.levels[&UART_SPI]);
                    let n = self.step - 11;
                    if n < b"SEEDBOX_INIT_PASS\n".len() {
                        Exit::Mmio {
                            addr: UART_ADDR,
                            len: 1,
                            write: true,
                            data: b"SEEDBOX_INIT_PASS\n"[n] as u64,
                            reg: None,
                        }
                    } else {
                        self.regs[0] = 0x8400_0008;
                        Exit::Hvc { imm: 0 }
                    }
                }
            })
        } else {
            None
        };
        let exit = if let Some(exit) = extra {
            exit
        } else {
            match self.step {
                0 => mmio(EVENT_ADDR + 32, ABI), // handshake
                1 => mmio(EVENT_ADDR, 100),      // arm deadline at 100 ns (e.g. HZ tick)
                2 => {
                    // seedbox_read(): local_irq_save(), page->now+1 > bound -> yield.
                    self.regs[33] |= 1 << 7;
                    unsafe {
                        self.m[&1].set_word(8, 100)?;
                    }
                    mmio(EVENT_ADDR + 24, 1)
                }
                3 => {
                    self.regs[33] &= !(1 << 7);
                    mmio(EVENT_ADDR + 8, 1)
                }
                n if n - 4 < 18 => Exit::Mmio {
                    addr: UART_ADDR,
                    len: 1,
                    write: true,
                    data: b"SEEDBOX_INIT_PASS\n"[n - 4] as u64,
                    reg: None,
                },
                _ => {
                    self.regs[0] = 0x8400_0008;
                    Exit::Hvc { imm: 0 }
                }
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
        assert!(
            !level || self.regs[33] & (1 << 7) == 0,
            "masked backend assertion"
        );
        if level {
            assert!(EXTENDED, "review repro ACK must cancel the deferred timer");
            *self.assertions.entry(intid).or_default() += 1;
        }
        self.levels.insert(intid, level);
        Ok(())
    }
    fn dirty_pages(&self, _: u32) -> Result<DirtyPages> {
        Ok(DirtyPages::Unsupported)
    }
}
#[test]
fn clock_poll_crossing_bound_must_not_abort_run() {
    let mut k = [0; 64];
    k[56..60].copy_from_slice(b"ARM\x64");
    k[16..24].copy_from_slice(&4096u64.to_le_bytes());
    let r = Machine::<G>::new(&k, b"cpio", 1).unwrap().run(|| {});
    assert!(r.is_ok(), "{}", r.unwrap_err().lines().next().unwrap());
    let report = r.unwrap();
    // Cancellation does not erase evidence of the masked expiry.
    assert_eq!(report.deferred_spi_events, 1);
    assert_eq!(report.deferred_clock_read_spis, 1);
}

#[test]
fn deferred_timer_and_uart_wait_for_semantic_unmask_and_assert_once() {
    let mut k = [0; 64];
    k[56..60].copy_from_slice(b"ARM\x64");
    k[16..24].copy_from_slice(&4096u64.to_le_bytes());
    let r = Machine::<G<true>>::new(&k, b"cpio", 1)
        .unwrap()
        .run(|| {})
        .unwrap();
    assert_eq!(r.virtual_ns, 101);
    // UART deferral is counted, but is not a masked clock-read expiry.
    assert_eq!(r.deferred_spi_events, 2);
    assert_eq!(r.deferred_clock_read_spis, 1);
    assert!(r.trace.iter().all(|record| record.kind != 4));
    assert_eq!(r.trace[6].time, 101);
}

#[test]
fn reassertion_at_unmask_consumes_the_deferred_level_once() {
    let mut k = [0; 64];
    k[56..60].copy_from_slice(b"ARM\x64");
    k[16..24].copy_from_slice(&4096u64.to_le_bytes());
    Machine::<G<true, true>>::new(&k, b"cpio", 1)
        .unwrap()
        .run(|| {})
        .unwrap();
}
