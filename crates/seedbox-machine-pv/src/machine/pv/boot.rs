//! Checked native boot placement; the 128 MiB constant is only a fixture default.
mod layout;
pub(crate) use layout::RamLayout;
pub(super) const LAYOUT_VERSION: u32 = layout::VERSION;
#[cfg(any(target_arch = "aarch64", test))]
pub(super) mod tree;
#[cfg(test)]
pub(super) const RAM_BYTES: usize = 128 << 20;
#[cfg(target_arch = "aarch64")]
pub(super) const RAM_BASE: u64 = 0x4000_0000;
#[cfg(target_arch = "x86_64")]
pub(super) const RAM_BASE: u64 = 0;
#[cfg(target_arch = "x86_64")]
pub(super) const KERNEL: u64 = 0x10_0000;

// Boot callers need asset/resource context rather than a bare DMA diagnostic.
fn placement_error(ram: &RamLayout, part: &str, error: impl std::fmt::Display) -> String {
    format!("PV {part} placement failed for configured RAM ({} bytes): {error}; adjust boot inputs or configured memory", ram.capacity)
}

#[cfg(target_arch = "aarch64")]
pub(super) fn layout_for(
    ram: &RamLayout,
    kernel: &[u8],
    initrd: &[u8],
) -> Result<(u64, u64), String> {
    arm_layout_for(ram, kernel, initrd)
}

#[cfg(any(target_arch = "aarch64", test))]
pub(super) fn arm_layout_for(
    ram: &RamLayout,
    kernel: &[u8],
    initrd: &[u8],
) -> Result<(u64, u64), String> {
    if kernel.len() < 64 || &kernel[56..60] != b"ARM\x64" {
        return Err("PV requires an arm64 Linux Image header".into());
    }
    let word = |offset| u64::from_le_bytes(kernel[offset..offset + 8].try_into().unwrap());
    let entry = ram
        .base
        .checked_add(word(8))
        .ok_or_else(|| placement_error(ram, "Image", "entry overflow"))?;
    let reserved = word(16).max(kernel.len() as u64);
    if word(16) == 0 || word(24) & 1 != 0 || entry % 4 != 0 {
        return Err("PV unsupported Image layout or endianness".into());
    }
    let end = entry
        .checked_add(reserved)
        .ok_or_else(|| placement_error(ram, "Image", "range overflow"))?;
    ram.offset(
        entry,
        usize::try_from(reserved).map_err(|e| placement_error(ram, "Image", e))?,
    )
    .map_err(|e| placement_error(ram, "Image", e))?;
    let address = end
        .checked_add(16383)
        .ok_or_else(|| placement_error(ram, "initrd", "alignment overflow"))?
        & !16383;
    if initrd.is_empty()
        || address
            .checked_add(initrd.len() as u64)
            .is_none_or(|end| end > ram.dtb().unwrap_or(0))
    {
        return Err(placement_error(
            ram,
            "initrd",
            "empty or exceeds space before DTB",
        ));
    }
    ram.offset(address, initrd.len())
        .map_err(|e| placement_error(ram, "initrd", e))?;
    Ok((entry, address))
}

#[cfg(target_arch = "x86_64")]
pub(super) fn layout_for(
    ram: &RamLayout,
    kernel: &[u8],
    initrd: &[u8],
) -> Result<(u64, u64), String> {
    if kernel.len() < 0x268 || &kernel[0x202..0x206] != b"HdrS" {
        return Err("PV requires an x86 bzImage boot header".into());
    }
    let half = |o: usize| u16::from_le_bytes([kernel[o], kernel[o + 1]]);
    let word = |o: usize| u32::from_le_bytes(kernel[o..o + 4].try_into().unwrap());
    let long = |o: usize| u64::from_le_bytes(kernel[o..o + 8].try_into().unwrap());
    if half(0x206) < 0x020c || half(0x236) & 1 == 0 || kernel[0x211] & 1 == 0 {
        return Err("PV requires a 64-bit bzImage loaded high (boot protocol 2.12+)".into());
    }
    let sectors = match kernel[0x1f1] {
        0 => 4,
        n => n as usize,
    };
    let setup = (sectors + 1) * 512;
    if setup >= kernel.len() {
        return Err("PV bzImage has no protected-mode payload".into());
    }
    let payload = kernel.len() - setup;
    let preferred = long(0x258).max(KERNEL);
    let reserved = word(0x260) as u64;
    let end = KERNEL
        .checked_add(payload as u64)
        .ok_or_else(|| placement_error(ram, "kernel payload", "range overflow"))?
        .max(preferred.checked_add(reserved).ok_or_else(|| {
            placement_error(ram, "kernel decompression reservation", "range overflow")
        })?);
    ram.offset(KERNEL, payload)
        .map_err(|e| placement_error(ram, "kernel payload", e))?;
    if reserved != 0 {
        ram.offset(preferred, reserved as usize)
            .map_err(|e| placement_error(ram, "kernel decompression reservation", e))?;
    }
    if end > 1 << 32 {
        return Err(placement_error(
            ram,
            "kernel payload/decompression",
            "must fit the 32-bit identity map",
        ));
    }
    let ceiling = (u64::from(word(0x22c)) + 1).min(1 << 32);
    let address = ram
        .place(initrd.len() as u64, ceiling, end, 4096)
        .map_err(|e| placement_error(ram, "initrd", e))?;
    // Both fields are currently the 32-bit Linux boot fields: never truncate.
    u32::try_from(address).map_err(|e| placement_error(ram, "initrd address", e))?;
    u32::try_from(initrd.len()).map_err(|e| placement_error(ram, "initrd length", e))?;
    Ok((setup as u64, address))
}

pub(super) fn arguments(command_line: &str) -> Result<(), String> {
    if command_line.contains('\0') {
        return Err("PV kernel arguments contain NUL".into());
    }
    if command_line.len() >= 2048 {
        return Err("PV kernel arguments exceed 2047 bytes".into());
    }
    Ok(())
}
