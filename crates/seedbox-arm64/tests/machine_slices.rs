//! Exercise world stops through Machine, including the guest-owned PV page.
use seedbox_arm64::{
    backend::*,
    machine::{Machine, MachineStop},
    memory::Memory,
    *,
};
use std::{
    cell::RefCell,
    collections::{BTreeMap, VecDeque},
    rc::Rc,
};

#[derive(Clone, Copy)]
enum Action {
    Ready(u64),
    Arm(u64),
    Yield,
    Idle,
    Ack,
    Off,
}
#[derive(Default)]
struct Observations {
    entries: Vec<(u64, u64)>,
    completions: usize,
    timers: usize,
}
thread_local! {
    static SCRIPT: RefCell<VecDeque<Action>> = RefCell::default();
    static OBSERVED: RefCell<Observations> = RefCell::default();
}
struct Mock {
    memory: BTreeMap<u32, Rc<Memory>>,
    registers: [u64; 34],
    system: BTreeMap<u16, u64>,
}
fn index(reg: CoreReg) -> usize {
    match reg {
        CoreReg::X(n) => n as usize,
        CoreReg::Sp => 31,
        CoreReg::Pc => 32,
        CoreReg::Pstate => 33,
    }
}
impl Backend for Mock {
    fn create() -> Result<Self> {
        Ok(Self {
            memory: BTreeMap::new(),
            registers: [0; 34],
            system: BTreeMap::new(),
        })
    }
    fn map_memory(&mut self, slot: u32, _: u64, memory: Rc<Memory>) -> Result<()> {
        self.memory.insert(slot, memory);
        Ok(())
    }
    fn unmap_memory(&mut self, slot: u32) -> Result<()> {
        self.memory.remove(&slot);
        Ok(())
    }
    fn create_vcpu(&mut self) -> Result<()> {
        Ok(())
    }
    fn get_reg(&self, reg: CoreReg) -> Result<u64> {
        Ok(self.registers[index(reg)])
    }
    fn set_reg(&mut self, reg: CoreReg, value: u64) -> Result<()> {
        self.registers[index(reg)] = value;
        Ok(())
    }
    fn get_sys_reg(&self, reg: SysReg) -> Result<u64> {
        Ok(*self.system.get(&reg.0).unwrap_or(&0))
    }
    fn set_sys_reg(&mut self, reg: SysReg, value: u64) -> Result<()> {
        self.system.insert(reg.0, value);
        Ok(())
    }
    fn run(&mut self) -> Result<Stop> {
        let page = &self.memory[&1];
        let publication = unsafe { (page.word(0)?, page.word(16)?) };
        OBSERVED.with(|o| o.borrow_mut().entries.push(publication));
        self.registers[33] &= !(1 << 7);
        let action = SCRIPT
            .with(|s| s.borrow_mut().pop_front())
            .ok_or("unexpected guest re-entry")?;
        let (offset, data) = match action {
            Action::Ready(last) => {
                unsafe {
                    page.set_word(8, last)?;
                }
                (32, ABI)
            }
            Action::Arm(deadline) => (0, deadline),
            Action::Yield => (24, 1),
            Action::Idle => (16, 1),
            Action::Ack => (8, 1),
            Action::Off => {
                self.registers[0] = 0x8400_0008;
                return Ok(Stop {
                    pc: RAM_BASE,
                    exit: Exit::Hvc { imm: 0 },
                });
            }
        };
        Ok(Stop {
            pc: RAM_BASE,
            exit: Exit::Mmio {
                addr: EVENT_ADDR + offset,
                len: 8,
                write: true,
                data,
                reg: None,
            },
        })
    }
    fn complete(&mut self, _: Option<u64>) -> Result<()> {
        OBSERVED.with(|o| o.borrow_mut().completions += 1);
        Ok(())
    }
    fn inject_spi(&mut self, intid: u32, level: bool) -> Result<()> {
        assert_eq!(intid, TIMER_SPI);
        if level {
            OBSERVED.with(|o| o.borrow_mut().timers += 1);
        }
        Ok(())
    }
    fn dirty_pages(&self, _: u32) -> Result<DirtyPages> {
        Ok(DirtyPages::Unsupported)
    }
}
fn machine(script: &[Action]) -> Machine<Mock> {
    SCRIPT.with(|s| *s.borrow_mut() = script.iter().copied().collect());
    OBSERVED.with(|o| *o.borrow_mut() = Observations::default());
    let mut kernel = [0; 64];
    kernel[56..60].copy_from_slice(b"ARM\x64");
    kernel[16..24].copy_from_slice(&4096u64.to_le_bytes());
    Machine::new(&kernel, b"initrd", 1).unwrap()
}

#[test]
fn horizon_stops_once_completes_exit_and_preserves_future_timer() {
    for rendezvous in [Action::Yield, Action::Idle] {
        let mut m = machine(&[
            Action::Ready(10),
            Action::Arm(100),
            rendezvous,
            Action::Idle,
            Action::Ack,
            Action::Idle,
        ]);
        assert_eq!(
            m.run_until(40, || {}).unwrap(),
            MachineStop::Horizon { time_ns: 40 }
        );
        OBSERVED.with(|o| {
            let o = o.borrow();
            assert_eq!(o.entries.len(), 3);
            assert_eq!(o.completions, 3);
            assert_eq!(o.timers, 0);
            assert_eq!(o.entries[0], (0, 40));
        });
        assert_eq!(
            m.run_until(40, || panic!("boundary must not re-enter"))
                .unwrap(),
            MachineStop::Horizon { time_ns: 40 }
        );
        assert_eq!(
            m.run_until(150, || {}).unwrap(),
            MachineStop::Horizon { time_ns: 150 }
        );
        OBSERVED.with(|o| {
            let o = o.borrow();
            assert_eq!(o.entries[3], (40, 100));
            assert_eq!(o.timers, 1);
            assert_eq!(o.completions, 6);
        });
    }
}

#[test]
fn equal_timer_is_injected_at_boundary_and_not_again_on_resume() {
    let mut m = machine(&[
        Action::Ready(0),
        Action::Arm(40),
        Action::Yield,
        Action::Ack,
        Action::Idle,
    ]);
    assert_eq!(
        m.run_until(40, || {}).unwrap(),
        MachineStop::Horizon { time_ns: 40 }
    );
    OBSERVED.with(|o| assert_eq!(o.borrow().timers, 1));
    assert_eq!(
        m.run_until(60, || {}).unwrap(),
        MachineStop::Horizon { time_ns: 60 }
    );
    OBSERVED.with(|o| assert_eq!(o.borrow().timers, 1));
}

#[test]
fn boot_clock_cannot_silently_advance_past_first_horizon() {
    let mut m = machine(&[Action::Ready(10)]);
    assert!(m
        .run_until(5, || {})
        .unwrap_err()
        .contains("exceeds world horizon"));
    assert_eq!(m.time_ns(), 0);
    OBSERVED.with(|o| {
        let o = o.borrow();
        assert_eq!(o.entries, vec![(0, 5)]);
        assert_eq!(o.completions, 0);
    });
}

#[test]
fn invalid_horizon_does_not_enter_guest_or_publish_new_bound() {
    let mut m = machine(&[Action::Ready(0), Action::Yield]);
    assert_eq!(
        m.run_until(40, || {}).unwrap(),
        MachineStop::Horizon { time_ns: 40 }
    );
    for horizon in [39, u64::MAX] {
        assert!(m
            .run_until(horizon, || panic!("invalid horizon entered guest"))
            .is_err());
    }
    assert_eq!(m.time_ns(), 40);
    OBSERVED.with(|o| assert_eq!(o.borrow().entries.len(), 2));
}

#[test]
fn zero_boundary_and_poweroff_never_reenter_guest() {
    let mut m = machine(&[Action::Ready(0), Action::Off]);
    assert_eq!(
        m.run_until(0, || panic!("zero boundary entered guest"))
            .unwrap(),
        MachineStop::Horizon { time_ns: 0 }
    );
    assert_eq!(m.run_until(10, || {}).unwrap(), MachineStop::PoweredOff);
    assert_eq!(
        m.run_until(20, || panic!("powered-off guest re-entered"))
            .unwrap(),
        MachineStop::PoweredOff
    );
    OBSERVED.with(|o| assert_eq!(o.borrow().entries.len(), 2));
}
