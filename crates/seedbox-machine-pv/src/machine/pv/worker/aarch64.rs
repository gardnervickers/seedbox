//! ARM64 platform of the stopped machine: Image/FDT boot, GICv3 SPIs, PL011,
//! PSCI and the software exclusive-reservation epoch. HVF on macOS, KVM on Linux.
//!
//! After the ready handshake, PSCI SYSTEM_OFF ends the boot as power-off and
//! SYSTEM_RESET (a deliberate reboot or a `panic=-1` restart) as a reset; the
//! crate's kernel routes Linux halt to SYSTEM_OFF instead of an endless masked
//! spin. Before the handshake both are setup errors.
mod backend;
use super::boot::tree;
use super::{
    boot,
    memory::{self, Memory},
    protocol::Stop,
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
/// The optional block device: SPI 11, described in the device tree.
const DISK: u32 = 43;
/// The optional NIC: SPI 12, described in the device tree.
const NET: u32 = 44;
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
pub(super) struct Entry(u64, u64);
/// Place the Image, initrd and generated device tree in guest RAM.
pub(super) fn load(
    ram: &mut Memory,
    kernel: &[u8],
    initrd: &[u8],
    seed: u64,
    command_line: &str,
    disk: bool,
    network: bool,
) -> Result<Entry> {
    let layout = ram.layout()?.clone();
    let (entry, initrd_address) = boot::layout_for(&layout, kernel, initrd)?;
    let dtb = layout.dtb()?;
    ram.write(layout.offset(entry, kernel.len())?, kernel)?;
    ram.write(layout.offset(initrd_address, initrd.len())?, initrd)?;
    ram.write(
        layout.offset(dtb, 65536)?,
        &tree::build_for(
            &layout,
            initrd_address,
            initrd.len(),
            seed,
            command_line,
            disk,
            network,
        )?,
    )?;
    Ok(Entry(entry, dtb))
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
    native.set_core(0, entry.1)?;
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
            let was_asserted = self.state.devices.asserted.remove(&number);
            self.native.irq(number, false)?;
            if was_asserted {
                self.record_native(b'j', 0, number.into(), 0);
            }
            return Ok(());
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
        self.record_native(b'I', 0, number.into(), 0);
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
        if let Some(irq) = self.block.as_ref().map(|block| block.irq()) {
            self.irq(DISK, irq)?;
        }
        if let Some(irq) = self.state.net.as_ref().map(|net| net.irq()) {
            self.irq(NET, irq)?;
        }
        if self.state.devices.asserted.is_empty() && self.native.core(PSTATE)? & PSTATE_I == 0 {
            if let Some(number) = self.state.devices.deferred.pop_first() {
                self.native.irq(number, true)?;
                self.record_native(b'I', 0, number.into(), 0);
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
    pub(super) fn maintenance(&mut self, _exit: &Exit) -> Result<bool> {
        if self.native.system(CNTV_CTL)? & 1 != 0
            || self.state.ready && self.native.system(CNTKCTL)? != 0
        {
            return Err("PV guest enabled host counter/timer".into());
        }
        #[cfg(target_os = "macos")]
        if matches!(
            _exit,
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
            #[cfg(any(target_os = "macos", test))]
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
                    // SYSTEM_OFF; the patched kernel's halt also arrives here.
                    0x8400_0008 => {
                        effect.terminal = Some(Stop::PoweredOff);
                        0
                    }
                    // SYSTEM_RESET: a deliberate reboot or a panic restart.
                    // The worker never resets itself; the boot ends here.
                    0x8400_0009 => {
                        effect.terminal = Some(Stop::Reset);
                        0
                    }
                    _ => u64::MAX,
                };
                self.native.set_core(0, value)?;
                for register in 1..4 {
                    self.native.set_core(register, 0)?;
                }
            }
            #[cfg(target_os = "macos")]
            Exit::System { register, .. } => {
                return Err(format!("PV unsupported system register {register:#x}"));
            }
            Exit::Interrupted => unreachable!(),
        }
        Ok(effect)
    }
}

#[cfg(test)]
mod tests;

// Closed persisted record: exhaustive destructuring prevents silently omitting new fields.
impl crate::machine::pv::codec::record::Record for Devices {
    const MIN: u64 = <BTreeSet<u32> as crate::machine::pv::codec::record::Record>::MIN
        + <BTreeSet<u32> as crate::machine::pv::codec::record::Record>::MIN
        + <[u32; 16] as crate::machine::pv::codec::record::Record>::MIN;
    fn encode<W: std::io::Write>(
        &self,
        w: &mut seedbox_machine::snapshot::Writer<'_, W>,
    ) -> seedbox_machine::snapshot::Result<()> {
        let Self {
            deferred,
            asserted,
            uart,
        } = self;
        deferred.encode(w)?;
        asserted.encode(w)?;
        uart.encode(w)?;
        Ok(())
    }
    fn decode<R: std::io::Read>(
        r: &mut seedbox_machine::snapshot::Reader<'_, R>,
    ) -> seedbox_machine::snapshot::Result<Self> {
        Ok(Self {
            deferred: crate::machine::pv::codec::record::Record::decode(r)?,
            asserted: crate::machine::pv::codec::record::Record::decode(r)?,
            uart: crate::machine::pv::codec::record::Record::decode(r)?,
        })
    }
}

impl Devices {
    pub(super) fn validate_snapshot(&self) -> Result<()> {
        // Sets are strictly ordered during closed decoding; interrupt inventory
        // and native GIC acceptance remain actual native restore checks.
        Ok(())
    }
}
