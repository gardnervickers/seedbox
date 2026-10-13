//! ARM64 platform of the stopped machine: Image/FDT boot, GICv3 SPIs, PL011,
//! PSCI and the software exclusive-reservation epoch. HVF on macOS, KVM on Linux.
#[path = "backend.rs"]
mod backend;
#[path = "tree.rs"]
mod tree;
use super::{
    boot,
    memory::{self, Memory},
    Effect, Machine, Result,
};
pub(super) use backend::{Architecture, Backend, Exit, Native};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
/// Guest-advanced exclusive-reservation epoch in the shared PV page.
const EXCLUSIVE_EPOCH: usize = 40;
pub(super) const TIMER: u32 = 40;
const UART: u32 = 33;
const CONSOLE: u32 = 42;
const PSTATE: u32 = 33;
const PSTATE_I: u64 = 1 << 7;
const CNTV_CTL: u16 = 0xdf19;
const CNTKCTL: u16 = 0xc708;
const TPIDRRO: u16 = 0xde83;
const UART_MMIO: u64 = 0x0900_0000;
const UART_END: u64 = UART_MMIO + 0xfff;
/// Interrupt-controller and PL011 state captured with the stopped machine.
#[derive(Clone, Default, Serialize, Deserialize)]
pub(super) struct Devices {
    deferred: BTreeSet<u32>,
    asserted: BTreeSet<u32>,
    uart: [u32; 16],
}
impl Devices {
    /// No interrupt is asserted or awaiting an unmasked rendezvous.
    pub(super) fn quiet(&self) -> bool {
        self.asserted.is_empty() && self.deferred.is_empty()
    }
    /// An interrupt awaits the guest's next unmasked PV rendezvous.
    pub(super) fn deferred(&self) -> bool {
        !self.deferred.is_empty()
    }
}
pub(super) struct Entry(u64);
/// Place the Image, initrd and generated device tree in guest RAM.
pub(super) fn load(
    ram: &mut Memory,
    kernel: &[u8],
    initrd: &[u8],
    seed: u64,
    command_line: &str,
) -> Result<Entry> {
    let (entry, initrd_address) = boot::layout(kernel, initrd)?;
    ram.write((entry - boot::RAM_BASE) as usize, kernel)?;
    ram.write((initrd_address - boot::RAM_BASE) as usize, initrd)?;
    ram.write(
        (boot::DTB - boot::RAM_BASE) as usize,
        &tree::build(initrd_address, initrd.len(), seed, command_line)?,
    )?;
    Ok(Entry(entry))
}
pub(super) fn reset<B: Backend>(native: &mut B, entry: &Entry) -> Result<()> {
    // No EL2, PMU or host topology belongs in the guest profile.
    for (register, mask, value) in [
        (0xc038, 0xf << 56, None),
        (0xc028, 0xf << 8, None),
        (0xc005, u64::MAX, Some(0x8000_0000)),
    ] {
        let want = value.unwrap_or(native.system(register)? & !mask);
        if native.system(register)? != want {
            native.set_system(register, want)?;
        }
        if native.system(register)? != want {
            return Err(format!("PV CPU feature {register:#x} readback"));
        }
    }
    for register in 0..32 {
        native.set_core(register, 0)?;
    }
    native.set_core(0, boot::DTB)?;
    native.set_core(32, entry.0)?;
    native.set_core(PSTATE, 0x3c5)?;
    for (register, value) in [(0xc080, 0x30d0_0800), (0xc708, 0), (0xdf19, 0)] {
        native.set_system(register, value)?;
    }
    Ok(())
}
impl<B: Backend> Machine<B> {
    /// A semantic guest rendezvous invalidates software exclusive reservations.
    pub(super) fn boundary(&mut self) -> Result<()> {
        let epoch = self
            .page
            .word(EXCLUSIVE_EPOCH)
            .checked_add(1)
            .ok_or("PV exclusive epoch overflow")?;
        self.page.set_word(EXCLUSIVE_EPOCH, epoch);
        if self.native.core(PSTATE)? & 15 == 0 {
            self.native.set_system(TPIDRRO, epoch)?;
        }
        Ok(())
    }
    fn irq(&mut self, number: u32, level: bool) -> Result<()> {
        if !level {
            self.state.devices.deferred.remove(&number);
            self.state.devices.asserted.remove(&number);
            return self.native.irq(number, false);
        }
        if self.state.devices.asserted.contains(&number) {
            return Ok(());
        }
        // Admit one unmasked IRQ source at a rendezvous; a second host-pending
        // source must not skid into a later guest instruction based on host timing.
        if !self.state.devices.asserted.is_empty() || self.native.core(PSTATE)? & PSTATE_I != 0 {
            self.state.devices.deferred.insert(number);
            return Ok(());
        }
        self.state.devices.deferred.remove(&number);
        self.native.irq(number, true)?;
        self.state.devices.asserted.insert(number);
        Ok(())
    }
    /// ARM asserts lines at rendezvous; nothing is injected at guest entry.
    pub(super) fn prepare(&mut self) -> Result<()> {
        Ok(())
    }
    pub(super) fn lower(&mut self, number: u32) -> Result<()> {
        self.irq(number, false)
    }
    pub(super) fn interrupts(&mut self) -> Result<()> {
        if self.state.deadline.is_some_and(|d| d <= self.state.now) {
            self.state.deadline = None;
            self.state.timer = true;
            self.irq(TIMER, true)?;
        }
        let uart = &self.state.devices.uart;
        self.irq(
            UART,
            uart[0x38 / 4] & 0x20 != 0 && uart[0x30 / 4] & 0x101 == 0x101,
        )?;
        self.irq(CONSOLE, self.state.console.irq())?;
        if self.state.devices.asserted.is_empty() && self.native.core(PSTATE)? & PSTATE_I == 0 {
            if let Some(number) = self.state.devices.deferred.pop_first() {
                self.native.irq(number, true)?;
                self.state.devices.asserted.insert(number);
            }
        }
        Ok(())
    }
    fn uart(&mut self, offset: u64, width: u8, write: bool, value: u64) -> Result<Option<u64>> {
        if !matches!(width, 1 | 2 | 4) || !offset.is_multiple_of(4) {
            return Err("PV PL011 access width/alignment".into());
        }
        if write {
            match offset {
                0 => self.console_byte(value as u8),
                4 | 0x44 | 0x48 => (),
                0x24..=0x38 => self.state.devices.uart[offset as usize / 4] = value as u32,
                _ => return Err(format!("PV PL011 write {offset:#x}")),
            }
            return Ok(None);
        }
        let uart = &self.state.devices.uart;
        Ok(Some(match offset {
            0 | 4 | 0x44 | 0x48 => 0,
            0x18 => 0x90,
            0x24..=0x38 => uart[offset as usize / 4] as u64,
            0x3c => 0x20,
            0x40 => {
                if uart[14] & 0x20 != 0 && uart[12] & 0x101 == 0x101 {
                    0x20
                } else {
                    0
                }
            }
            0xfe0 => 0x11,
            0xfe4 => 0x10,
            0xfe8 => 0x14,
            0xfec => 0,
            0xff0 => 0x0d,
            0xff4 => 0xf0,
            0xff8 => 5,
            0xffc => 0xb1,
            _ => return Err(format!("PV PL011 read {offset:#x}")),
        }))
    }
    /// Check host-counter isolation and absorb backend maintenance exits.
    /// Returns true when the exit was maintenance, not a guest rendezvous.
    pub(super) fn maintenance(&mut self, exit: &Exit) -> Result<bool> {
        if self.native.system(CNTV_CTL)? & 1 != 0
            || self.state.ready && self.native.system(CNTKCTL)? != 0
        {
            return Err("PV guest enabled host counter/timer".into());
        }
        if matches!(
            exit,
            Exit::System {
                register: 0x809c | 0x8084,
                write: true,
                value: 0
            }
        ) {
            self.native.complete(None)?;
            return Ok(true);
        }
        Ok(false)
    }
    /// Event registers beyond the shared ABI are not defined on ARM.
    pub(super) fn event_register(&mut self, _offset: u64, _value: u64) -> Result<bool> {
        Err("PV unknown clockevent register".into())
    }
    pub(super) fn platform(&mut self, exit: Exit) -> Result<Effect> {
        let mut effect = Effect::default();
        match exit {
            Exit::Mmio {
                address,
                width,
                write,
                value,
            } => match address {
                UART_MMIO..=UART_END => {
                    effect.response = self.uart(address - UART_MMIO, width, write, value)?
                }
                _ => return Err(format!("PV unmapped MMIO {address:#x}")),
            },
            Exit::Wait { event } => {
                if !event {
                    if !self.state.ready {
                        return Err("PV WFI before handshake".into());
                    }
                    effect.idle = true;
                }
            }
            Exit::Call => {
                let function = self.native.core(0)?;
                let value = match function {
                    0x8400_0000 | 0x8400_0006 => 2,
                    0x8400_0008 => {
                        effect.off = true;
                        0
                    }
                    _ => u64::MAX,
                };
                self.native.set_core(0, value)?;
                for register in 1..4 {
                    self.native.set_core(register, 0)?;
                }
            }
            Exit::System { register, .. } => {
                return Err(format!("PV unsupported system register {register:#x}"));
            }
            Exit::Interrupted => unreachable!(),
        }
        Ok(effect)
    }
}

#[cfg(test)]
mod tests {
    use super::super::{State, Stop, IRQ_DEFERRED, LAST, NOW, SYSCALL_COST};
    use super::*;
    use std::{
        collections::{BTreeMap, VecDeque},
        rc::Rc,
    };
    struct Scripted {
        core: [u64; 34],
        system: BTreeMap<u16, u64>,
        exits: VecDeque<Exit>,
        entries: usize,
        irqs: Vec<(u32, bool)>,
    }
    impl Backend for Scripted {
        fn create(_: Rc<Memory>, _: Rc<Memory>) -> Result<Self> {
            Ok(Self {
                core: [0; 34],
                system: BTreeMap::new(),
                exits: VecDeque::new(),
                entries: 0,
                irqs: Vec::new(),
            })
        }
        fn core(&self, r: u32) -> Result<u64> {
            Ok(self.core[r as usize])
        }
        fn set_core(&mut self, r: u32, v: u64) -> Result<()> {
            self.core[r as usize] = v;
            Ok(())
        }
        fn system(&self, r: u16) -> Result<u64> {
            Ok(*self.system.get(&r).unwrap_or(&0))
        }
        fn set_system(&mut self, r: u16, v: u64) -> Result<()> {
            self.system.insert(r, v);
            Ok(())
        }
        fn enter(&mut self) -> Result<Exit> {
            self.entries += 1;
            self.exits
                .pop_front()
                .ok_or("unexpected scripted entry".into())
        }
        fn complete(&mut self, _: Option<u64>) -> Result<()> {
            Ok(())
        }
        fn irq(&mut self, n: u32, v: bool) -> Result<()> {
            self.irqs.push((n, v));
            Ok(())
        }
        fn capture(&self) -> Result<Architecture> {
            Err("script capture unused".into())
        }
        fn restore(&mut self, _: &Architecture) -> Result<()> {
            Err("script restore unused".into())
        }
    }
    fn machine() -> Machine<Scripted> {
        let mut image = vec![0; 64];
        image[16..24].copy_from_slice(&4096u64.to_le_bytes());
        image[56..60].copy_from_slice(b"ARM\x64");
        Machine::boot(&image, b"initrd", 7, "").unwrap()
    }
    fn ready() -> Exit {
        Exit::Mmio {
            address: 0x0a01_0020,
            width: 8,
            write: true,
            value: 3,
        }
    }
    fn idle() -> Exit {
        Exit::Mmio {
            address: 0x0a01_0010,
            width: 8,
            write: true,
            value: 0,
        }
    }
    #[test]
    fn pending_cross_channel_writes_capture_ties_and_drain_once() {
        use crate::pv::transport::{GuestWrite, LifecycleWrite};
        let mut vm = machine();
        vm.state.now = 3;
        vm.uart(0, 4, true, b'A' as u64).unwrap();
        vm.state.console.output.push(3, b"SBX_READY1\n".to_vec());
        vm.collect_lifecycle();
        vm.uart(0, 4, true, b'B' as u64).unwrap();
        vm.state.now = 10;
        let image = serde_json::to_vec(&vm.state).unwrap();
        let mut restored: State = serde_json::from_slice(&image).unwrap();
        assert_eq!(
            std::mem::take(&mut restored.writes),
            [
                GuestWrite::Console(LifecycleWrite {
                    time: 3,
                    bytes: b"A".to_vec()
                }),
                GuestWrite::Lifecycle(LifecycleWrite {
                    time: 3,
                    bytes: b"SBX_READY1\n".to_vec()
                }),
                GuestWrite::Console(LifecycleWrite {
                    time: 3,
                    bytes: b"B".to_vec()
                }),
            ]
        );
        assert!(restored.writes.is_empty());
        assert!(restored.console.output.drain().is_empty());
        assert_eq!(vm.output, b"AB");
    }

    #[test]
    fn host_interruptions_neither_consume_time_nor_invalidate_exclusives() {
        let mut vm = machine();
        vm.native
            .exits
            .extend([Exit::Interrupted, Exit::Interrupted, ready(), idle()]);
        assert!(matches!(
            vm.run(0, 10, 1000, false, &[]).unwrap(),
            Stop::Blocked(None)
        ));
        assert_eq!(vm.state.now, 0);
        assert_eq!(vm.page.word(EXCLUSIVE_EPOCH), 2);
        assert_eq!(vm.native.entries, 4);
        let entries = vm.native.entries;
        assert!(matches!(
            vm.run(25, 25, 1000, false, &[]).unwrap(),
            Stop::Blocked(None)
        ));
        assert_eq!(vm.page.word(EXCLUSIVE_EPOCH), 2);
        assert_eq!(vm.native.entries, entries);
        assert_eq!(vm.page.word(NOW), 25);
    }
    #[test]
    fn zero_budget_adoption_coalesces_due_timer_without_guest_entry() {
        let mut vm = machine();
        vm.state.ready = true;
        vm.state.idle = true;
        vm.state.deadline = Some(5);
        assert!(matches!(
            vm.run(20, 20, 1000, false, &[]).unwrap(),
            Stop::Horizon
        ));
        assert_eq!(vm.native.entries, 0);
        assert_eq!(vm.page.word(EXCLUSIVE_EPOCH), 0);
        assert_eq!(vm.state.now, 20);
        assert_eq!(vm.state.deadline, None);
        assert!(vm.state.timer);
        assert!(vm.state.devices.deferred.contains(&TIMER));
        assert_eq!(vm.page.word(IRQ_DEFERRED), 1);
    }
    #[test]
    fn early_shutdown_buffers_once_and_does_not_invent_wakeup() {
        let mut vm = machine();
        vm.state.ready = true;
        vm.state.idle = true;
        assert!(matches!(
            vm.run(10, 10, 1000, true, &[]).unwrap(),
            Stop::Blocked(None)
        ));
        let state = serde_json::to_vec(&vm.state).unwrap();
        let epoch = vm.page.word(EXCLUSIVE_EPOCH);
        assert!(matches!(
            vm.run(10, 11, 1000, true, &[]).unwrap(),
            Stop::Blocked(None)
        ));
        assert_eq!(vm.native.entries, 0);
        assert_eq!(vm.page.word(EXCLUSIVE_EPOCH), epoch);
        assert_eq!(serde_json::to_vec(&vm.state).unwrap(), state);
    }
    #[test]
    fn zero_budget_republishes_next_price_without_guest_entry_or_progress() {
        let mut vm = machine();
        vm.page.set_word(SYSCALL_COST, 4000);
        assert!(matches!(
            vm.run(0, 0, 1001, false, &[]).unwrap(),
            Stop::Horizon
        ));
        assert_eq!(vm.page.word(SYSCALL_COST), 1001);
        assert_eq!(vm.page.word(LAST), 0);
        assert_eq!(vm.page.word(EXCLUSIVE_EPOCH), 0);
        assert_eq!(vm.native.entries, 0);
        assert_eq!(vm.state.now, 0);
        vm.state.idle = true;
        vm.state.deadline = Some(500);
        assert!(matches!(
            vm.run(100, 100, 1_000_000, false, &[]).unwrap(),
            Stop::Blocked(Some(500))
        ));
        assert_eq!(vm.page.word(SYSCALL_COST), 1_000_000);
        assert_eq!(vm.state.deadline, Some(500));
        assert_eq!(vm.state.now, 100);
        assert_eq!(vm.page.word(LAST), 100);
        assert_eq!(vm.native.entries, 0);
    }
}
