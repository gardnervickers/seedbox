//! Private fixed 128 MiB guest layout, checked before launching a worker.
//! ARM64 loads a Linux Image at 0x4000_0000; x86-64 loads a bzImage's
//! protected-mode payload at 1 MiB in RAM starting at physical address 0.
pub(super) const RAM_BYTES: usize = 128 << 20;
#[cfg(target_arch = "aarch64")]
pub(super) const RAM_BASE: u64 = 0x4000_0000;
#[cfg(target_arch = "aarch64")]
pub(super) const DTB: u64 = RAM_BASE + RAM_BYTES as u64 - (64 << 10);
#[cfg(target_arch = "aarch64")]
pub(super) fn layout(kernel: &[u8], initrd: &[u8]) -> Result<(u64, u64), String> {
    if kernel.len() < 64 || &kernel[56..60] != b"ARM\x64" {
        return Err("PV requires an arm64 Linux Image header".into());
    }
    let word = |offset| u64::from_le_bytes(kernel[offset..offset + 8].try_into().unwrap());
    let entry = RAM_BASE
        .checked_add(word(8))
        .ok_or("PV Image entry overflow")?;
    let reserved = word(16).max(kernel.len() as u64);
    if word(16) == 0 || word(24) & 1 != 0 || entry % 4 != 0 {
        return Err("PV unsupported Image layout or endianness".into());
    }
    let end = entry
        .checked_add(reserved)
        .ok_or("PV Image range overflow")?;
    let address = end
        .checked_add(16383)
        .ok_or("PV initrd alignment overflow")?
        & !16383;
    if initrd.is_empty()
        || address
            .checked_add(initrd.len() as u64)
            .is_none_or(|end| end > DTB)
    {
        return Err("PV initrd is empty or exceeds RAM layout".into());
    }
    Ok((entry, address))
}

#[cfg(target_arch = "x86_64")]
pub(super) const RAM_BASE: u64 = 0;
/// Load address of the bzImage protected-mode payload; the 64-bit boot
/// protocol enters 0x200 bytes after it.
#[cfg(target_arch = "x86_64")]
pub(super) const KERNEL: u64 = 0x10_0000;
/// Validate an x86 bzImage for the 64-bit boot protocol. Returns the file
/// offset of its protected-mode payload and the initrd address at the top of RAM.
#[cfg(target_arch = "x86_64")]
pub(super) fn layout(kernel: &[u8], initrd: &[u8]) -> Result<(usize, u64), String> {
    if kernel.len() < 0x268 || &kernel[0x202..0x206] != b"HdrS" {
        return Err("PV requires an x86 bzImage boot header".into());
    }
    let half = |offset: usize| u16::from_le_bytes([kernel[offset], kernel[offset + 1]]);
    let word = |offset: usize| u32::from_le_bytes(kernel[offset..offset + 4].try_into().unwrap());
    let long = |offset: usize| u64::from_le_bytes(kernel[offset..offset + 8].try_into().unwrap());
    // Boot protocol 2.12 added xloadflags; XLF_KERNEL_64 marks the 64-bit entry.
    if half(0x206) < 0x020c || half(0x236) & 1 == 0 || kernel[0x211] & 1 == 0 {
        return Err("PV requires a 64-bit bzImage loaded high (boot protocol 2.12+)".into());
    }
    let sectors = match kernel[0x1f1] {
        0 => 4,
        sectors => sectors as usize,
    };
    let setup = (sectors + 1) * 512;
    if setup >= kernel.len() {
        return Err("PV bzImage has no protected-mode payload".into());
    }
    // A relocatable kernel decompresses at its preferred address when loaded
    // lower, and needs init_size bytes from there.
    let payload = (kernel.len() - setup) as u64;
    let preferred = long(0x258).max(KERNEL);
    let end = (KERNEL + payload).max(
        preferred
            .checked_add(word(0x260) as u64)
            .ok_or("PV bzImage init size overflow")?,
    );
    let top = RAM_BASE + RAM_BYTES as u64;
    let maximum = (word(0x22c) as u64).min(top - 1);
    let address = top
        .checked_sub(initrd.len() as u64)
        .ok_or("PV initrd exceeds RAM")?
        & !0xfff;
    if initrd.is_empty() || address < end || address + initrd.len() as u64 - 1 > maximum {
        return Err("PV initrd is empty or exceeds RAM layout".into());
    }
    Ok((setup, address))
}

// Linux COMMAND_LINE_SIZE (arm64 UAPI, x86 boot default) includes the
// terminating NUL.
pub(super) fn arguments(command_line: &str) -> Result<(), String> {
    if command_line.contains('\0') {
        return Err("PV kernel arguments contain NUL".into());
    }
    if command_line.len() >= 2048 {
        return Err("PV kernel arguments exceed 2047 bytes".into());
    }
    Ok(())
}
