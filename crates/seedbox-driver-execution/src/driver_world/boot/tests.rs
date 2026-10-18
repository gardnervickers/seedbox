//! Pure archive fixtures: no VM execution and no claim of native state evidence.
use super::*;
use std::io::Read;
fn entry(out: &mut Vec<u8>, name: &str, mode: u32, inode: u32, links: u32, data: &[u8]) {
    out.extend_from_slice(b"070701");
    for value in [
        inode,
        mode,
        12,
        34,
        links,
        56,
        data.len() as u32,
        0,
        0,
        0,
        0,
        name.len() as u32 + 1,
        0,
    ] {
        out.extend_from_slice(format!("{value:08x}").as_bytes());
    }
    out.extend_from_slice(name.as_bytes());
    out.push(0);
    while !out.len().is_multiple_of(4) {
        out.push(0);
    }
    out.extend_from_slice(data);
    while !out.len().is_multiple_of(4) {
        out.push(0);
    }
}
fn trailer(out: &mut Vec<u8>) {
    entry(out, "TRAILER!!!", 0, 0, 1, &[]);
}
#[test]
fn complete_boot_overlay_is_imported_without_host_extraction_or_stale_payload() {
    let mut bytes = vec![];
    entry(&mut bytes, "app/value", 0o100751, 1, 1, b"stale");
    trailer(&mut bytes);
    entry(&mut bytes, "app/value", 0o100640, 2, 1, b"replacement");
    entry(&mut bytes, "app/link", 0o120777, 3, 1, b"value");
    trailer(&mut bytes);
    let before = bytes.clone();
    let view = filesystem(&bytes).unwrap();
    assert_eq!(bytes, before);
    let mut archive = tar::Archive::new(view.as_slice());
    let mut found = BTreeMap::new();
    for entry in archive.entries().unwrap() {
        let mut entry = entry.unwrap();
        let path = entry.path().unwrap().into_owned();
        let mut data = vec![];
        entry.read_to_end(&mut data).unwrap();
        found.insert(
            path,
            (
                entry.header().mode().unwrap(),
                entry.header().uid().unwrap(),
                entry.header().gid().unwrap(),
                data,
                entry.link_name().unwrap().map(|p| p.into_owned()),
            ),
        );
    }
    let value = &found[Path::new("app/value")];
    assert_eq!(value.0, 0o640);
    assert_eq!((value.1, value.2), (12, 34));
    assert_eq!(value.3, b"replacement");
    assert_eq!(
        found[Path::new("app/link")].4.as_deref(),
        Some(Path::new("value"))
    );
}
#[test]
fn forward_newc_hardlink_payload_and_concatenated_inode_namespaces_are_preserved() {
    let mut bytes = vec![];
    entry(&mut bytes, "a", 0o100644, 7, 2, b"");
    entry(&mut bytes, "b", 0o100644, 7, 2, b"linked");
    trailer(&mut bytes);
    entry(&mut bytes, "c", 0o100644, 7, 1, b"separate archive");
    trailer(&mut bytes);
    let view = filesystem(&bytes).unwrap();
    let mut archive = tar::Archive::new(view.as_slice());
    let mut entries = archive.entries().unwrap();
    let mut first = entries.next().unwrap().unwrap();
    let mut data = vec![];
    first.read_to_end(&mut data).unwrap();
    assert_eq!(data, b"linked");
    let second = entries.next().unwrap().unwrap();
    assert!(second.header().entry_type().is_hard_link());
    assert_eq!(second.link_name().unwrap().unwrap(), Path::new("a"));
    let mut third = entries.next().unwrap().unwrap();
    let mut data = vec![];
    third.read_to_end(&mut data).unwrap();
    assert_eq!(data, b"separate archive");
}
#[test]
fn import_refuses_truncation_and_retains_a_verified_attempt_owned_view() {
    let mut bytes = vec![];
    entry(&mut bytes, "payload", 0o100644, 1, 1, b"new bytes");
    trailer(&mut bytes);
    assert!(filesystem(&bytes[..109]).is_err());
    let root = tempfile::tempdir().unwrap();
    let artifact = retain_view(&bytes, root.path()).unwrap();
    let mut file = super::super::checkpoint::verified(&artifact).unwrap();
    let mut data = vec![];
    file.read_to_end(&mut data).unwrap();
    assert_eq!(data, filesystem(&bytes).unwrap());
    let other = retain_view(&bytes, root.path()).unwrap();
    assert_ne!(artifact.artifact, other.artifact);
    assert_eq!(artifact.sha256, other.sha256);
}
#[test]
fn boot_ram_floor_uses_actual_complete_bytes_and_checked_arithmetic() {
    assert_eq!(minimum_memory(7).unwrap(), 128 * 1024 * 1024 + 21);
    assert!(minimum_memory(u64::MAX).is_err());
}
