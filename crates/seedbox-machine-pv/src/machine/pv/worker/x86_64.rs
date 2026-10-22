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
//! traps application RDTSC/RDTSCP.
//!
//! Guest terminal requests after the ready handshake end the boot: a triple
//! fault (KVM shutdown exit) is the platform's only reset, reached by a
//! deliberate reboot or a `panic=-1` restart, and is reported as a reset; HLT
//! with interrupts disabled can never resume (there is no NMI source) and is
//! reported like power-off. Before the handshake both are setup errors.
mod backend;
const ACK_PORT: u16 = 0x500;
const YIELD_PORT: u16 = 0x501;
const PIC_UNMASK_PORT: u16 = 0x502;
// Guest-owned pending transaction, not another interrupt controller. Low byte
// replaces IMR, next byte acknowledges ISR bits, bit 16 marks a publication.
// The stopped worker clears it after applying it to the authoritative PIC.
const PIC_UPDATE: usize = 56;
const PIC_UPDATE_VALID: u64 = 1 << 16;

use super::{
    boot,
    memory::{self, Memory},
    protocol::Stop,
    Effect, Machine, Result,
};
pub(super) use backend::{Architecture, Backend, Exit, Native};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
pub(super) const TIMER: u32 = 0;
const UART: u32 = 4;
const CONSOLE: u32 = 5;
/// The optional block device's PIC line, announced on the command line.
const DISK: u32 = 6;
/// Kernel argument registering the block device window; the PV kernel has
/// no device tree on x86. Platform id 1: the kernel registers the lifecycle
/// console as `virtio-mmio.0` before parsing device-level arguments.
const DISK_ARGUMENT: &str = " virtio_mmio.device=16K@0xb020000:6:1";
/// The optional NIC's PIC line, announced on the command line.
const NET: u32 = 7;
/// Kernel argument registering the NIC window. Platform id 2 stays distinct
/// from the console (0) and disk (1) whether or not a disk is present.
const NET_ARGUMENT: &str = " virtio_mmio.device=4K@0xb030000:7:2";
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
    let offset = ram.layout()?.offset(address, bytes.len())?;
    ram.write(offset, bytes)
}

/// Place the payload, boot parameters, identity page tables and initrd in RAM.
pub(super) fn load(
    ram: &mut Memory,
    kernel: &[u8],
    initrd: &[u8],
    seed: u64,
    command_line: &str,
    disk: bool,
    network: bool,
) -> Result<Entry> {
    let (setup, initrd_address) = boot::layout_for(ram.layout()?, kernel, initrd)?;
    let setup = usize::try_from(setup).map_err(|_| "PV setup offset overflow")?;
    let command_line = if disk || network {
        let mut arguments = command_line.to_owned();
        if disk {
            arguments.push_str(DISK_ARGUMENT);
        }
        if network {
            arguments.push_str(NET_ARGUMENT);
        }
        boot::arguments(&arguments)?;
        std::borrow::Cow::Owned(arguments)
    } else {
        std::borrow::Cow::Borrowed(command_line)
    };
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
    let mut memory = vec![(0u64, 0x9_fc00u64)];
    for segment in &ram.layout()?.segments {
        let start = segment.start.max(boot::KERNEL);
        if segment.end > start {
            memory.push((start, segment.end - start));
        }
    }
    // E820 omits reserved apertures; Linux must never allocate them as RAM.
    if memory.len() > 128 {
        return Err("PV E820 table overflow".into());
    }
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
    /// Consume a single-CPU guest publication before new input, time adoption
    /// or injection. Guest IRQs stay disabled across publication and the
    /// unconditional unmask OUT; other semantic exits may consume it earlier.
    /// Snapshot RAM retains an interrupted publication exactly until consumption.
    pub(super) fn consume_irq_update(&mut self) -> Result<()> {
        let update = self.page.word(PIC_UPDATE);
        if update == 0 {
            return Ok(());
        }
        if update & PIC_UPDATE_VALID == 0 || update >> 17 != 0 {
            return Err("PV invalid shared PIC update".into());
        }
        self.state.devices.pic.imr = update as u8;
        self.state.devices.pic.isr &= !((update >> 8) as u8);
        self.page.set_word(PIC_UPDATE, 0);
        // Record the actual observed transaction, never synthetic port exits.
        self.record_native(b'q', 8, PIC_UPDATE as u64, update);
        Ok(())
    }
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
            | (self.state.console.irq() as u8) << CONSOLE
            | (self.block.as_ref().is_some_and(|block| block.irq()) as u8) << DISK
            | (self.state.net.as_ref().is_some_and(|net| net.irq()) as u8) << NET;
        Ok(())
    }
    /// Inject the next deliverable interrupt immediately before guest entry.
    pub(super) fn prepare(&mut self) -> Result<()> {
        let devices = &mut self.state.devices;
        let next = devices.pic.next(devices.lines);
        match next {
            Some(line) if self.native.interruptible() => {
                let vector = devices.pic.base.wrapping_add(line);
                self.native.inject(vector)?;
                self.state.witness = seedbox_machine::machine::execution_evidence::chain::record(
                    &self.state.witness,
                    b'I',
                    0,
                    self.state.now,
                    vector.into(),
                    0,
                );
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
    pub(super) fn platform(&mut self, exit: Exit, upto: u64) -> Result<Effect> {
        let mut effect = Effect::default();
        match exit {
            Exit::Port {
                port,
                width,
                write,
                value,
            } if matches!(port, ACK_PORT | YIELD_PORT | PIC_UNMASK_PORT) => {
                if width != 1 || !write || value != 1 {
                    return Err("PV clockevent port access".into());
                }
                if port != PIC_UNMASK_PORT {
                    return self.clock_event(if port == ACK_PORT { 8 } else { 24 }, value, upto);
                }
                // The run loop already consumed the shared PIC update.
            }
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
                if interrupts {
                    effect.idle = true;
                } else {
                    // Linux halt (stop_this_cpu): permanently halted.
                    effect.terminal = Some(Stop::PoweredOff);
                }
            }
            Exit::Window => (),
            // Triple fault: the only architectural reset on this platform.
            Exit::Shutdown => effect.terminal = Some(Stop::Reset),
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
mod resource_tests;

#[cfg(test)]
mod tests;

// Closed persisted record: exhaustive destructuring prevents silently omitting new fields.
impl seedbox_machine::snapshot::Record for Pic {
    const MIN: u64 = <u8 as seedbox_machine::snapshot::Record>::MIN
        + <u8 as seedbox_machine::snapshot::Record>::MIN
        + <u8 as seedbox_machine::snapshot::Record>::MIN
        + <u8 as seedbox_machine::snapshot::Record>::MIN
        + <bool as seedbox_machine::snapshot::Record>::MIN
        + <u8 as seedbox_machine::snapshot::Record>::MIN
        + <u8 as seedbox_machine::snapshot::Record>::MIN;
    fn encode<W: std::io::Write>(
        &self,
        w: &mut seedbox_machine::snapshot::Writer<'_, W>,
    ) -> seedbox_machine::snapshot::Result<()> {
        let Self {
            imr,
            isr,
            base,
            icw,
            read_isr,
            slave_imr,
            slave_icw,
        } = self;
        imr.encode(w)?;
        isr.encode(w)?;
        base.encode(w)?;
        icw.encode(w)?;
        read_isr.encode(w)?;
        slave_imr.encode(w)?;
        slave_icw.encode(w)?;
        Ok(())
    }
    fn decode<R: std::io::Read>(
        r: &mut seedbox_machine::snapshot::Reader<'_, R>,
    ) -> seedbox_machine::snapshot::Result<Self> {
        Ok(Self {
            imr: seedbox_machine::snapshot::Record::decode(r)?,
            isr: seedbox_machine::snapshot::Record::decode(r)?,
            base: seedbox_machine::snapshot::Record::decode(r)?,
            icw: seedbox_machine::snapshot::Record::decode(r)?,
            read_isr: seedbox_machine::snapshot::Record::decode(r)?,
            slave_imr: seedbox_machine::snapshot::Record::decode(r)?,
            slave_icw: seedbox_machine::snapshot::Record::decode(r)?,
        })
    }
}

// Closed persisted record: exhaustive destructuring prevents silently omitting new fields.
impl seedbox_machine::snapshot::Record for Uart {
    const MIN: u64 = <u8 as seedbox_machine::snapshot::Record>::MIN
        + <u8 as seedbox_machine::snapshot::Record>::MIN
        + <u8 as seedbox_machine::snapshot::Record>::MIN
        + <u8 as seedbox_machine::snapshot::Record>::MIN
        + <u8 as seedbox_machine::snapshot::Record>::MIN
        + <u8 as seedbox_machine::snapshot::Record>::MIN
        + <bool as seedbox_machine::snapshot::Record>::MIN;
    fn encode<W: std::io::Write>(
        &self,
        w: &mut seedbox_machine::snapshot::Writer<'_, W>,
    ) -> seedbox_machine::snapshot::Result<()> {
        let Self {
            ier,
            lcr,
            mcr,
            scr,
            dll,
            dlm,
            thre,
        } = self;
        ier.encode(w)?;
        lcr.encode(w)?;
        mcr.encode(w)?;
        scr.encode(w)?;
        dll.encode(w)?;
        dlm.encode(w)?;
        thre.encode(w)?;
        Ok(())
    }
    fn decode<R: std::io::Read>(
        r: &mut seedbox_machine::snapshot::Reader<'_, R>,
    ) -> seedbox_machine::snapshot::Result<Self> {
        Ok(Self {
            ier: seedbox_machine::snapshot::Record::decode(r)?,
            lcr: seedbox_machine::snapshot::Record::decode(r)?,
            mcr: seedbox_machine::snapshot::Record::decode(r)?,
            scr: seedbox_machine::snapshot::Record::decode(r)?,
            dll: seedbox_machine::snapshot::Record::decode(r)?,
            dlm: seedbox_machine::snapshot::Record::decode(r)?,
            thre: seedbox_machine::snapshot::Record::decode(r)?,
        })
    }
}

// Closed persisted record: exhaustive destructuring prevents silently omitting new fields.
impl seedbox_machine::snapshot::Record for Devices {
    const MIN: u64 = <Pic as seedbox_machine::snapshot::Record>::MIN
        + <Uart as seedbox_machine::snapshot::Record>::MIN
        + <u8 as seedbox_machine::snapshot::Record>::MIN;
    fn encode<W: std::io::Write>(
        &self,
        w: &mut seedbox_machine::snapshot::Writer<'_, W>,
    ) -> seedbox_machine::snapshot::Result<()> {
        let Self { pic, uart, lines } = self;
        pic.encode(w)?;
        uart.encode(w)?;
        lines.encode(w)?;
        Ok(())
    }
    fn decode<R: std::io::Read>(
        r: &mut seedbox_machine::snapshot::Reader<'_, R>,
    ) -> seedbox_machine::snapshot::Result<Self> {
        Ok(Self {
            pic: seedbox_machine::snapshot::Record::decode(r)?,
            uart: seedbox_machine::snapshot::Record::decode(r)?,
            lines: seedbox_machine::snapshot::Record::decode(r)?,
        })
    }
}

impl Devices {
    pub(super) fn validate_snapshot(&self) -> Result<()> {
        if self.pic.icw > 3 || self.pic.slave_icw > 3 || self.pic.base & 7 != 0 {
            return Err("PV snapshot PIC structure".into());
        }
        Ok(())
    }
}
