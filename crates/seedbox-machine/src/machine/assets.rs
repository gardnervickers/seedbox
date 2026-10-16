//! Explicit, offline-built guest inputs. Loading never builds or searches.
// TODO: Re-evaluate: separate backends consume this canonical helper.
// Crate-private access cannot cross packages; representation remains private.
use std::{fs, io, path::Path};
/// Read and own all bytes at root.join(name), without search or building.
/// Missing/unreadable files retain their IO error kind with path context; an
/// empty file is rejected. The path is supplied by the caller, not sandboxed.
pub fn read(root: &Path, name: &str) -> io::Result<Vec<u8>> {
    let path = root.join(name);
    let bytes = fs::read(&path)
        .map_err(|error| io::Error::new(error.kind(), format!("{}: {error}", path.display())))?;
    if bytes.is_empty() {
        return Err(io::Error::other(format!(
            "{}: empty guest asset",
            path.display()
        )));
    }
    Ok(bytes)
}
/// Require that the supplied path resolves to a file, following symlinks.
/// Returns an error naming the path otherwise. Does not execute the file or
/// check executable permissions, architecture, contents or protocol support.
pub fn executable(path: &Path) -> io::Result<()> {
    if !path.is_file() {
        return Err(io::Error::other(format!(
            "{}: executable file not found",
            path.display()
        )));
    }
    Ok(())
}
/// Read root/command-line as owned UTF-8, rejecting NUL or a byte length >= limit.
/// IO failures include the path; contents are returned verbatim without trimming,
/// search or execution. The caller must supply a positive limit.
pub fn arguments(root: &Path, limit: usize) -> io::Result<String> {
    let path = root.join("command-line");
    let bytes = fs::read(&path)
        .map_err(|error| io::Error::new(error.kind(), format!("{}: {error}", path.display())))?;
    let text = String::from_utf8(bytes)
        .map_err(|_| io::Error::other("guest command-line is not UTF-8"))?;
    if text.contains('\0') || text.len() >= limit {
        return Err(io::Error::other(format!(
            "guest command-line must contain no NUL and fit in {} bytes",
            limit - 1
        )));
    }
    Ok(text)
}
/// Validate a supplied little-endian 64-bit ELF for the requested machine ID.
/// Checks supported headers and bounded load segments, requiring loadable memory;
/// static_program additionally rejects PT_INTERP. Returns an error for malformed
/// or incompatible bytes without modifying them; does not load or execute ELF.
pub fn elf(bytes: &[u8], machine: u16, static_program: bool) -> io::Result<()> {
    let invalid = || io::Error::other("invalid or incompatible guest ELF asset");
    if bytes.len() < 64
        || &bytes[..4] != b"\x7fELF"
        || bytes[4] != 2
        || bytes[5] != 1
        || bytes[6] != 1
        || !matches!(u16::from_le_bytes(bytes[16..18].try_into().unwrap()), 2 | 3)
        || u32::from_le_bytes(bytes[20..24].try_into().unwrap()) != 1
        || u16::from_le_bytes(bytes[52..54].try_into().unwrap()) != 64
        || u16::from_le_bytes(bytes[18..20].try_into().unwrap()) != machine
    {
        return Err(invalid());
    }
    let offset = usize::try_from(u64::from_le_bytes(bytes[32..40].try_into().unwrap()))
        .map_err(|_| invalid())?;
    let size = usize::from(u16::from_le_bytes(bytes[54..56].try_into().unwrap()));
    let count = usize::from(u16::from_le_bytes(bytes[56..58].try_into().unwrap()));
    if count > 0 && size < 56 {
        return Err(invalid());
    }
    let mut loadable = false;
    for index in 0..count {
        let at = index
            .checked_mul(size)
            .and_then(|delta| offset.checked_add(delta))
            .ok_or_else(invalid)?;
        let header = bytes
            .get(at..at.checked_add(size).ok_or_else(invalid)?)
            .ok_or_else(invalid)?;
        let kind = u32::from_le_bytes(header[..4].try_into().unwrap());
        if kind == 1 {
            let file_offset = u64::from_le_bytes(header[8..16].try_into().unwrap());
            let file_size = u64::from_le_bytes(header[32..40].try_into().unwrap());
            let memory_size = u64::from_le_bytes(header[40..48].try_into().unwrap());
            if file_size > memory_size
                || file_offset
                    .checked_add(file_size)
                    .is_none_or(|end| end > bytes.len() as u64)
            {
                return Err(invalid());
            }
            loadable |= memory_size > 0;
        }
        if static_program && kind == 3 {
            return Err(io::Error::other(
                "guest init requires a static ELF executable",
            ));
        }
    }
    if !loadable {
        return Err(invalid());
    }
    Ok(())
}
/// Check an arm64 Linux Image header, for the arm64 PV factory; TCG checks
/// an x86-64 bzImage with [`bzimage`].
#[cfg(target_arch = "aarch64")]
pub fn kernel(bytes: &[u8]) -> io::Result<()> {
    if bytes.get(56..60) != Some(b"ARM\x64".as_slice()) {
        return Err(io::Error::other(
            "invalid or incompatible guest Linux Image",
        ));
    }
    Ok(())
}

/// Check an x86-64 Linux bzImage, independent of the host architecture: the
/// `HdrS` header, boot protocol 2.12 or later, a kernel loaded high with a
/// 64-bit entry (`XLF_KERNEL_64`), and a protected-mode payload after setup.
pub fn bzimage(bytes: &[u8]) -> io::Result<()> {
    let invalid = || io::Error::other("invalid or incompatible guest x86-64 bzImage");
    if bytes.len() < 0x268 || &bytes[0x202..0x206] != b"HdrS" {
        return Err(invalid());
    }
    let protocol = u16::from_le_bytes([bytes[0x206], bytes[0x207]]);
    let xloadflags = u16::from_le_bytes([bytes[0x236], bytes[0x237]]);
    // A zero setup sector count means the historical four.
    let sectors = match bytes[0x1f1] {
        0 => 4,
        sectors => usize::from(sectors),
    };
    let setup = (sectors + 1) * 512;
    if protocol < 0x020c || bytes[0x211] & 1 == 0 || xloadflags & 1 == 0 || setup >= bytes.len() {
        return Err(invalid());
    }
    Ok(())
}

/// Check an x86 BIOS image: QEMU maps a nonzero whole number of 64 KiB
/// blocks ending at 4 GiB, with its reset vector 16 bytes from the end.
pub fn bios(bytes: &[u8]) -> io::Result<()> {
    if bytes.is_empty() || !bytes.len().is_multiple_of(64 * 1024) {
        return Err(io::Error::other(
            "guest firmware must be an x86 BIOS image of whole 64 KiB blocks",
        ));
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests;
