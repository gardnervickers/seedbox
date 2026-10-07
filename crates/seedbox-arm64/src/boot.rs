//! Checked boot placement for the current single-vCPU arm64 guest profile.
//!
//! Validation establishes address and copy safety, not guest boot readiness or
//! support for Compose applications. RAM has a fixed 2 MiB-aligned base and must
//! meet the backends' 16 KiB host mapping alignment. The current guest uses 4 KiB
//! pages. Neither backend selects an IPA width, and CPU policy preserves
//! ID_AA64MMFR0_EL1.PARange; staying below 4 GiB uses its minimum defined 32-bit
//! range. Larger layouts need an explicit backend/CPU address-width contract.
use crate::{machine::image_layout, Result, DTB_ADDR, INITRD_ADDR, PAGE_SIZE, RAM_BASE, RAM_SIZE};
use std::ops::Range;

/// Fixed reservation; the generated FDT must also fit this bound before mmap.
/// This is smaller than Linux arm64's 2 MiB maximum DTB size.
pub const DTB_RESERVATION: u64 = 64 << 10;
pub const GPA_LIMIT: u64 = 1 << 32;
pub const MAX_RAM_BYTES: u64 = GPA_LIMIT - RAM_BASE;
/// Linux arm64 COMMAND_LINE_SIZE includes the terminating NUL.
pub const COMMAND_LINE_SIZE: usize = 2048;
pub const DEFAULT_COMMAND_LINE: &str = "console=ttyAMA0 earlycon=pl011,0x09000000 rdinit=/init nokaslr nosmp maxcpus=1 random.trust_cpu=off random.trust_bootloader=on mitigations=off panic=-1 loglevel=4";

/// Borrowed inputs and explicit placements. Call `validate` before using them.
#[derive(Clone, Copy, Debug)]
pub struct BootSpec<'a> {
    pub kernel: &'a [u8],
    pub initrd: &'a [u8],
    pub seed: u64,
    pub memory_bytes: u64,
    pub initrd_addr: u64,
    pub dtb_addr: u64,
    pub command_line: &'a str,
}
impl<'a> BootSpec<'a> {
    /// Historical fixture placement, preserving its addresses and command line.
    pub fn fixture(kernel: &'a [u8], initrd: &'a [u8], seed: u64) -> Self {
        Self {
            kernel,
            initrd,
            seed,
            memory_bytes: RAM_SIZE as u64,
            initrd_addr: INITRD_ADDR,
            dtb_addr: DTB_ADDR,
            command_line: DEFAULT_COMMAND_LINE,
        }
    }

    /// Place an application archive after the complete reserved kernel Image,
    /// and reserve the final 64 KiB of RAM for the device tree. No guest address
    /// arithmetic is required of the caller. Insufficient RAM is an error;
    /// this does not estimate the application's runtime memory needs.
    pub fn for_application(
        kernel: &'a [u8],
        initrd: &'a [u8],
        seed: u64,
        memory_bytes: u64,
        command_line: &'a str,
    ) -> Result<Self> {
        let ram = ram_range(memory_bytes)?;
        let (entry, size) = image_layout(kernel)?;
        let kernel_end = entry.checked_add(size).ok_or("kernel address overflow")?;
        let initrd_addr = align_up(kernel_end)?;
        let dtb_addr = ram
            .end
            .checked_sub(DTB_RESERVATION)
            .filter(|addr| *addr >= ram.start)
            .ok_or("RAM is too small for the device tree reservation")?;
        let spec = Self {
            kernel,
            initrd,
            seed,
            memory_bytes,
            initrd_addr,
            dtb_addr,
            command_line,
        };
        spec.validate()?;
        Ok(spec)
    }

    /// Validate all ranges before memory allocation or VM creation. The kernel
    /// reservation includes Image.image_size, which can exceed file length.
    pub fn validate(&self) -> Result<BootLayout<'a>> {
        let ram = ram_range(self.memory_bytes)?;
        if self.command_line.as_bytes().contains(&0) {
            return Err("guest command line contains NUL".into());
        }
        if self.command_line.len() >= COMMAND_LINE_SIZE {
            return Err("guest command line exceeds Linux arm64's 2047-byte limit".into());
        }
        let (entry, size) = image_layout(self.kernel)?;
        if !entry.is_multiple_of(4) {
            return Err("kernel entry is not instruction aligned".into());
        }
        let kernel = checked_range("kernel", entry, size, &ram)?;
        if self.initrd.is_empty() {
            return Err("initrd is empty".into());
        }
        for (name, addr) in [("initrd", self.initrd_addr), ("device tree", self.dtb_addr)] {
            if !addr.is_multiple_of(PAGE_SIZE as u64) {
                return Err(format!("{name} address must be aligned to 16 KiB"));
            }
        }
        let initrd = checked_range("initrd", self.initrd_addr, self.initrd.len() as u64, &ram)?;
        let dtb = checked_range("device tree", self.dtb_addr, DTB_RESERVATION, &ram)?;
        for (left, right, message) in [
            (&kernel, &initrd, "kernel overlaps initrd"),
            (&kernel, &dtb, "kernel overlaps device tree reservation"),
            (&initrd, &dtb, "initrd overlaps device tree reservation"),
        ] {
            if left.start < right.end && right.start < left.end {
                return Err(message.into());
            }
        }
        // All ranges are within [1 GiB, 4 GiB), thus also satisfy Linux's
        // common 1 GiB-aligned, at most 32 GiB Image/initrd placement window.
        Ok(BootLayout {
            ram,
            kernel,
            initrd,
            dtb,
            seed: self.seed,
            command_line: self.command_line,
        })
    }
}

/// Immutable validated guest physical ranges; no field can bypass validation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BootLayout<'a> {
    ram: Range<u64>,
    kernel: Range<u64>,
    initrd: Range<u64>,
    dtb: Range<u64>,
    seed: u64,
    command_line: &'a str,
}
impl<'a> BootLayout<'a> {
    pub fn ram_range(&self) -> Range<u64> {
        self.ram.clone()
    }
    pub fn memory_bytes(&self) -> usize {
        // ram_range validated the host usize conversion before constructing us.
        (self.ram.end - self.ram.start) as usize
    }
    pub fn kernel_range(&self) -> Range<u64> {
        self.kernel.clone()
    }
    pub fn initrd_range(&self) -> Range<u64> {
        self.initrd.clone()
    }
    /// The complete reserved span, not the generated FDT's serialized length.
    pub fn dtb_range(&self) -> Range<u64> {
        self.dtb.clone()
    }
    pub fn seed(&self) -> u64 {
        self.seed
    }
    pub fn command_line(&self) -> &'a str {
        self.command_line
    }
    /// Translate a complete GPA span into its RAM offset. MMIO and the separate
    /// PV time page are outside RAM and must never be translated this way.
    pub fn ram_offset(&self, gpa: u64, len: usize) -> Result<usize> {
        checked_range("guest RAM access", gpa, len as u64, &self.ram)?;
        usize::try_from(gpa - self.ram.start).map_err(|_| "RAM offset exceeds host usize".into())
    }
}

fn ram_range(memory_bytes: u64) -> Result<Range<u64>> {
    if memory_bytes == 0 || !memory_bytes.is_multiple_of(PAGE_SIZE as u64) {
        return Err("memory size must be a positive multiple of 16 KiB".into());
    }
    let end = RAM_BASE
        .checked_add(memory_bytes)
        .ok_or("RAM address overflow")?;
    if end > GPA_LIMIT {
        return Err("RAM exceeds the conservative 32-bit guest physical address range".into());
    }
    usize::try_from(memory_bytes).map_err(|_| "RAM size exceeds host usize")?;
    Ok(RAM_BASE..end)
}
fn checked_range(name: &str, addr: u64, len: u64, ram: &Range<u64>) -> Result<Range<u64>> {
    let end = addr
        .checked_add(len)
        .ok_or_else(|| format!("{name} address overflow"))?;
    if addr < ram.start || end > ram.end {
        return Err(format!("{name} is outside guest RAM"));
    }
    Ok(addr..end)
}
fn align_up(addr: u64) -> Result<u64> {
    addr.checked_add(PAGE_SIZE as u64 - 1)
        .map(|addr| addr & !(PAGE_SIZE as u64 - 1))
        .ok_or_else(|| "boot placement alignment overflow".into())
}
