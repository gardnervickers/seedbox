use super::super::{
    memory::GRANULE, State, Stop, BOUND, EVENT_MMIO, IRQ_DEFERRED, LAST, NOW, SYSCALL_COST,
};
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
    fn canonical_pending_io(&self) -> Result<Vec<u8>> {
        Ok(vec![0])
    }
    fn stopped_parts(&mut self) -> Result<Vec<(String, String)>> {
        // The scripted register file, not a native register projection.
        Ok(vec![super::super::evidence::part(
            "cpu",
            &(&self.core[..], &self.system),
        )?])
    }
    // Retained snapshots keep no native state of the script; guest writes are
    // never scripted, so there is nothing to track, collect or reset.
    fn capture(&self) -> Result<Architecture> {
        Ok(Architecture {
            core: Vec::new(),
            system: BTreeMap::new(),
            simd: Vec::new(),
            icc: BTreeMap::new(),
            gic: Vec::new(),
        })
    }
    fn restore(&mut self, _: &Architecture) -> Result<()> {
        Ok(())
    }
    fn track(&mut self) -> Result<()> {
        Ok(())
    }
    fn collect(&mut self) -> Result<()> {
        Ok(())
    }
    fn reset(&mut self) -> Result<()> {
        Ok(())
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
        vm.run(0, 10, 1000, false, &[], Vec::new(), &[]).unwrap(),
        Stop::Blocked(None)
    ));
    assert_eq!(vm.state.now, 0);
    assert_eq!(vm.page.word(EXCLUSIVE_EPOCH), 2);
    assert_eq!(vm.native.entries, 4);
    let entries = vm.native.entries;
    assert!(matches!(
        vm.run(25, 25, 1000, false, &[], Vec::new(), &[]).unwrap(),
        Stop::Blocked(None)
    ));
    assert_eq!(vm.page.word(EXCLUSIVE_EPOCH), 2);
    assert_eq!(vm.native.entries, entries);
    assert_eq!(vm.page.word(NOW), 25);
}
fn clock_yield() -> Exit {
    Exit::Mmio {
        address: EVENT_MMIO + 24,
        width: 8,
        write: true,
        value: 1,
    }
}
/// C3: a yield only for the run's bound leaves no trace: no witness record,
/// no reservation epoch, and the stopped page holds the machine's own wake as
/// its bound, not the run's. The next run adopts nothing and enters the guest
/// where it stopped, and the two runs record what one run would.
#[test]
fn a_yield_only_for_the_runs_bound_leaves_no_trace() {
    use seedbox_machine::machine::execution_evidence::chain;
    for wake in [None, Some(30)] {
        let start = |vm: &mut Machine<Scripted>| {
            vm.state.ready = true;
            vm.state.deadline = wake;
            // The guest has read up to the bound, 10, and reads past it.
            vm.page.set_word(LAST, 10);
        };
        let mut vm = machine();
        start(&mut vm);
        vm.native.exits.push_back(clock_yield());
        assert_eq!(
            vm.run(0, 10, 1000, false, &[], Vec::new(), &[]).unwrap(),
            Stop::Horizon
        );
        assert_eq!(vm.page.word(EXCLUSIVE_EPOCH), 0);
        assert_eq!(vm.state.witness, chain::initial());
        assert_eq!(vm.page.word(BOUND), wake.unwrap_or(u64::MAX));
        assert_eq!(vm.state.now, 10);
        vm.native.exits.push_back(idle());
        let entries = vm.native.entries;
        assert_eq!(
            vm.run(10, 20, 1000, false, &[], Vec::new(), &[]).unwrap(),
            Stop::Blocked(wake)
        );
        assert_eq!(vm.native.entries, entries + 1);
        let mut alone = machine();
        start(&mut alone);
        alone.native.exits.push_back(idle());
        alone.run(0, 20, 1000, false, &[], Vec::new(), &[]).unwrap();
        assert_eq!(vm.state.witness, alone.state.witness);
        assert_eq!(vm.page.word(EXCLUSIVE_EPOCH), 1);
        assert_eq!(alone.page.word(EXCLUSIVE_EPOCH), 1);
    }
}
/// C3: a guest that goes idle exactly at the run's bound blocks there, as it
/// does when the run would go on. Stopping at the horizon instead would have
/// the next run enter it again, for an idle exit no other schedule has.
#[test]
fn a_guest_idle_at_the_runs_bound_blocks_there() {
    let start = |vm: &mut Machine<Scripted>| {
        vm.state.ready = true;
        vm.state.deadline = Some(30);
        // The guest has read up to 5 and goes idle there.
        vm.page.set_word(LAST, 5);
        vm.native.exits.push_back(idle());
    };
    let mut vm = machine();
    start(&mut vm);
    assert_eq!(
        vm.run(0, 5, 1000, false, &[], Vec::new(), &[]).unwrap(),
        Stop::Blocked(Some(30))
    );
    // A later run that delivers nothing does not enter the idle guest.
    let entries = vm.native.entries;
    assert_eq!(
        vm.run(5, 20, 1000, false, &[], Vec::new(), &[]).unwrap(),
        Stop::Blocked(Some(30))
    );
    assert_eq!(vm.native.entries, entries);
    let mut alone = machine();
    start(&mut alone);
    assert_eq!(
        alone.run(0, 20, 1000, false, &[], Vec::new(), &[]).unwrap(),
        Stop::Blocked(Some(30))
    );
    assert_eq!(vm.state.witness, alone.state.witness);
    assert_eq!(
        vm.page.word(EXCLUSIVE_EPOCH),
        alone.page.word(EXCLUSIVE_EPOCH)
    );
}
/// A yield for the machine's own wake, or to take a deferred interrupt, is
/// recorded and advances the reservation epoch, without the run's bound.
#[test]
fn a_yield_for_the_wake_or_an_interrupt_is_recorded_without_the_bound() {
    use seedbox_machine::machine::execution_evidence::chain;
    let yielded = |at| {
        let h = chain::record(&chain::initial(), b'w', 8, at, EVENT_MMIO + 24, 1);
        chain::record(&h, b'Y', 0, at, 0, 0)
    };
    // For the wake at 30, which is also the run's bound.
    let mut vm = machine();
    vm.state.ready = true;
    vm.state.deadline = Some(30);
    vm.page.set_word(LAST, 30);
    vm.native.exits.push_back(clock_yield());
    assert_eq!(
        vm.run(0, 30, 1000, false, &[], Vec::new(), &[]).unwrap(),
        Stop::Horizon
    );
    assert_eq!(vm.page.word(EXCLUSIVE_EPOCH), 1);
    assert_eq!(vm.state.witness, yielded(30));
    assert!(vm.state.timer);
    // To take a deferred timer interrupt, with interrupts unmasked, before
    // the bound; the guest then acknowledges it and goes idle.
    let mut vm = machine();
    vm.state.ready = true;
    vm.state.timer = true;
    vm.state.devices.deferred.insert(TIMER);
    vm.native.core[33] = 0;
    let acknowledge = Exit::Mmio {
        address: EVENT_MMIO + 8,
        width: 8,
        write: true,
        value: 1,
    };
    vm.native.exits.extend([clock_yield(), acknowledge, idle()]);
    assert_eq!(
        vm.run(0, 10, 1000, false, &[], Vec::new(), &[]).unwrap(),
        Stop::Blocked(None)
    );
    assert_eq!(vm.page.word(EXCLUSIVE_EPOCH), 3);
    let h = chain::record(&yielded(0), b'I', 0, 0, TIMER.into(), 0);
    let h = chain::record(&h, b'w', 8, 0, EVENT_MMIO + 8, 1);
    let h = chain::record(&h, b'j', 0, 0, TIMER.into(), 0);
    assert_eq!(
        vm.state.witness,
        chain::record(&h, b'w', 8, 0, EVENT_MMIO + 16, 0)
    );
}
#[test]
fn zero_budget_adoption_coalesces_due_timer_without_guest_entry() {
    let mut vm = machine();
    vm.state.ready = true;
    vm.state.idle = true;
    vm.state.deadline = Some(5);
    assert!(matches!(
        vm.run(20, 20, 1000, false, &[], Vec::new(), &[]).unwrap(),
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
        vm.run(10, 10, 1000, true, &[], Vec::new(), &[]).unwrap(),
        Stop::Blocked(None)
    ));
    let state = serde_json::to_vec(&vm.state).unwrap();
    let epoch = vm.page.word(EXCLUSIVE_EPOCH);
    assert!(matches!(
        vm.run(10, 11, 1000, true, &[], Vec::new(), &[]).unwrap(),
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
        vm.run(0, 0, 1001, false, &[], Vec::new(), &[]).unwrap(),
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
        vm.run(100, 100, 1_000_000, false, &[], Vec::new(), &[])
            .unwrap(),
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
            vm.run(0, 10, 1000, false, &[], Vec::new(), &[]).is_err(),
            "{function:#x} before handshake"
        );
        let mut vm = machine();
        vm.native.core[0] = function;
        vm.native.exits.extend([ready(), Exit::Call]);
        assert_eq!(
            vm.run(0, 10, 1000, false, &[], Vec::new(), &[]).unwrap(),
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
        assert!(vm.run(0, 10, 1000, false, &[], Vec::new(), &[]).is_err());
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
        vm.run(0, 10, 1000, false, &[], Vec::new(), &[]).unwrap(),
        Stop::Blocked(None)
    );
    assert_eq!(vm.native.core[0], u64::MAX);
    assert!(!vm.ended);
}

/// One-component mutations of a stopped machine against each comparison
/// field. The execution chain records native interactions only; pending
/// output belongs to the pending-IO witness; the stopped state parts project
/// CPU, clock, interrupt controller and PL011 registers, devices and RAM with
/// the PV page; the disk is compared through its own digests, now and after a
/// power loss. The matrix states what each mutation must change and what it
/// must leave alone.
#[test]
fn each_component_mutation_changes_exactly_its_comparison_fields() {
    use super::super::block::Block;
    use seedbox_machine::machine::{combined_digest, disk::Store, hex};
    use sha2::{Digest, Sha256};
    let root = tempfile::tempdir().unwrap();
    let fresh = || {
        let mut image = vec![0; 64];
        image[16..24].copy_from_slice(&4096u64.to_le_bytes());
        image[56..60].copy_from_slice(b"ARM\x64");
        let mut store = Store::zeroed(16).unwrap();
        store.write(1, &[3; 512]).unwrap();
        store.flush();
        store.write(2, &[5; 512]).unwrap();
        // Small RAM keeps the per-field RAM digests cheap.
        let mut vm: Machine<Scripted> = Machine::boot_with_memory(
            &image,
            b"initrd",
            7,
            "",
            Some(store),
            Some([2, 0, 0, 0, 0, 1]),
            16 << 20,
        )
        .unwrap();
        let policy = super::super::super::policy::Named::new(
            "node",
            &seedbox_machine::definition::BackendPolicy::default(),
        )
        .unwrap()
        .setup(0);
        vm.configure_policy(Some(&policy)).unwrap();
        vm.state.now = 10;
        vm.uart(0, 4, true, b'r' as u64).unwrap();
        vm.record_native(b'w', 4, 0x0900_0000, 1);
        unsafe {
            vm.ram
                .modify(|ram| ram[0x2000..0x200a].copy_from_slice(b"guest data"))
        };
        vm
    };
    fn edit_disk(vm: &mut Machine<Scripted>, edit: impl FnOnce(&mut Store)) {
        let block = vm.block.as_ref().unwrap();
        let mut store = block.store().clone();
        edit(&mut store);
        vm.block = Some(Block::restore(
            block.transport().clone(),
            store,
            block.faults().clone(),
        ));
    }
    let mut taken = 0;
    let mut fields = |vm: &mut Machine<Scripted>| -> BTreeMap<String, String> {
        taken += 1;
        let evidence = vm
            .stopped_evidence(&root.path().join(taken.to_string()))
            .unwrap();
        let execution = vm.native_execution().unwrap();
        let store = vm.block.as_ref().unwrap().store();
        let mut fields: BTreeMap<String, String> = evidence.parts.iter().cloned().collect();
        fields.insert("state".into(), combined_digest(&evidence.parts));
        fields.insert("chain".into(), hex(&execution.chain));
        fields.insert("pending".into(), hex(&Sha256::digest(&execution.pending)));
        fields.insert("disk".into(), store.state_digest(false).unwrap());
        let (lost, _) = store.crash(None, 0).unwrap();
        fields.insert("disk_after_loss".into(), lost.state_digest(false).unwrap());
        fields
    };
    let original = fields(&mut fresh());
    assert_eq!(
        fields(&mut fresh()),
        original,
        "evidence is not reproducible"
    );
    type Mutation = fn(&mut Machine<Scripted>);
    let matrix: [(&str, Mutation, &[&str]); 9] = [
        (
            "console byte",
            |vm| {
                let seedbox_machine::pv::transport::GuestWrite::Console(write) =
                    &mut vm.state.writes[0]
                else {
                    panic!("console write");
                };
                write.bytes[0] ^= 1;
            },
            &["pending", "pending_writes", "state"],
        ),
        ("exit record", |vm| vm.state.witness[0] ^= 1, &["chain"]),
        (
            "timer delivery",
            |vm| {
                vm.state.deadline = Some(vm.state.now);
                vm.interrupts().unwrap();
            },
            &["clock", "devices", "state"],
        ),
        (
            "PL011 register",
            |vm| {
                vm.uart(0x38, 4, true, 0x20).unwrap();
            },
            &["devices", "state"],
        ),
        (
            "unflushed sector",
            |vm| edit_disk(vm, |s| s.write(2, &[6; 512]).unwrap()),
            &["disk"],
        ),
        (
            "durable sector",
            |vm| {
                edit_disk(vm, |s| {
                    s.write(1, &[4; 512]).unwrap();
                    s.flush();
                })
            },
            &["disk", "disk_after_loss"],
        ),
        (
            "guest RAM byte",
            |vm| unsafe { vm.ram.modify(|ram| ram[0x2000] = b'G') },
            &["ram", "ram_pages", "state"],
        ),
        (
            "exclusive epoch",
            |vm| vm.boundary().unwrap(),
            &["ram", "ram_pages", "state"],
        ),
        (
            "CPU register",
            |vm| vm.native.set_core(3, 7).unwrap(),
            &["cpu", "state"],
        ),
    ];
    for (name, mutate, expected) in matrix {
        let mut vm = fresh();
        mutate(&mut vm);
        let changed: Vec<String> = fields(&mut vm)
            .into_iter()
            .filter(|(field, value)| original[field] != *value)
            .map(|(field, _)| field)
            .collect();
        let mut expected = expected.to_vec();
        expected.sort_unstable();
        assert_eq!(changed, expected, "{name}");
    }
}

#[test]
fn only_raised_lines_are_lowered() {
    let mut vm = machine();
    vm.interrupts().unwrap();
    vm.lower(TIMER).unwrap();
    // Raised while masked: deferred, never reaches the native GIC.
    vm.irq(DISK, true).unwrap();
    assert!(vm.state.devices.deferred.contains(&DISK));
    vm.irq(DISK, false).unwrap();
    assert!(vm.state.devices.deferred.is_empty());
    assert!(vm.native.irqs.is_empty());
    vm.native.core[PSTATE as usize] &= !PSTATE_I;
    vm.irq(UART, true).unwrap();
    vm.irq(UART, false).unwrap();
    vm.irq(UART, false).unwrap();
    assert_eq!(vm.native.irqs, [(UART, true), (UART, false)]);
}

/// Write a byte to each of these RAM pages behind tracking's back, as a write
/// that escaped it would.
fn escape(vm: &Machine<Scripted>, pages: &[usize]) {
    for &page in pages {
        unsafe { *vm.ram.ptr().add(page * GRANULE + 1) ^= 0xa5 };
    }
}

#[test]
fn a_snapshot_that_finds_a_missed_write_loses_no_tracked_one() {
    let mut vm = machine();
    vm.snapshot(u64::MAX, true).unwrap().unwrap();
    vm.ram.set_word(300 * GRANULE, 1);
    escape(&vm, &[100, 200]);
    let error = vm.snapshot(u64::MAX, true).err().expect("a missed write");
    assert!(error.contains("missed a write to RAM page 100"), "{error}");
    // Nothing was written since: the next snapshot takes every change,
    // tracked or not, and verifies.
    vm.snapshot(u64::MAX, true).unwrap().unwrap();
}

#[test]
fn a_rewind_that_finds_a_missed_write_can_be_rewound_again() {
    let mut vm = machine();
    let version = vm.snapshot(u64::MAX, true).unwrap().unwrap().id;
    let saved = unsafe { vm.ram.bytes() }.to_vec();
    vm.ram.set_word(300 * GRANULE, 1);
    vm.rewind(version, false).unwrap();
    escape(&vm, &[100, 200]);
    let error = vm.rewind(version, true).expect_err("a missed write");
    assert!(error.contains("left RAM page 100 differing"), "{error}");
    // The failed rewind already made the version current; the next one must
    // still rewrite what differs from it.
    vm.rewind(version, false).unwrap();
    assert!(unsafe { vm.ram.bytes() } == &saved[..]);
}
fn networked() -> Machine<Scripted> {
    let mut image = vec![0; 64];
    image[16..24].copy_from_slice(&4096u64.to_le_bytes());
    image[56..60].copy_from_slice(b"ARM\x64");
    let mut vm = Machine::boot(&image, b"initrd", 7, "", None, Some([2, 0, 0, 0, 0, 1])).unwrap();
    vm.state.ready = true;
    vm
}
fn acknowledge() -> Exit {
    Exit::Mmio {
        address: EVENT_MMIO + 8,
        width: 8,
        write: true,
        value: 1,
    }
}
/// What a stopped machine shows, for comparing two schedules of it.
fn look(vm: &Machine<Scripted>) -> (String, [u64; 4], usize) {
    (
        serde_json::to_string(&vm.state).unwrap(),
        [
            vm.page.word(NOW),
            vm.page.word(LAST),
            vm.page.word(BOUND),
            vm.page.word(EXCLUSIVE_EPOCH),
        ],
        vm.native.entries,
    )
}
/// A timed run delivers each frame at its time and goes on across blocks
/// whose wake comes first, making exactly the runs the client would make
/// with one run per input: the stopped machine is the same.
#[test]
fn a_timed_run_makes_the_runs_one_run_per_input_would() {
    let frame = |tag: u8| {
        let mut frame = vec![0xff; 6];
        frame.extend([2, 0, 0, 0, 0, 2, 0x88, 0xb5, tag]);
        frame.resize(60, 0);
        frame
    };
    // Idle from the start with a timer at 30: frames at 15 and 50, the
    // timer taken between them. Then busy up to the first frame, which the
    // guest reaches by a yield at the bound, and the timer after it.
    for busy in [false, true] {
        let script = |vm: &mut Machine<Scripted>| {
            vm.state.deadline = Some(30);
            if busy {
                vm.page.set_word(LAST, 15);
                vm.native
                    .exits
                    .extend([clock_yield(), idle(), acknowledge(), idle()]);
            } else {
                vm.native.exits.extend([idle(), acknowledge(), idle()]);
            }
        };
        let mut split = networked();
        script(&mut split);
        let first = split.run(0, 15, 1000, false, &[], Vec::new(), &[]).unwrap();
        assert_eq!(
            first,
            if busy {
                Stop::Horizon
            } else {
                Stop::Blocked(Some(30))
            }
        );
        let wake = if busy { 100 } else { 50 };
        assert_eq!(
            split
                .run(15, wake, 1000, false, &[], vec![frame(1)], &[])
                .unwrap(),
            Stop::Blocked(Some(30))
        );
        let last = if busy {
            split.run(30, 100, 1000, false, &[], Vec::new(), &[])
        } else {
            split.run(30, 50, 1000, false, &[], Vec::new(), &[])
        };
        assert_eq!(last.unwrap(), Stop::Blocked(None));
        let mut frames = vec![(15, frame(1))];
        if !busy {
            assert_eq!(
                split
                    .run(50, 100, 1000, false, &[], vec![frame(2)], &[])
                    .unwrap(),
                Stop::Blocked(None)
            );
            frames.push((50, frame(2)));
        }
        let delivered = frames.len();
        let mut timed = networked();
        script(&mut timed);
        assert_eq!(
            timed
                .run_timed(0, 100, 1000, false, &[], Vec::new(), frames, &[])
                .unwrap(),
            (Stop::Blocked(None), delivered)
        );
        assert_eq!(look(&timed), look(&split), "busy {busy}");
        assert!(timed.native.exits.is_empty());
    }
}
/// A timed run that ends before a frame, here at the guest's power-off,
/// leaves it undelivered.
#[test]
fn a_timed_run_leaves_the_frames_it_does_not_reach() {
    // Idle with a timer at 15, which wakes it after the first frame; it
    // powers off there, before the second.
    let mut vm = networked();
    vm.state.deadline = Some(15);
    vm.native.core[0] = 0x8400_0008;
    vm.native.exits.extend([idle(), Exit::Call]);
    let frame = vec![0xff; 60];
    assert_eq!(
        vm.run_timed(
            0,
            100,
            1000,
            false,
            &[],
            Vec::new(),
            vec![(10, frame.clone()), (20, frame.clone())],
            &[]
        )
        .unwrap(),
        (Stop::PoweredOff, 1)
    );
    // Frames must lie inside the run, in order.
    let mut vm = networked();
    for frames in [
        vec![(0, frame.clone())],
        vec![(100, frame.clone())],
        vec![(20, frame.clone()), (10, frame)],
    ] {
        assert!(vm
            .run_timed(0, 100, 1000, false, &[], Vec::new(), frames, &[])
            .is_err());
    }
}
