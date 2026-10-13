//! Private fixed 128 MiB Linux Image layout, checked before launching a worker.
pub(super) const RAM_BASE: u64 = 0x4000_0000;
pub(super) const RAM_BYTES: usize = 128 << 20;
pub(super) const DTB: u64 = RAM_BASE + RAM_BYTES as u64 - (64 << 10);
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

// Linux arm64 UAPI COMMAND_LINE_SIZE includes the terminating NUL.
pub(super) fn arguments(command_line: &str) -> Result<(), String> {
    if command_line.contains('\0') {
        return Err("PV kernel arguments contain NUL".into());
    }
    if command_line.len() >= 2048 {
        return Err("PV kernel arguments exceed 2047 bytes".into());
    }
    Ok(())
}
