//! Validate the actual private build utility's archive, without extracting it.
use std::{collections::BTreeMap, fs, path::Path, process::Command};

#[derive(Debug)]
struct Entry {
    mode: u32,
    device: (u32, u32),
    bytes: Vec<u8>,
}
fn entries(bytes: &[u8]) -> BTreeMap<String, Entry> {
    let mut result = BTreeMap::new();
    let mut at = 0;
    while at < bytes.len() {
        assert_eq!(&bytes[at..at + 6], b"070701");
        let number = |field: usize| {
            u32::from_str_radix(
                std::str::from_utf8(&bytes[at + 6 + field * 8..at + 14 + field * 8]).unwrap(),
                16,
            )
            .unwrap()
        };
        let size = number(6) as usize;
        let name_end = at + 110 + number(11) as usize;
        let name = std::str::from_utf8(&bytes[at + 110..name_end - 1]).unwrap();
        assert_eq!(bytes[name_end - 1], 0);
        assert_eq!((number(2), number(3), number(5)), (0, 0, 0));
        let data = (name_end + 3) & !3;
        result.insert(
            name.to_owned(),
            Entry {
                mode: number(1),
                device: (number(9), number(10)),
                bytes: bytes[data..data + size].to_vec(),
            },
        );
        at = (data + size + 3) & !3;
    }
    result
}
fn pack(directory: &Path, base: Option<&Path>, output: &Path) {
    let mut command = Command::new("python3");
    command
        .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("guest-initramfs.py"))
        .arg("--init")
        .arg(directory.join("init"))
        .arg("--application")
        .arg(directory.join("application"))
        .arg("--output")
        .arg(output);
    if let Some(base) = base {
        command.arg("--base").arg(base);
    }
    assert!(command.status().unwrap().success());
}
#[test]
fn private_guest_archive_preserves_binary_inputs_and_overlays_without_extraction() {
    let directory = tempfile::tempdir().unwrap();
    let directory = directory.path();
    fs::write(directory.join("init"), b"\x7fELF\0init").unwrap();
    fs::write(directory.join("application"), b"\x7fELF\0app").unwrap();
    let base = directory.join("base.cpio");
    pack(directory, None, &base);
    let bytes = fs::read(&base).unwrap();
    let parsed = entries(&bytes);
    assert_eq!(parsed["init"].bytes, b"\x7fELF\0init");
    assert_eq!(parsed["rootfs/application"].bytes, b"\x7fELF\0app");
    assert_eq!(parsed["init"].mode, 0o100755);
    assert_eq!(parsed["rootfs/application"].mode, 0o100755);
    assert_eq!(parsed["dev/console"].mode, 0o020600);
    assert_eq!(parsed["dev/console"].device, (5, 1));
    assert_eq!(parsed["tmp"].mode, 0o041777);
    assert_eq!(parsed.len(), 11);
    assert!(
        parsed["seedbox/application.bin"]
            .bytes
            .starts_with(b"SBXAPP2\0")
    );
    fs::write(directory.join("application"), b"replacement\0bytes").unwrap();
    let overlay = directory.join("overlay.cpio");
    pack(directory, Some(&base), &overlay);
    let overlaid = fs::read(overlay).unwrap();
    assert!(overlaid.starts_with(&bytes));
    assert_eq!(
        entries(&overlaid)["rootfs/application"].bytes,
        b"replacement\0bytes"
    );
    assert_eq!(entries(&overlaid)["init"].bytes, b"\x7fELF\0init");
    assert!(!directory.join("dev").exists());
}
