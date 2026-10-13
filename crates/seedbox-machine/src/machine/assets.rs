//! Explicit, offline-built guest inputs. Loading never builds or searches.
use std::{fs, io, path::Path};
pub(super) fn read(root: &Path, name: &str) -> io::Result<Vec<u8>> {
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
pub(super) fn executable(path: &Path) -> io::Result<()> {
    if !path.is_file() {
        return Err(io::Error::other(format!(
            "{}: executable file not found",
            path.display()
        )));
    }
    Ok(())
}
pub(super) fn arguments(root: &Path, limit: usize) -> io::Result<String> {
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
pub(super) fn elf(bytes: &[u8], machine: u16, static_program: bool) -> io::Result<()> {
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
pub(super) fn kernel(bytes: &[u8], arm: bool) -> io::Result<()> {
    let valid = if arm {
        bytes.get(56..60) == Some(b"ARM\x64".as_slice())
    } else {
        bytes.get(48..56) == Some(b"RISCV\0\0\0".as_slice())
            && bytes.get(56..60) == Some(b"RSC\x05".as_slice())
    };
    if !valid {
        return Err(io::Error::other(
            "invalid or incompatible guest Linux Image",
        ));
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    pub(crate) fn image(machine: u16) -> Vec<u8> {
        let mut bytes = vec![0; 120];
        bytes[..7].copy_from_slice(b"\x7fELF\x02\x01\x01");
        bytes[16..18].copy_from_slice(&2_u16.to_le_bytes());
        bytes[18..20].copy_from_slice(&machine.to_le_bytes());
        bytes[20..24].copy_from_slice(&1_u32.to_le_bytes());
        bytes[32..40].copy_from_slice(&64_u64.to_le_bytes());
        bytes[52..54].copy_from_slice(&64_u16.to_le_bytes());
        bytes[54..56].copy_from_slice(&56_u16.to_le_bytes());
        bytes[56..58].copy_from_slice(&1_u16.to_le_bytes());
        bytes[64..68].copy_from_slice(&1_u32.to_le_bytes());
        bytes[96..104].copy_from_slice(&120_u64.to_le_bytes());
        bytes[104..112].copy_from_slice(&120_u64.to_le_bytes());
        bytes
    }
    #[test]
    fn command_line_is_verbatim_including_empty_and_rejects_nul_or_byte_overflow() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("command-line");
        for text in ["", "  console=x\n", "a=b a=c"] {
            fs::write(&path, text).unwrap();
            assert_eq!(arguments(directory.path(), 1024).unwrap(), text);
        }
        for text in ["\0".to_owned(), "é".repeat(512)] {
            fs::write(&path, text).unwrap();
            assert!(arguments(directory.path(), 1024).is_err());
        }
    }
    #[test]
    fn elf_requires_correct_architecture_and_bounded_loadable_segments() {
        let valid = image(183);
        elf(&valid, 183, true).unwrap();
        assert!(elf(&valid, 243, true).is_err());
        let mut invalid = valid.clone();
        invalid[56..58].copy_from_slice(&0_u16.to_le_bytes());
        assert!(elf(&invalid, 183, true).is_err());
        let mut invalid = valid.clone();
        invalid[96..104].copy_from_slice(&121_u64.to_le_bytes());
        assert!(elf(&invalid, 183, true).is_err());
        let mut invalid = valid.clone();
        invalid[104..112].copy_from_slice(&119_u64.to_le_bytes());
        assert!(elf(&invalid, 183, true).is_err());
        let mut invalid = valid.clone();
        invalid[64..68].copy_from_slice(&3_u32.to_le_bytes());
        assert!(elf(&invalid, 183, true).is_err());
        let mut invalid = valid.clone();
        invalid[16..18].copy_from_slice(&1_u16.to_le_bytes());
        assert!(elf(&invalid, 183, false).is_err());
    }
}
