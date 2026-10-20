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
    Machine::boot(&image, b"initrd", 7, "", None, None).unwrap()
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
    use seedbox_machine::pv::transport::{GuestWrite, LifecycleWrite};
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
        vm.run(0, 10, 1000, false, &[], Vec::new()).unwrap(),
        Stop::Blocked(None)
    ));
    assert_eq!(vm.state.now, 0);
    assert_eq!(vm.page.word(EXCLUSIVE_EPOCH), 2);
    assert_eq!(vm.native.entries, 4);
    let entries = vm.native.entries;
    assert!(matches!(
        vm.run(25, 25, 1000, false, &[], Vec::new()).unwrap(),
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
        vm.run(20, 20, 1000, false, &[], Vec::new()).unwrap(),
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
        vm.run(10, 10, 1000, true, &[], Vec::new()).unwrap(),
        Stop::Blocked(None)
    ));
    let state = serde_json::to_vec(&vm.state).unwrap();
    let epoch = vm.page.word(EXCLUSIVE_EPOCH);
    assert!(matches!(
        vm.run(10, 11, 1000, true, &[], Vec::new()).unwrap(),
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
        vm.run(0, 0, 1001, false, &[], Vec::new()).unwrap(),
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
        vm.run(100, 100, 1_000_000, false, &[], Vec::new()).unwrap(),
        Stop::Blocked(Some(500))
    ));
    assert_eq!(vm.page.word(SYSCALL_COST), 1_000_000);
    assert_eq!(vm.state.deadline, Some(500));
    assert_eq!(vm.state.now, 100);
    assert_eq!(vm.page.word(LAST), 100);
    assert_eq!(vm.native.entries, 0);
}
#[test]
fn psci_system_off_and_reset_end_the_boot_only_after_handshake() {
    for (function, expected) in [
        (0x8400_0008u64, Stop::PoweredOff),
        (0x8400_0009, Stop::Reset),
    ] {
        let mut vm = machine();
        vm.native.core[0] = function;
        vm.native.exits.extend([Exit::Call]);
        assert!(
            vm.run(0, 10, 1000, false, &[], Vec::new()).is_err(),
            "{function:#x} before handshake"
        );
        let mut vm = machine();
        vm.native.core[0] = function;
        vm.native.exits.extend([ready(), Exit::Call]);
        assert_eq!(
            vm.run(0, 10, 1000, false, &[], Vec::new()).unwrap(),
            expected
        );
        use seedbox_machine::machine::execution_evidence::chain;
        let h = chain::record(
            &chain::initial(),
            b'w',
            8,
            0,
            super::super::EVENT_MMIO + 32,
            3,
        );
        assert_eq!(vm.state.witness, chain::record(&h, b'k', 0, 0, function, 0));
        assert_eq!(vm.native.entries, 2);
        // The ended boot is never entered or captured again.
        assert!(vm.run(0, 10, 1000, false, &[], Vec::new()).is_err());
        assert_eq!(vm.native.entries, 2);
        let directory = tempfile::tempdir().unwrap();
        assert!(vm.capture(&directory.path().join("capture")).is_err());
    }
}
#[test]
fn unknown_psci_call_returns_not_supported_and_wfi_stays_blocked() {
    let mut vm = machine();
    vm.native.core[0] = 0x8400_000a;
    vm.native
        .exits
        .extend([ready(), Exit::Call, Exit::Wait { event: false }]);
    assert_eq!(
        vm.run(0, 10, 1000, false, &[], Vec::new()).unwrap(),
        Stop::Blocked(None)
    );
    assert_eq!(vm.native.core[0], u64::MAX);
    assert!(!vm.ended);
}
