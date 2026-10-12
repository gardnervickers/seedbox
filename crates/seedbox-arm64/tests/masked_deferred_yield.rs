//! A masked clock read whose step crosses its bound while a deferred SPI is
//! pending must still advance to that bound. The guest cannot take the
//! interrupt and its yield does not publish the read, so without the advance
//! it yields again at the same time forever.
use seedbox_arm64::{backend::*, machine::Machine, memory::Memory, *};
use std::{cell::Cell, collections::BTreeMap, rc::Rc};
thread_local!(static SEEN: Cell<Option<(u64, u64, u64)>> = const { Cell::new(None) });
struct G {
    assertions: usize,
    step: usize,
    regs: [u64; 34],
    m: BTreeMap<u32, Rc<Memory>>,
    sys: BTreeMap<u16, u64>,
}
impl Backend for G {
    fn create() -> Result<Self> {
        Ok(Self {
            assertions: 0,
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
    fn run(&mut self) -> Result<RunStop> {
        let mmio = |addr, data| Exit::Mmio {
            addr,
            len: 8,
            write: true,
            data,
            reg: None,
        };
        let exit = match self.step {
            0 => mmio(EVENT_ADDR + 32, ABI),
            1 => mmio(EVENT_ADDR, 100),
            2 => {
                // A masked read crosses the first deadline: the tick is deferred.
                self.regs[33] |= 1 << 7;
                unsafe {
                    self.m[&1].set_word(8, 100)?;
                }
                mmio(EVENT_ADDR + 24, 1)
            }
            // Still masked, the guest re-arms its deadline. Its last read is
            // short of the new bound, and a 100 ns read step crosses it.
            3 => mmio(EVENT_ADDR, 200),
            4 => {
                unsafe {
                    self.m[&1].set_word(8, 150)?;
                }
                mmio(EVENT_ADDR + 24, 1)
            }
            5 => {
                let p = &self.m[&1];
                SEEN.with(|s| {
                    s.set(Some(unsafe {
                        (p.word(0).unwrap(), p.word(16).unwrap(), p.word(32).unwrap())
                    }))
                });
                assert_eq!(self.assertions, 0);
                // Unmasked again, the next read delivers the deferred tick.
                self.regs[33] &= !(1 << 7);
                mmio(EVENT_ADDR + 24, 1)
            }
            n if n - 6 < b"SEEDBOX_INIT_PASS\n".len() => Exit::Mmio {
                addr: UART_ADDR,
                len: 1,
                write: true,
                data: b"SEEDBOX_INIT_PASS\n"[n - 6] as u64,
                reg: None,
            },
            _ => {
                self.regs[0] = 0x8400_0008;
                Exit::Hvc { imm: 0 }
            }
        };
        self.step += 1;
        Ok(Stop {
            pc: 0x4008_0000 + 4 * self.step as u64,
            exit,
        }
        .into())
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
fn masked_yield_at_its_bound_advances_with_a_deferred_spi() {
    let mut k = [0; 64];
    k[56..60].copy_from_slice(b"ARM\x64");
    k[16..24].copy_from_slice(&4096u64.to_le_bytes());
    let report = Machine::<G>::new(&k, b"cpio", 1)
        .unwrap()
        .run(|| {})
        .unwrap();
    let (now, bound, pending) = SEEN.with(|s| s.get()).unwrap();
    assert_eq!(
        (now, pending),
        (200, 1),
        "masked yield at bound {bound} with a deferred tick left the clock at {now}"
    );
    assert_eq!(report.virtual_ns, 200);
}
