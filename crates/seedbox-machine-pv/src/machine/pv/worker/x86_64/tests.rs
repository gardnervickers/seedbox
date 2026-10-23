use super::super::{Stop, IRQ_DEFERRED, LAST, NOW};
use super::*;
use std::{collections::VecDeque, rc::Rc};
#[derive(Default)]
struct Scripted {
    exits: VecDeque<Exit>,
    entries: usize,
    interruptible: VecDeque<bool>,
    injected: Vec<u8>,
    window: bool,
    responses: Vec<Option<u64>>,
    page: Option<Rc<Memory>>,
    publications: std::collections::BTreeMap<usize, u64>,
    virtio_publications: std::collections::BTreeMap<usize, (usize, u64)>,
    nic_words_at_entry: Vec<u64>,
}
impl Backend for Scripted {
    fn create(_: Rc<Memory>, page: Rc<Memory>) -> Result<Self> {
        Ok(Self {
            page: Some(page),
            ..Self::default()
        })
    }
    fn boot(&mut self, _: backend::Boot) -> Result<()> {
        Ok(())
    }
    fn enter(&mut self) -> Result<Exit> {
        self.entries += 1;
        if let Some(update) = self.publications.remove(&self.entries) {
            self.page.as_ref().unwrap().set_word(PIC_UPDATE, update);
        }
        if let Some((slot, word)) = self.virtio_publications.remove(&self.entries) {
            self.page
                .as_ref()
                .unwrap()
                .set_word(VIRTIO_IRQ + slot * 8, word);
        }
        self.nic_words_at_entry
            .push(self.page.as_ref().unwrap().word(VIRTIO_IRQ + 16));
        self.exits
            .pop_front()
            .ok_or("unexpected scripted entry".into())
    }
    fn canonical_pending_io(&self) -> Result<Vec<u8>> {
        Ok(vec![0])
    }
    fn complete(&mut self, response: Option<u64>) -> Result<()> {
        self.responses.push(response);
        Ok(())
    }
    fn interruptible(&mut self) -> bool {
        self.interruptible.pop_front().unwrap_or(false)
    }
    fn inject(&mut self, vector: u8) -> Result<()> {
        self.injected.push(vector);
        Ok(())
    }
    fn request_window(&mut self, enabled: bool) {
        self.window = enabled;
    }
    fn location(&self) -> u64 {
        0
    }
    fn stopped_parts(&mut self) -> Result<Vec<(String, String)>> {
        // Actual Scripted model state, not a native/VM acceptance substitute.
        use super::super::evidence::part;
        Ok(vec![
            part("regs", &self.entries)?,
            part("vectors", &self.responses)?,
        ])
    }
    fn capture(&mut self) -> Result<Architecture> {
        Err("script capture unused".into())
    }
    fn restore(&mut self, _: &Architecture) -> Result<()> {
        Err("script restore unused".into())
    }
}
fn machine() -> Machine<Scripted> {
    machine_with(None)
}
fn machine_with(disk: Option<seedbox_machine::machine::disk::Store>) -> Machine<Scripted> {
    machine_full(disk, None)
}
fn machine_full(
    disk: Option<seedbox_machine::machine::disk::Store>,
    network: Option<[u8; 6]>,
) -> Machine<Scripted> {
    // Minimal 64-bit bzImage header; the scripted backend executes nothing.
    let mut image = vec![0; 1024 + 0x210];
    image[0x1f1] = 1;
    image[0x201] = 0x66;
    image[0x202..0x206].copy_from_slice(b"HdrS");
    image[0x206..0x208].copy_from_slice(&0x020fu16.to_le_bytes());
    image[0x211] = 1;
    image[0x22c..0x230].copy_from_slice(&0x7fff_ffffu32.to_le_bytes());
    image[0x236] = 1;
    image[0x258..0x260].copy_from_slice(&0x10_0000u64.to_le_bytes());
    Machine::boot(&image, b"initrd", 7, "console=ttyS0", disk, network).unwrap()
}
fn event(offset: u64, value: u64) -> Exit {
    Exit::Mmio {
        address: super::super::EVENT_MMIO + offset,
        width: 8,
        write: true,
        value,
    }
}
fn port(port: u16, write: bool, value: u64) -> Exit {
    Exit::Port {
        port,
        width: 1,
        write,
        value,
    }
}
fn halt() -> Exit {
    Exit::Halt { interrupts: true }
}
/// Initialize the PIC like Linux: ICW1-4, then unmask lines 0, 4 and 5.
fn pic_setup() -> [Exit; 5] {
    [
        port(0x20, true, 0x11),
        port(0x21, true, 0x30),
        port(0x21, true, 0x04),
        port(0x21, true, 0x01),
        port(0x21, true, 0xce),
    ]
}
#[test]
fn policy_fresh_arm_hook_draws_at_programming_not_wake_and_speed_is_published() {
    let mut vm = machine();
    let selected = super::super::super::policy::Named::new(
        "node",
        &seedbox_machine::definition::BackendPolicy {
            seed: 42,
            timer_jitter_ns: 17,
            seeded_pv_speed: true,
            ..Default::default()
        },
    )
    .unwrap()
    .setup(0);
    vm.configure_policy(Some(&selected)).unwrap();
    vm.native
        .exits
        .extend([event(32, 3), event(0, 100), event(0, 100), halt()]);
    assert!(matches!(
        vm.run(0, 200, 1000, false, &[], Vec::new()).unwrap(),
        Stop::Blocked(Some(116))
    ));
    assert_eq!(vm.page.word(super::super::SYSCALL_COST), 985);
    let before = serde_json::to_vec(&vm.state.policy).unwrap();
    assert!(matches!(
        vm.run(116, 116, 1000, false, &[], Vec::new()).unwrap(),
        Stop::Horizon
    ));
    assert_eq!(serde_json::to_vec(&vm.state.policy).unwrap(), before);
}
#[test]
fn actual_mapped_ram_and_shared_page_evidence_do_not_enter_or_snapshot() {
    let mut vm = machine();
    let selected = super::super::super::policy::Named::new(
        "node",
        &seedbox_machine::definition::BackendPolicy::default(),
    )
    .unwrap()
    .setup(0);
    vm.configure_policy(Some(&selected)).unwrap();
    let root = tempfile::tempdir().unwrap();
    let a = vm.stopped_evidence(&root.path().join("a")).unwrap();
    let b = vm.stopped_evidence(&root.path().join("b")).unwrap();
    assert_eq!(a.parts, b.parts);
    assert_eq!(a.regions, b.regions);
    assert_eq!(a.page_bytes, b.page_bytes);
    assert_eq!(vm.native.entries, 0);
    assert!(a.regions.contains(&(0x0a00_0000, 16384)));
    let pages = std::fs::read(root.path().join("a")).unwrap();
    assert_eq!(pages.len() as u64, a.page_bytes);
    vm.page.set_word(super::super::NOW, 7);
    let changed = vm.stopped_evidence(&root.path().join("c")).unwrap();
    assert_ne!(
        a.parts.iter().find(|(n, _)| n == "ram"),
        changed.parts.iter().find(|(n, _)| n == "ram")
    );
    assert_eq!(vm.native.entries, 0);
}
#[test]
fn host_interruptions_neither_consume_time_nor_inject() {
    let mut vm = machine();
    vm.native
        .exits
        .extend([Exit::Interrupted, Exit::Interrupted, event(32, 3), halt()]);
    assert!(matches!(
        vm.run(0, 10, 1000, false, &[], Vec::new()).unwrap(),
        Stop::Blocked(None)
    ));
    assert_eq!(vm.state.now, 0);
    assert_eq!(vm.native.entries, 4);
    assert!(vm.native.injected.is_empty());
    let entries = vm.native.entries;
    assert!(matches!(
        vm.run(25, 25, 1000, false, &[], Vec::new()).unwrap(),
        Stop::Blocked(None)
    ));
    assert_eq!(vm.native.entries, entries);
    assert_eq!(vm.page.word(NOW), 25);
}
#[test]
fn zero_budget_adoption_marks_timer_without_entry_or_deferral() {
    let mut vm = machine();
    vm.state.ready = true;
    vm.state.idle = true;
    vm.state.deadline = Some(5);
    assert!(matches!(
        vm.run(20, 20, 1000, false, &[], Vec::new()).unwrap(),
        Stop::Horizon
    ));
    assert_eq!(vm.native.entries, 0);
    assert_eq!(vm.state.deadline, None);
    assert!(vm.state.timer);
    assert_eq!(vm.state.devices.lines, 1);
    assert_eq!(vm.page.word(IRQ_DEFERRED), 0);
    assert_eq!(vm.page.word(LAST), 20);
    assert!(vm.native.injected.is_empty());
}
#[test]
fn timer_waits_for_an_interrupt_window_and_is_injected_once() {
    let mut vm = machine();
    vm.native.exits.extend([event(32, 3)]);
    vm.native.exits.extend(pic_setup());
    vm.native.exits.extend([event(0, 5), halt()]);
    assert!(matches!(
        vm.run(0, 100, 1000, false, &[], Vec::new()).unwrap(),
        Stop::Blocked(Some(5))
    ));
    assert_eq!(vm.state.devices.pic.imr, 0xce);
    // Zero-budget adoption raises the line without entering or injecting.
    assert!(matches!(
        vm.run(5, 5, 1000, false, &[], Vec::new()).unwrap(),
        Stop::Horizon
    ));
    assert!(vm.native.injected.is_empty());
    let entries = vm.native.entries;
    // Not interruptible at entry: request a window; inject at its exit.
    vm.native.interruptible.extend([false, true]);
    vm.native.exits.extend([
        Exit::Window,
        // Linux's mask_and_ack: mask, specific EOI; handler acks the timer.
        port(0x21, true, 0xcf),
        port(0x20, true, 0x60),
        port(ACK_PORT, true, 1),
        port(0x21, true, 0xce),
        halt(),
    ]);
    assert!(matches!(
        vm.run(5, 100, 1000, false, &[], Vec::new()).unwrap(),
        Stop::Blocked(None)
    ));
    assert_eq!(vm.native.entries, entries + 6);
    assert_eq!(vm.native.injected, [0x30]);
    assert_eq!(vm.state.devices.pic.isr, 0);
    assert_eq!(vm.state.devices.lines, 0);
    assert!(!vm.native.window);
}
#[test]
fn port_yield_advances_to_the_deadline_or_run_horizon() {
    for deadline in [None, Some(5), Some(20)] {
        let mut vm = machine();
        vm.native.exits.push_back(event(32, 3));
        if let Some(deadline) = deadline {
            vm.native.exits.push_back(event(0, deadline));
        }
        vm.native
            .exits
            .extend([port(YIELD_PORT, true, 1), event(40, 1)]);
        let stop = vm.run(0, 10, 1000, false, &[], Vec::new()).unwrap();
        assert_eq!(vm.state.now, deadline.unwrap_or(10).min(10));
        assert_eq!(vm.state.timer, deadline == Some(5));
        assert_eq!(vm.state.devices.lines & 1 != 0, deadline == Some(5));
        assert_eq!(
            stop,
            if deadline == Some(5) {
                Stop::PoweredOff
            } else {
                Stop::Horizon
            }
        );
        assert!(vm.native.responses.iter().all(Option::is_none));
        if deadline.is_none() {
            use seedbox_machine::machine::execution_evidence::chain;
            let h = chain::record(
                &chain::initial(),
                b'w',
                8,
                0,
                super::super::EVENT_MMIO + 32,
                3,
            );
            let h = chain::record(&h, b'O', 1, 0, YIELD_PORT.into(), 1);
            assert_eq!(vm.state.witness, chain::record(&h, b'Y', 0, 0, 10, 0));
        }
    }
}
#[test]
fn clock_signal_ports_reject_invalid_accesses() {
    for port in [ACK_PORT, YIELD_PORT, PIC_UNMASK_PORT] {
        for (width, write, value) in [(2, true, 1), (1, false, 0), (1, true, 0), (1, true, 2)] {
            let mut vm = machine();
            vm.native.exits.extend([
                event(32, 3),
                Exit::Port {
                    port,
                    width,
                    write,
                    value,
                },
            ]);
            let error = vm.run(0, 10, 1000, false, &[], Vec::new()).unwrap_err();
            assert_eq!(error, "PV clockevent port access");
            assert_eq!(vm.native.responses.len(), 1);
        }
    }
}
#[test]
fn in_service_line_blocks_reinjection_until_eoi() {
    let mut pic = Pic::default();
    for (port, value) in [(0x20, 0x11), (0x21, 0x30), (0x21, 4), (0x21, 1), (0x21, 0)] {
        pic.write(port, value);
    }
    assert_eq!(pic.next(0b10_0001), Some(0));
    pic.isr |= 1;
    assert_eq!(pic.next(0b10_0001), None, "lower priority waits for EOI");
    pic.write(0x20, 0x0b);
    assert_eq!(pic.read(0x20, 0b10_0001), 1);
    pic.write(0x20, 0x20);
    assert_eq!(pic.isr, 0);
    assert_eq!(pic.next(0b10_0000), Some(5));
    pic.isr |= 1 << 5;
    assert_eq!(pic.next(0b10_0001), Some(0), "higher priority preempts");
    pic.write(0x20, 0x65);
    assert_eq!(pic.isr, 0);
}
#[test]
fn ports_uart_and_power_off_are_stopped_rendezvous() {
    let mut vm = machine();
    vm.native.exits.extend([
        event(32, 3),
        port(0x3f8, true, b'X' as u64),
        port(0x3fd, false, 0),
        Exit::Port {
            port: 0x71,
            width: 2,
            write: false,
            value: 0,
        },
        Exit::Mmio {
            address: 0xfee0_0020,
            width: 4,
            write: false,
            value: 0,
        },
        event(40, 1),
    ]);
    assert!(matches!(
        vm.run(0, 10, 1000, false, &[], Vec::new()).unwrap(),
        Stop::PoweredOff
    ));
    assert_eq!(vm.output, b"X");
    assert_eq!(
        vm.native.responses,
        [None, None, Some(0x60), Some(0xffff), Some(0), None]
    );
}
#[test]
fn terminal_exits_before_handshake_are_errors() {
    for exit in [Exit::Halt { interrupts: false }, Exit::Shutdown, halt()] {
        let mut vm = machine();
        vm.native.exits.extend([exit]);
        assert!(vm.run(0, 10, 1000, false, &[], Vec::new()).is_err());
    }
    // A power-off event before the handshake is a setup failure too.
    let mut vm = machine();
    vm.native.exits.extend([event(40, 1)]);
    assert!(vm.run(0, 10, 1000, false, &[], Vec::new()).is_err());
}
#[test]
fn masked_halt_powers_off_and_triple_fault_resets_after_handshake() {
    for (exit, expected) in [
        (Exit::Halt { interrupts: false }, Stop::PoweredOff),
        (Exit::Shutdown, Stop::Reset),
    ] {
        let mut vm = machine();
        vm.native
            .exits
            .extend([event(32, 3), port(0x3f8, true, b'Z' as u64), exit]);
        let stop = vm.run(0, 10, 1000, false, &[], Vec::new()).unwrap();
        assert_eq!(stop, expected);
        use seedbox_machine::machine::execution_evidence::chain;
        let h = chain::record(
            &chain::initial(),
            b'w',
            8,
            0,
            super::super::EVENT_MMIO + 32,
            3,
        );
        let h = chain::record(&h, b'O', 1, 0, 0x3f8, b'Z' as u64);
        assert_eq!(
            vm.state.witness,
            chain::record(
                &h,
                if expected == Stop::Reset { b'G' } else { b'H' },
                0,
                0,
                0,
                0
            )
        );
        assert_eq!(vm.output, b"Z");
        assert_eq!(vm.native.entries, 3);
        // The ended boot is never entered or captured again.
        assert!(vm.run(0, 10, 1000, false, &[], Vec::new()).is_err());
        assert!(vm.run(10, 10, 1000, false, &[], Vec::new()).is_err());
        assert_eq!(vm.native.entries, 3);
        let directory = tempfile::tempdir().unwrap();
        assert!(vm.capture(&directory.path().join("capture")).is_err());
    }
}
fn disk_read(offset: u64) -> Exit {
    Exit::Mmio {
        address: super::super::DISK_MMIO + offset,
        width: 4,
        write: false,
        value: 0,
    }
}
fn command_line(vm: &Machine<Scripted>) -> Vec<u8> {
    let ram = unsafe { vm.ram.bytes() };
    let at = COMMAND_LINE as usize;
    let length = ram[at..].iter().position(|&byte| byte == 0).unwrap();
    ram[at..at + length].to_vec()
}
#[test]
fn only_a_boot_with_a_disk_registers_and_maps_its_window() {
    let mut vm = machine();
    assert_eq!(command_line(&vm), b"console=ttyS0");
    vm.native.exits.extend([event(32, 3), disk_read(0)]);
    let error = vm.run(0, 10, 1000, false, &[], Vec::new()).unwrap_err();
    assert!(error.contains("unmapped MMIO 0xb020000"), "{error}");

    let mut vm = machine_with(Some(
        seedbox_machine::machine::disk::Store::zeroed(1 << 33).unwrap(),
    ));
    assert_eq!(
        command_line(&vm),
        b"console=ttyS0 virtio_mmio.device=16K@0xb020000:6:1"
    );
    vm.native.exits.extend([
        event(32, 3),
        disk_read(0x00),
        disk_read(0x08),
        disk_read(0x100),
        disk_read(0x104),
        Exit::Halt { interrupts: false },
    ]);
    assert_eq!(
        vm.run(0, 10, 1000, false, &[], Vec::new()).unwrap(),
        Stop::PoweredOff
    );
    assert_eq!(
        vm.native.responses,
        [None, Some(0x7472_6976), Some(2), Some(0), Some(2), None]
    );
    assert_eq!(vm.state.devices.lines & 1 << DISK, 0);
}
#[test]
fn ended_boot_exports_both_disk_layers_unchanged() {
    let mut store = seedbox_machine::machine::disk::Store::zeroed(8).unwrap();
    store.write(1, &[1; 512]).unwrap();
    store.flush();
    store.write(2, &[2; 512]).unwrap();
    let mut vm = machine_with(Some(store.clone()));
    vm.native
        .exits
        .extend([event(32, 3), Exit::Halt { interrupts: false }]);
    assert_eq!(
        vm.run(0, 10, 1000, false, &[], Vec::new()).unwrap(),
        Stop::PoweredOff
    );
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("export");
    vm.export_disk(&path).unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), store.encode_overlays());
    // Exports never replace an existing file or invent a disk.
    assert!(vm.export_disk(&path).is_err());
    assert!(machine()
        .export_disk(&directory.path().join("diskless"))
        .is_err());
}
#[test]
fn interruptible_halt_stays_blocked_not_exited() {
    let mut vm = machine();
    vm.native.exits.extend([event(32, 3), event(0, 50), halt()]);
    assert!(matches!(
        vm.run(0, 10, 1000, false, &[], Vec::new()).unwrap(),
        Stop::Blocked(Some(50))
    ));
    assert!(!vm.ended);
}
fn nic(offset: u64, width: u8, write: bool, value: u64) -> Exit {
    Exit::Mmio {
        address: super::super::NET_MMIO + offset,
        width,
        write,
        value,
    }
}
const MAC: [u8; 6] = [2, 0, 0, 0, 0, 7];
// Guest-physical split rings for RX (queue 0) and TX (queue 1).
const RINGS: [[u64; 3]; 2] = [
    [0x400_0000, 0x400_1000, 0x400_2000],
    [0x400_3000, 0x400_4000, 0x400_5000],
];
/// Negotiate the NIC like Linux and enable both 16-entry queues.
fn nic_setup() -> Vec<Exit> {
    let mut exits = vec![
        nic(0x70, 4, true, 3),
        nic(0x24, 4, true, 0),
        nic(0x20, 4, true, (1 << 3) | (1 << 5)),
        nic(0x24, 4, true, 1),
        nic(0x20, 4, true, 1),
        nic(0x70, 4, true, 11),
    ];
    for (queue, rings) in RINGS.iter().enumerate() {
        exits.extend([nic(0x30, 4, true, queue as u64), nic(0x38, 4, true, 16)]);
        for (low, address) in [0x80, 0x90, 0xa0].into_iter().zip(rings) {
            exits.extend([nic(low, 4, true, *address), nic(low + 4, 4, true, 0)]);
        }
        exits.push(nic(0x44, 4, true, 1));
    }
    exits.push(nic(0x70, 4, true, 15));
    exits
}
/// Offer one descriptor at `index` of `queue`, pointing at `buffer`.
fn offer(vm: &Machine<Scripted>, queue: usize, buffer: u64, bytes: &[u8], writable: bool) {
    unsafe {
        vm.ram.modify(|ram| {
            let [descriptors, available, _] = RINGS[queue].map(|a| a as usize);
            let length = if writable { 1526 } else { bytes.len() };
            ram[buffer as usize..buffer as usize + bytes.len()].copy_from_slice(bytes);
            ram[descriptors..descriptors + 8].copy_from_slice(&buffer.to_le_bytes());
            ram[descriptors + 8..descriptors + 12].copy_from_slice(&(length as u32).to_le_bytes());
            ram[descriptors + 12..descriptors + 14]
                .copy_from_slice(&(writable as u16 * 2).to_le_bytes());
            ram[available + 4..available + 6].copy_from_slice(&0u16.to_le_bytes());
            ram[available + 2..available + 4].copy_from_slice(&1u16.to_le_bytes());
        })
    }
}
#[test]
fn only_a_boot_with_a_nic_registers_and_maps_its_window() {
    let mut vm = machine();
    vm.native.exits.extend([event(32, 3), nic(0, 4, false, 0)]);
    let error = vm.run(0, 10, 1000, false, &[], Vec::new()).unwrap_err();
    assert!(error.contains("unmapped MMIO 0xb030000"), "{error}");
    assert!(machine()
        .run(0, 0, 1000, false, &[], vec![vec![0; 60]])
        .unwrap_err()
        .contains("without a NIC"));

    let vm = machine_full(None, Some(MAC));
    assert_eq!(
        command_line(&vm),
        b"console=ttyS0 virtio_mmio.device=4K@0xb030000:7:2"
    );
    let disk = Some(seedbox_machine::machine::disk::Store::zeroed(8).unwrap());
    assert_eq!(
        command_line(&machine_full(disk, Some(MAC))),
        b"console=ttyS0 virtio_mmio.device=16K@0xb020000:6:1 virtio_mmio.device=4K@0xb030000:7:2"
    );
    let mut vm = machine_full(None, Some(MAC));
    vm.native.exits.extend([
        event(32, 3),
        nic(0x00, 4, false, 0),
        nic(0x08, 4, false, 0),
        nic(0x105, 1, false, 0),
        nic(0x10a, 2, false, 0),
        Exit::Halt { interrupts: false },
    ]);
    assert_eq!(
        vm.run(0, 10, 1000, false, &[], Vec::new()).unwrap(),
        Stop::PoweredOff
    );
    assert_eq!(
        vm.native.responses,
        [None, Some(0x7472_6976), Some(1), Some(7), Some(1500), None]
    );
}
#[test]
fn tx_frames_join_the_write_stream_at_the_notify_and_rx_raises_line_seven() {
    let mut vm = machine_full(None, Some(MAC));
    let frame: Vec<u8> = (0..60).collect();
    let mut packet = vec![0; 12];
    packet.extend(&frame);
    offer(&vm, 1, 0x400_8000, &packet, false);
    vm.native.exits.push_back(event(32, 3));
    vm.native.exits.extend(nic_setup());
    vm.native.exits.extend([
        port(0x3f8, true, b'a' as u64),
        nic(0x50, 4, true, 1),
        port(0x3f8, true, b'b' as u64),
        event(24, 0),
    ]);
    assert_eq!(
        vm.run(5, 10, 1000, false, &[], Vec::new()).unwrap(),
        Stop::Horizon
    );
    use seedbox_machine::pv::transport::{GuestWrite, LifecycleWrite};
    let write = |time, bytes: &[u8]| LifecycleWrite {
        time,
        bytes: bytes.to_vec(),
    };
    assert_eq!(
        std::mem::take(&mut vm.state.writes),
        [
            GuestWrite::Console(write(5, b"a")),
            GuestWrite::Frame(write(5, &frame)),
            GuestWrite::Console(write(5, b"b")),
        ]
    );
    assert_ne!(vm.state.devices.lines & 1 << NET, 0, "TX completion");
    // Acknowledge, then admit a frame with zero budget: the guest buffer
    // fills and the line rises without any guest entry.
    vm.native
        .exits
        .extend([nic(0x64, 4, true, 1), event(24, 0)]);
    vm.run(10, 20, 1000, false, &[], Vec::new()).unwrap();
    assert_eq!(vm.state.devices.lines & 1 << NET, 0);
    offer(&vm, 0, 0x400_9000, &[], true);
    let entries = vm.native.entries;
    let received: Vec<u8> = (100..160).collect();
    assert_eq!(
        vm.run(20, 20, 1000, false, &[], vec![received.clone()])
            .unwrap(),
        Stop::Horizon
    );
    assert_eq!(vm.native.entries, entries);
    assert_ne!(vm.state.devices.lines & 1 << NET, 0, "RX delivery");
    let ram = unsafe { vm.ram.bytes() };
    assert_eq!(ram[0x400_9000 + 10], 1, "num_buffers");
    assert_eq!(&ram[0x400_9000 + 12..0x400_9000 + 72], &received[..]);
    assert!(vm.state.writes.is_empty());
    // Out-of-bounds host frames are caller errors.
    assert!(vm.run(20, 20, 1000, false, &[], vec![vec![0; 13]]).is_err());
}

#[test]
fn actual_native_chain_excludes_host_landing_and_reads_ignore_returned_value() {
    use seedbox_machine::machine::execution_evidence::chain;
    let mut vm = machine();
    vm.state.ready = true;
    vm.native.exits.extend([
        Exit::Interrupted,
        Exit::Port {
            port: 0x3f8,
            width: 1,
            write: true,
            value: 65,
        },
        Exit::Port {
            port: 0x3fd,
            width: 1,
            write: false,
            value: 999,
        },
        Exit::Halt { interrupts: true },
    ]);
    assert!(matches!(
        vm.run(0, 10, 1000, false, &[], Vec::new()).unwrap(),
        Stop::Blocked(_)
    ));
    let expected = chain::record(
        &chain::record(
            &chain::record(&chain::initial(), b'O', 1, 0, 0x3f8, 65),
            b'i',
            1,
            0,
            0x3fd,
            0,
        ),
        b'H',
        0,
        0,
        0,
        0,
    );
    assert_eq!(vm.state.witness, expected);
    let before = (
        vm.native.entries,
        vm.state.now,
        vm.state.writes.len(),
        vm.output.clone(),
    );
    let a = vm.native_execution().unwrap();
    let b = vm.native_execution().unwrap();
    assert_eq!(a.chain, b.chain);
    assert_eq!(a.pending, b.pending);
    assert_eq!(
        chain::observe(&a.chain, &a.pending),
        chain::observe(&b.chain, &b.pending)
    );
    assert_eq!(
        before,
        (
            vm.native.entries,
            vm.state.now,
            vm.state.writes.len(),
            vm.output.clone()
        )
    );
    assert_eq!(vm.output, b"A");
}

#[test]
fn canonical_pending_native_writes_preserve_channel_order_and_opaque_json_bytes() {
    use seedbox_machine::machine::execution_evidence::chain;
    use seedbox_machine::pv::transport::{GuestWrite, LifecycleWrite};
    let mut vm = machine();
    let a = GuestWrite::Lifecycle(LifecycleWrite {
        time: 7,
        bytes: br#"{"z":1.25,"a":2}"#.to_vec(),
    });
    let b = GuestWrite::Console(LifecycleWrite {
        time: 7,
        bytes: b"same-time".to_vec(),
    });
    vm.state.writes = vec![a.clone(), b.clone()];
    let first = vm.native_execution().unwrap();
    vm.state.writes = vec![b, a];
    let reordered = vm.native_execution().unwrap();
    assert_ne!(
        chain::observe(&first.chain, &first.pending),
        chain::observe(&reordered.chain, &reordered.pending)
    );
    vm.state.writes = vec![
        GuestWrite::Lifecycle(LifecycleWrite {
            time: 7,
            bytes: br#"{"a":2,"z":1.25}"#.to_vec(),
        }),
        vm.state.writes[0].clone(),
    ];
    let rewritten = vm.native_execution().unwrap();
    assert_ne!(first.pending, rewritten.pending);
    assert_eq!(vm.native.entries, 0);
}

#[test]
fn shared_pic_mask_ack_waits_for_unmask_and_repeated_level_injects_once() {
    let mut vm = machine();
    vm.state.ready = true;
    vm.state.timer = true;
    vm.state.devices.pic.imr = 0xfe;
    vm.native.interruptible.extend([true, false, true]);
    vm.native.publications.insert(1, PIC_UPDATE_VALID | 0x1ff);
    vm.native.publications.insert(2, PIC_UPDATE_VALID | 0xfe);
    vm.native.publications.insert(4, PIC_UPDATE_VALID | 0x1ff);
    vm.native.exits.extend([
        // Mask+EOI becomes visible at the handler's device access. The timer
        // remains asserted: no new injection until the explicit unmask.
        port(0x3fd, false, 0),
        port(PIC_UNMASK_PORT, true, 1),
        Exit::Window,
        port(ACK_PORT, true, 1),
        halt(),
    ]);
    assert_eq!(
        vm.run(5, 100, 1000, false, &[], vec![]).unwrap(),
        Stop::Blocked(None)
    );
    assert_eq!(vm.native.injected, [0x30, 0x30]);
    assert_eq!(vm.state.devices.pic.isr, 0);
    assert_eq!(vm.page.word(PIC_UPDATE), 0);
    assert_eq!(vm.state.now, 5);
}

#[test]
fn shared_pic_old_ack_is_consumed_before_due_time_and_new_receive() {
    let mut vm = machine_full(None, Some(MAC));
    vm.state.ready = true;
    vm.native.exits.extend(nic_setup());
    vm.native.exits.push_back(event(24, 0));
    vm.run(0, 1, 1000, false, &[], vec![]).unwrap();
    offer(&vm, 0, 0x400_9000, &[], true);
    vm.state.devices.pic.isr = 0x81;
    vm.state.deadline = Some(5);
    vm.page.set_word(PIC_UPDATE, PIC_UPDATE_VALID | 0x817e);
    // Old PIC EOI may clear old in-service bits, never the newly admitted
    // timer or NIC level. Zero-budget adoption must consume it without entry.
    let entries = vm.native.entries;
    vm.run(5, 5, 1000, false, &[], vec![vec![42; 60]]).unwrap();
    assert_eq!(vm.native.entries, entries);
    assert_eq!(vm.state.devices.lines & 0x81, 0x81);
    assert_eq!(vm.state.devices.pic.isr, 0);
    assert_eq!(vm.page.word(PIC_UPDATE), 0);
    vm.native.interruptible.push_back(true);
    vm.prepare().unwrap();
    assert_eq!(vm.native.injected, [0x30]);
    vm.consume_irq_update().unwrap();
    assert_eq!(
        vm.state.devices.pic.isr, 1,
        "consumed ACK cannot clear a new injection"
    );
}

#[test]
fn shared_pic_pending_transaction_survives_stopped_state_roundtrip() {
    let mut vm = machine();
    vm.state.devices.pic.isr = 1;
    vm.page.set_word(PIC_UPDATE, PIC_UPDATE_VALID | 0x1ff);
    let mut restored = machine();
    // Snapshot stores State and the complete shared page separately. Retain
    // both, including an unconsumed transaction; never republish a cached mask.
    restored.state = serde_json::from_slice(&serde_json::to_vec(&vm.state).unwrap()).unwrap();
    unsafe { restored.page.modify(|p| p.copy_from_slice(vm.page.bytes())) };
    restored.consume_irq_update().unwrap();
    assert_eq!(restored.state.devices.pic.imr, 0xff);
    assert_eq!(restored.state.devices.pic.isr, 0);
    assert_eq!(restored.page.word(PIC_UPDATE), 0);
    let witness = restored.state.witness;
    restored.consume_irq_update().unwrap();
    assert_eq!(restored.state.witness, witness);
}

#[test]
fn shared_virtio_old_ack_precedes_zero_budget_receive_and_notify() {
    let mut vm = machine_full(None, Some(MAC));
    vm.state.ready = true;
    vm.native.exits.extend(nic_setup());
    vm.native.exits.push_back(event(24, 0));
    vm.run(0, 1, 1000, false, &[], vec![]).unwrap();
    offer(&vm, 0, 0x400_9000, &[], true);
    vm.run(5, 5, 1000, false, &[], vec![vec![42; 60]]).unwrap();
    assert_eq!(vm.page.word(VIRTIO_IRQ + 16), 1);
    // Reoffer descriptor 0 in the next available slot, as a guest refill does.
    unsafe {
        vm.ram.modify(|ram| {
            ram[RINGS[0][1] as usize + 2..RINGS[0][1] as usize + 4]
                .copy_from_slice(&2u16.to_le_bytes());
        })
    };
    vm.page.set_word(VIRTIO_IRQ + 16, 1 | (1 << 32));
    vm.page.set_word(PIC_UPDATE, PIC_UPDATE_VALID | 0x807f);
    vm.state.devices.pic.isr = 0x80;
    let entries = vm.native.entries;
    vm.run(5, 5, 1000, false, &[], vec![vec![43; 60]]).unwrap();
    assert_eq!(vm.native.entries, entries);
    assert_eq!(vm.page.word(VIRTIO_IRQ + 16), 1);
    assert_eq!(vm.state.devices.lines & 0x80, 0x80);
    assert_eq!(vm.state.devices.pic.isr, 0);
    assert_eq!(unsafe { vm.ram.bytes()[0x400_9000 + 12] }, 43);
    // A repeated admission with no available buffer is backlogged; no old ACK
    // remains to clear the current completion. Host observation changes nothing.
    let before = vm.native_execution().unwrap();
    let after = vm.native_execution().unwrap();
    assert_eq!(before.chain, after.chain);
    assert_eq!(before.pending, after.pending);
    vm.run(5, 5, 1000, false, &[], vec![vec![44; 60]]).unwrap();
    assert_eq!(vm.page.word(VIRTIO_IRQ + 16), 1);
    let mut packet = vec![0; 12];
    packet.extend([45; 60]);
    offer(&vm, 1, 0x400_8000, &packet, false);
    vm.native
        .virtio_publications
        .insert(entries + 1, (2, 1 | (1 << 32)));
    vm.native
        .exits
        .extend([nic(0x50, 4, true, 1), event(24, 0)]);
    vm.run(5, 6, 1000, false, &[], vec![]).unwrap();
    // The old RX ACK was consumed before TX notify reasserted the same bit.
    assert_eq!(vm.page.word(VIRTIO_IRQ + 16), 1);
    let frames: Vec<_> = vm
        .state
        .writes
        .iter()
        .filter_map(|w| match w {
            seedbox_machine::pv::transport::GuestWrite::Frame(f) => Some(f),
            _ => None,
        })
        .collect();
    assert_eq!(frames.len(), 1);
    assert_eq!(frames[0].time, 5);
    assert_eq!(frames[0].bytes, [45; 60]);
}

#[test]
fn shared_virtio_ack_bits_roundtrip_and_host_interrupts_are_invisible() {
    let mut vm = machine_full(None, Some(MAC));
    vm.state.ready = true;
    // Exercise bitwise accumulation independently of the current devices,
    // which only generate used-ring interrupts (configuration is static).
    let mut net = serde_json::to_value(vm.state.net.as_ref().unwrap()).unwrap();
    net["interrupt"] = 3.into();
    vm.state.net = Some(serde_json::from_value(net).unwrap());
    vm.publish(10);
    vm.page.set_word(VIRTIO_IRQ + 16, 3 | (1 << 32));
    let first = vm.native_execution().unwrap();
    let second = vm.native_execution().unwrap();
    assert_eq!(first.chain, second.chain);
    assert_eq!(first.pending, second.pending);
    assert_eq!(vm.page.word(VIRTIO_IRQ + 16), 3 | (1 << 32));
    assert_eq!(vm.state.net.as_ref().unwrap().interrupt_status(), 3);
    let mut restored = machine_full(None, Some(MAC));
    restored.state = serde_json::from_slice(&serde_json::to_vec(&vm.state).unwrap()).unwrap();
    unsafe { restored.page.modify(|p| p.copy_from_slice(vm.page.bytes())) };
    for machine in [&mut vm, &mut restored] {
        machine.consume_irq_update().unwrap();
        machine.publish(10);
        assert_eq!(machine.page.word(VIRTIO_IRQ + 16), 2);
        // Guest accumulates both bits. A host interruption after publication
        // must not consume it; the next semantic exit records the actual word.
        machine
            .native
            .virtio_publications
            .insert(1, (2, 2 | (3 << 32)));
        machine
            .native
            .exits
            .extend([Exit::Interrupted, event(24, 0)]);
        machine.run(0, 10, 1000, false, &[], vec![]).unwrap();
        assert_eq!(machine.page.word(VIRTIO_IRQ + 16), 0);
        assert_eq!(machine.native.nic_words_at_entry, [2 | (3 << 32); 2]);
        let witness = machine.state.witness;
        machine.consume_irq_update().unwrap();
        assert_eq!(machine.state.witness, witness);
    }
    assert_eq!(vm.state.witness, restored.state.witness);
    assert_eq!(unsafe { vm.page.bytes() }, unsafe { restored.page.bytes() });
}
