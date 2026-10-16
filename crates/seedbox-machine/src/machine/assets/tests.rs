use super::*;
use crate::test_support::assets::{bzimage_header, image};
#[test]
fn bzimage_requires_a_64_bit_high_kernel_with_a_payload() {
    bzimage(&bzimage_header()).unwrap();
    let mut zero_sectors = bzimage_header();
    zero_sectors[0x1f1] = 0;
    bzimage(&zero_sectors).unwrap();
    let edits: [fn(&mut Vec<u8>); 6] = [
        |bytes| bytes[0x202] = b'h',
        |bytes| bytes[0x206..0x208].copy_from_slice(&0x020b_u16.to_le_bytes()),
        |bytes| bytes[0x211] = 0,
        |bytes| bytes[0x236] = 0,
        |bytes| bytes.truncate(5 * 512),
        |bytes| bytes.truncate(0x267),
    ];
    for edit in edits {
        let mut bytes = bzimage_header();
        edit(&mut bytes);
        assert!(bzimage(&bytes).is_err());
    }
    // Neither an arm64 Image nor an ELF is a bzImage.
    let mut arm = vec![0; 0x1000];
    arm[56..60].copy_from_slice(b"ARM\x64");
    assert!(bzimage(&arm).is_err());
    assert!(bzimage(&image(62)).is_err());
}
#[test]
fn bios_is_whole_64_kib_blocks() {
    bios(&[0; 64 * 1024]).unwrap();
    bios(&[0; 128 * 1024]).unwrap();
    for length in [0, 1, 64 * 1024 - 1, 64 * 1024 + 1] {
        assert!(bios(&vec![0; length]).is_err());
    }
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
