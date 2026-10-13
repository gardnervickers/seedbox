//! x86-64 platform of the stopped machine: bzImage 64-bit boot protocol, an
//! 8259 PIC, a 16550 console UART and the PV power-off register, on Linux KVM
//! with a userspace interrupt controller.
//!
//! KVM keeps no in-kernel LAPIC, IOAPIC or PIT, so no device runs on host time.
//! Interrupt injection is instruction-exact: a vector is injected immediately
//! before guest entry when the vCPU can accept it; otherwise an interrupt-window
//! exit stops the guest at the first instruction boundary where it can. The
//! ARM deferred-rendezvous page word therefore stays zero on x86.
//!
//! CPUID hides the APIC, RDRAND/RDSEED, RDPID, MONITOR/WAITPKG, TSC_ADJUST,
//! host frequency leaves, performance counters and hypervisor leaves, and the
//! VM has no vPMU. AMD cannot trap
//! hidden RDRAND/RDSEED, so the mandatory instrumentation replaces them in
//! applications with guest getrandom draws; uninstrumented guest code that
//! executes them regardless of CPUID still reads host entropy (the kernel only
//! reaches them behind CPUID checks). Kernel TSC reads are PV time and CR4.TSD
//! traps application RDTSC/RDTSCP. A guest reset or panic reboot ends in an
//! architectural shutdown exit, which the worker reports as an error.
#[path = "kvm_x86.rs"]
mod backend;
use super::{
    boot,
    memory::{self, Memory},
    Effect, Machine, Result,
};
pub(super) use backend::{Architecture, Backend, Exit, Native};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
pub(super) const TIMER: u32 = 0;
const UART: u32 = 4;
const CONSOLE: u32 = 5;
const POWER_OFF: u64 = 40;
const LAPIC: u64 = 0xfee0_0000;
const LAPIC_END: u64 = LAPIC + 0xfff;
const GDT: u64 = 0x500;
const PML4: u64 = 0x9000;
const ZERO_PAGE: u64 = 0x1_0000;
const COMMAND_LINE: u64 = 0x2_0000;
const SETUP_DATA: u64 = 0x3_0000;
const STACK: u64 = 0x8000;
const CODE_SELECTOR: u16 = 0x10;
const DATA_SELECTOR: u16 = 0x18;

/// Master and cascaded slave 8259 in fully nested mode. Lines are levels that
/// the stopped machine recomputes at every rendezvous.
#[derive(Clone, Serialize, Deserialize)]
struct Pic {
    imr: u8,
    isr: u8,
    base: u8,
    icw: u8,
    read_isr: bool,
    slave_imr: u8,
    slave_icw: u8,
}
impl Default for Pic {
    fn default() -> Self {
        Self {
            imr: 0xff,
            isr: 0,
            base: 0x30,
            icw: 0,
            read_isr: false,
            slave_imr: 0xff,
            slave_icw: 0,
        }
    }
}
impl Pic {
    /// Highest-priority unmasked line above everything in service.
    fn next(&self, lines: u8) -> Option<u8> {
        let candidate = lines & !self.imr & !self.isr;
        if candidate == 0 {
            return None;
        }
        let line = candidate.trailing_zeros() as u8;
        (line < self.isr.trailing_zeros() as u8).then_some(line)
    }
    fn write(&mut self, port: u16, value: u8) {
        match port {
            0x20 if value & 0x10 != 0 => {
                // ICW1: reinitialize; ICW2..4 follow on the data port.
                self.icw = 1;
                self.imr = 0;
                self.isr = 0;
                self.read_isr = false;
            }
            0x20 if value & 0x08 != 0 => {
                // OCW3: select IRR or ISR for command-port reads.
                if value & 2 != 0 {
                    self.read_isr = value & 1 != 0;
                }
            }
            0x20 => match value & 0xe0 {
                // OCW2 non-specific EOI clears the highest-priority in-service line.
                0x20 | 0xa0 => self.isr &= self.isr.wrapping_sub(1),
                // OCW2 specific EOI.
                0x60 | 0xe0 => self.isr &= !(1 << (value & 7)),
                _ => (),
            },
            0x21 => match self.icw {
                1 => {
                    self.base = value & 0xf8;
                    self.icw = 2;
                }
                2 => self.icw = 3,
                3 => self.icw = 0,
                _ => self.imr = value,
            },
            0xa0 if value & 0x10 != 0 => self.slave_icw = 1,
            0xa1 => match self.slave_icw {
                1 => self.slave_icw = 2,
                2 => self.slave_icw = 3,
                3 => self.slave_icw = 0,
                _ => self.slave_imr = value,
            },
            _ => (),
        }
    }
    fn read(&self, port: u16, lines: u8) -> u8 {
        match port {
            0x20 if self.read_isr => self.isr,
            0x20 => lines,
            0x21 => self.imr,
            0xa1 => self.slave_imr,
            _ => 0,
        }
    }
}

/// 16550A transmitter with immediate completion; no receive path is wired.
#[derive(Clone, Default, Serialize, Deserialize)]
struct Uart {
    ier: u8,
    lcr: u8,
    mcr: u8,
    scr: u8,
    dll: u8,
    dlm: u8,
    thre: bool,
}
impl Uart {
    fn dlab(&self) -> bool {
        self.lcr & 0x80 != 0
    }
    fn level(&self) -> bool {
        self.ier & 2 != 0 && self.thre
    }
}

/// Interrupt-controller and UART state captured with the stopped machine.
#[derive(Clone, Default, Serialize, Deserialize)]
pub(super) struct Devices {
    pic: Pic,
    uart: Uart,
    lines: u8,
}
impl Devices {
    /// No unmasked interrupt is waiting for delivery.
    pub(super) fn quiet(&self) -> bool {
        self.pic.next(self.lines).is_none()
    }
    /// x86 delivery never waits for a PV clock rendezvous.
    pub(super) fn deferred(&self) -> bool {
        false
    }
}

pub(super) struct Entry {
    rip: u64,
}

fn put(ram: &mut Memory, address: u64, bytes: &[u8]) -> Result<()> {
    ram.write((address - boot::RAM_BASE) as usize, bytes)
}

/// Place the payload, boot parameters, identity page tables and initrd in RAM.
pub(super) fn load(
    ram: &mut Memory,
    kernel: &[u8],
    initrd: &[u8],
    seed: u64,
    command_line: &str,
) -> Result<Entry> {
    let (setup, initrd_address) = boot::layout(kernel, initrd)?;
    put(ram, boot::KERNEL, &kernel[setup..])?;
    put(ram, initrd_address, initrd)?;
    // Flat 64-bit code and data descriptors.
    let mut gdt = Vec::new();
    for descriptor in [
        0u64,
        0,
        0x00af_9b00_0000_ffff, // 0x10: 64-bit code.
        0x00cf_9300_0000_ffff, // 0x18: data.
    ] {
        gdt.extend(descriptor.to_le_bytes());
    }
    put(ram, GDT, &gdt)?;
    // Identity-map the first 4 GiB with 2 MiB pages, covering RAM and PV MMIO.
    let mut tables = vec![0u8; 6 * 4096];
    tables[..8].copy_from_slice(&((PML4 + 0x1000) | 3).to_le_bytes());
    for directory in 0..4u64 {
        let entry = (PML4 + 0x2000 + directory * 0x1000) | 3;
        let at = 0x1000 + directory as usize * 8;
        tables[at..at + 8].copy_from_slice(&entry.to_le_bytes());
        for page in 0..512u64 {
            let address = ((directory * 512 + page) << 21) | 0x83;
            let at = 0x2000 + directory as usize * 0x1000 + page as usize * 8;
            tables[at..at + 8].copy_from_slice(&address.to_le_bytes());
        }
    }
    put(ram, PML4, &tables)?;
    // Zero page: the image's setup header plus loader-owned fields.
    let mut zero = vec![0u8; 4096];
    let header_end = (0x202 + kernel[0x201] as usize).clamp(0x268, 0x290.min(kernel.len()));
    zero[0x1f1..header_end].copy_from_slice(&kernel[0x1f1..header_end]);
    zero[0x210] = 0xff; // type_of_loader: undefined.
    zero[0x211] |= 1; // LOADED_HIGH.
    let set = |zero: &mut [u8], at: usize, bytes: &[u8]| {
        zero[at..at + bytes.len()].copy_from_slice(bytes);
    };
    set(&mut zero, 0x218, &(initrd_address as u32).to_le_bytes());
    set(&mut zero, 0x21c, &(initrd.len() as u32).to_le_bytes());
    set(&mut zero, 0x228, &(COMMAND_LINE as u32).to_le_bytes());
    set(&mut zero, 0x250, &SETUP_DATA.to_le_bytes());
    let top = boot::RAM_BASE + boot::RAM_BYTES as u64;
    let memory = [(0u64, 0x9_fc00u64), (boot::KERNEL, top - boot::KERNEL)];
    zero[0x1e8] = memory.len() as u8;
    for (index, (address, size)) in memory.iter().enumerate() {
        let at = 0x2d0 + index * 20;
        set(&mut zero, at, &address.to_le_bytes());
        set(&mut zero, at + 8, &size.to_le_bytes());
        set(&mut zero, at + 16, &1u32.to_le_bytes());
    }
    put(ram, ZERO_PAGE, &zero)?;
    let mut arguments = command_line.as_bytes().to_vec();
    arguments.push(0);
    put(ram, COMMAND_LINE, &arguments)?;
    // SETUP_RNG_SEED with the same seeded entropy domain as the ARM device tree.
    let entropy = Sha256::digest([b"seedbox pv boot v1".as_slice(), &seed.to_le_bytes()].concat());
    let mut data = Vec::new();
    data.extend(0u64.to_le_bytes());
    data.extend(9u32.to_le_bytes());
    data.extend((entropy.len() as u32).to_le_bytes());
    data.extend(entropy);
    put(ram, SETUP_DATA, &data)?;
    Ok(Entry {
        rip: boot::KERNEL + 0x200,
    })
}
pub(super) fn reset<B: Backend>(native: &mut B, entry: &Entry) -> Result<()> {
    native.boot(backend::Boot {
        rip: entry.rip,
        rsi: ZERO_PAGE,
        rsp: STACK,
        cr3: PML4,
        gdt: GDT,
        code: CODE_SELECTOR,
        data: DATA_SELECTOR,
    })
}
impl<B: Backend> Machine<B> {
    /// x86 has no software reservation epoch: rendezvous change no page word.
    pub(super) fn boundary(&mut self) -> Result<()> {
        Ok(())
    }
    /// Lines are recomputed from device state by `interrupts`.
    pub(super) fn lower(&mut self, _number: u32) -> Result<()> {
        self.interrupts()
    }
    pub(super) fn interrupts(&mut self) -> Result<()> {
        if self.state.deadline.is_some_and(|d| d <= self.state.now) {
            self.state.deadline = None;
            self.state.timer = true;
        }
        let devices = &mut self.state.devices;
        devices.lines = (self.state.timer as u8) << TIMER
            | (devices.uart.level() as u8) << UART
            | (self.state.console.irq() as u8) << CONSOLE;
        Ok(())
    }
    /// Inject the next deliverable interrupt immediately before guest entry.
    pub(super) fn prepare(&mut self) -> Result<()> {
        let devices = &mut self.state.devices;
        let next = devices.pic.next(devices.lines);
        match next {
            Some(line) if self.native.interruptible() => {
                self.native.inject(devices.pic.base.wrapping_add(line))?;
                devices.pic.isr |= 1 << line;
                self.native.request_window(false);
            }
            Some(_) => self.native.request_window(true),
            None => self.native.request_window(false),
        }
        Ok(())
    }
    pub(super) fn maintenance(&mut self, _exit: &Exit) -> Result<bool> {
        Ok(false)
    }
    /// The x86 kernel powers off through a PV event register (no PSCI/ACPI).
    pub(super) fn event_register(&mut self, offset: u64, value: u64) -> Result<bool> {
        if offset == POWER_OFF && value == 1 {
            return Ok(true);
        }
        Err("PV unknown clockevent register".into())
    }
    fn uart(&mut self, offset: u16, write: bool, value: u8) -> u8 {
        let uart = &mut self.state.devices.uart;
        if write {
            match offset {
                0 if uart.dlab() => uart.dll = value,
                0 => {
                    uart.thre = true;
                    self.console_byte(value);
                    return 0;
                }
                1 if uart.dlab() => uart.dlm = value,
                1 => {
                    if value & 2 != 0 && uart.ier & 2 == 0 {
                        uart.thre = true;
                    }
                    uart.ier = value & 0x0f;
                }
                3 => uart.lcr = value,
                4 => uart.mcr = value,
                7 => uart.scr = value,
                _ => (),
            }
            return 0;
        }
        match offset {
            0 if uart.dlab() => uart.dll,
            0 => 0,
            1 if uart.dlab() => uart.dlm,
            1 => uart.ier,
            2 => {
                if uart.level() {
                    uart.thre = false;
                    0xc2
                } else {
                    0xc1
                }
            }
            3 => uart.lcr,
            4 => uart.mcr,
            5 => 0x60,
            6 if uart.mcr & 0x10 != 0 => {
                let m = uart.mcr;
                ((m >> 1) & 1) << 4 | (m & 1) << 5 | ((m >> 2) & 1) << 6 | ((m >> 3) & 1) << 7
            }
            6 => 0xb0,
            _ => uart.scr,
        }
    }
    pub(super) fn platform(&mut self, exit: Exit) -> Result<Effect> {
        let mut effect = Effect::default();
        match exit {
            Exit::Port {
                port,
                width,
                write,
                value,
            } => {
                let byte = match port {
                    0x3f8..=0x3ff if width == 1 => self.uart(port - 0x3f8, write, value as u8),
                    0x20 | 0x21 | 0xa0 | 0xa1 if width == 1 => {
                        let devices = &mut self.state.devices;
                        if write {
                            devices.pic.write(port, value as u8);
                            0
                        } else {
                            devices.pic.read(port, devices.lines)
                        }
                    }
                    // Absent legacy devices read as floating bus; writes are ignored.
                    _ => 0xff,
                };
                if !write {
                    effect.response =
                        Some(u64::from_le_bytes([byte; 8]) & (u64::MAX >> (64 - 8 * width as u32)));
                }
            }
            Exit::Halt { interrupts } => {
                if !self.state.ready {
                    return Err("PV HLT before handshake".into());
                }
                if !interrupts {
                    return Err("PV guest halted with interrupts disabled".into());
                }
                effect.idle = true;
            }
            Exit::Window => (),
            Exit::Shutdown => return Err("PV x86 guest shutdown (triple fault or reset)".into()),
            // CPUID reports no local APIC, but Linux's CPU identification still
            // reads APIC registers through the default driver before falling
            // back to PIC mode. The absent APIC reads as zero (APIC ID 0).
            Exit::Mmio {
                address: LAPIC..=LAPIC_END,
                write: false,
                ..
            } => effect.response = Some(0),
            Exit::Mmio { address, .. } => {
                return Err(format!(
                    "PV unmapped MMIO {address:#x} at {:#x}",
                    self.native.location()
                ))
            }
            Exit::Interrupted => unreachable!(),
        }
        Ok(effect)
    }
}

#[cfg(test)]
mod tests {
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
    }
    impl Backend for Scripted {
        fn create(_: Rc<Memory>, _: Rc<Memory>) -> Result<Self> {
            Ok(Self::default())
        }
        fn boot(&mut self, _: backend::Boot) -> Result<()> {
            Ok(())
        }
        fn enter(&mut self) -> Result<Exit> {
            self.entries += 1;
            self.exits
                .pop_front()
                .ok_or("unexpected scripted entry".into())
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
        fn capture(&mut self) -> Result<Architecture> {
            Err("script capture unused".into())
        }
        fn restore(&mut self, _: &Architecture) -> Result<()> {
            Err("script restore unused".into())
        }
    }
    fn machine() -> Machine<Scripted> {
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
        Machine::boot(&image, b"initrd", 7, "").unwrap()
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
    fn host_interruptions_neither_consume_time_nor_inject() {
        let mut vm = machine();
        vm.native
            .exits
            .extend([Exit::Interrupted, Exit::Interrupted, event(32, 3), halt()]);
        assert!(matches!(
            vm.run(0, 10, 1000, false, &[]).unwrap(),
            Stop::Blocked(None)
        ));
        assert_eq!(vm.state.now, 0);
        assert_eq!(vm.native.entries, 4);
        assert!(vm.native.injected.is_empty());
        let entries = vm.native.entries;
        assert!(matches!(
            vm.run(25, 25, 1000, false, &[]).unwrap(),
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
            vm.run(20, 20, 1000, false, &[]).unwrap(),
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
            vm.run(0, 100, 1000, false, &[]).unwrap(),
            Stop::Blocked(Some(5))
        ));
        assert_eq!(vm.state.devices.pic.imr, 0xce);
        // Zero-budget adoption raises the line without entering or injecting.
        assert!(matches!(
            vm.run(5, 5, 1000, false, &[]).unwrap(),
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
            event(8, 1),
            port(0x21, true, 0xce),
            halt(),
        ]);
        assert!(matches!(
            vm.run(5, 100, 1000, false, &[]).unwrap(),
            Stop::Blocked(None)
        ));
        assert_eq!(vm.native.entries, entries + 6);
        assert_eq!(vm.native.injected, [0x30]);
        assert_eq!(vm.state.devices.pic.isr, 0);
        assert_eq!(vm.state.devices.lines, 0);
        assert!(!vm.native.window);
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
            vm.run(0, 10, 1000, false, &[]).unwrap(),
            Stop::PoweredOff
        ));
        assert_eq!(vm.output, b"X");
        assert_eq!(
            vm.native.responses,
            [None, None, Some(0x60), Some(0xffff), Some(0), None]
        );
    }
    #[test]
    fn masked_halt_and_shutdown_are_errors() {
        let mut vm = machine();
        vm.native
            .exits
            .extend([event(32, 3), Exit::Halt { interrupts: false }]);
        assert!(vm.run(0, 10, 1000, false, &[]).is_err());
        let mut vm = machine();
        vm.native.exits.extend([halt()]);
        assert!(vm.run(0, 10, 1000, false, &[]).is_err());
        let mut vm = machine();
        vm.native.exits.extend([event(32, 3), Exit::Shutdown]);
        assert!(vm.run(0, 10, 1000, false, &[]).is_err());
    }
}
