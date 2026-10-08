//! Valid synthetic parser fixture, shared by integration and CLI resolver tests.
#![allow(dead_code)]
use flate2::{write::GzEncoder, Compression};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, io::Write};
const PLATFORM: &str = "x86_64-linux";
pub fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
pub fn elf() -> Vec<u8> {
    let mut b = vec![0; 128];
    b[..7].copy_from_slice(b"\x7fELF\x02\x01\x01");
    b[16..18].copy_from_slice(&2u16.to_le_bytes());
    b[18..20].copy_from_slice(&62u16.to_le_bytes());
    b[20..24].copy_from_slice(&1u32.to_le_bytes());
    b[32..40].copy_from_slice(&64u64.to_le_bytes());
    b[52..54].copy_from_slice(&64u16.to_le_bytes());
    b[54..56].copy_from_slice(&56u16.to_le_bytes());
    b[56..58].copy_from_slice(&1u16.to_le_bytes());
    b[64..68].copy_from_slice(&1u32.to_le_bytes()); // PT_LOAD
    b[68..72].copy_from_slice(&5u32.to_le_bytes());
    b[96..104].copy_from_slice(&128u64.to_le_bytes());
    b[104..112].copy_from_slice(&128u64.to_le_bytes());
    b
}
pub fn kernel() -> Vec<u8> {
    let mut b = vec![0; 4096];
    b[0x1f1] = 4;
    b[0x1f4..0x1f8].copy_from_slice(&96u32.to_le_bytes());
    b[510..512].copy_from_slice(&[0x55, 0xaa]);
    b[514..518].copy_from_slice(b"HdrS");
    b[518..520].copy_from_slice(&0x020cu16.to_le_bytes());
    b[0x211] = 1;
    b[0x214..0x218].copy_from_slice(&0x100000u32.to_le_bytes());
    b[0x22c..0x230].copy_from_slice(&0x37ffffffu32.to_le_bytes());
    b[0x230..0x234].copy_from_slice(&0x200000u32.to_le_bytes());
    b[0x234] = 1;
    b[0x236..0x238].copy_from_slice(&3u16.to_le_bytes());
    b[0x258..0x260].copy_from_slice(&0x100000u64.to_le_bytes());
    b[0x260..0x264].copy_from_slice(&4096u32.to_le_bytes());
    b
}
pub fn files() -> BTreeMap<String, Vec<u8>> {
    let mut files = BTreeMap::from([
        ("bzImage".into(), kernel()),
        ("compose-init".into(), elf()),
        ("busybox".into(), elf()),
        ("mke2fs".into(), elf()),
        (
            "NOTICE.txt".into(),
            b"Synthetic parser fixtures only. No redistributed upstream binary.\n".to_vec(),
        ),
    ]);
    let sha = "1".repeat(64);
    let recipes = [
        "flake.lock",
        "flake.nix",
        "guest/compose-init.c",
        "guest/control.h",
        "guest/barrier.h",
        "guest/io.h",
        "kernel-arm64/supervisor-io.h",
        "kernel/apply.py",
        "kernel/config.options",
        "kernel/build.sh",
        "guest/build-compose.sh",
        "tools/linux-runtime/package.py",
        "tools/linux-runtime/build-cli.sh",
    ]
    .into_iter()
    .map(|name| (name, sha.clone()))
    .collect::<BTreeMap<_, _>>();
    let artifacts = ["bzImage", "compose-init", "busybox", "mke2fs"]
        .into_iter()
        .map(|name| (name, hash(&files[name])))
        .collect::<BTreeMap<_, _>>();
    let provenance = json!({"format":1,"source_revision":"a".repeat(40),"source_dirty":false,
        "flake_lock_sha256":sha,"notice_sha256":hash(&files["NOTICE.txt"]),"recipes":recipes,
        "sources":{"linux":{"sha256":sha},"busybox":{"sha256":sha},"e2fsprogs":{"sha256":sha},"musl":{"sha256":sha}},
        "artifacts":artifacts});
    files.insert(
        "provenance.json".into(),
        serde_json::to_vec(&provenance).unwrap(),
    );
    files
}
pub fn manifest(files: &BTreeMap<String, Vec<u8>>) -> Value {
    let roles = BTreeMap::from([
        ("bzImage", "kernel"),
        ("compose-init", "supervisor"),
        ("busybox", "busybox"),
        ("mke2fs", "filesystem_formatter"),
        ("NOTICE.txt", "notice"),
        ("provenance.json", "provenance"),
    ]);
    json!({"format":1,"host_platform":PLATFORM,"guest_arch":"x86_64","files":files.iter()
        .map(|(name,bytes)|(name,json!({"role":roles[name.as_str()],"size":bytes.len(),"sha256":hash(bytes),"execution_arch":if ["compose-init","busybox","mke2fs"].contains(&name.as_str()) {Some("x86_64")} else {None}}))).collect::<BTreeMap<_,_>>()})
}
pub fn archive(files: &BTreeMap<String, Vec<u8>>, manifest: &Value) -> Vec<u8> {
    let entries = std::iter::once((
        "assets.json".to_owned(),
        serde_json::to_vec(manifest).unwrap(),
        b'0',
    ))
    .chain(
        files
            .iter()
            .map(|(name, bytes)| (name.clone(), bytes.clone(), b'0')),
    )
    .collect::<Vec<_>>();
    archive_entries(&entries)
}
pub fn archive_entries(entries: &[(String, Vec<u8>, u8)]) -> Vec<u8> {
    let mut tar = tar::Builder::new(Vec::new());
    for (name, bytes, kind) in entries {
        let mut header = tar::Header::new_ustar();
        header.set_mode(0o600);
        header.set_size(bytes.len() as u64);
        header.set_mtime(0);
        header.set_entry_type(tar::EntryType::new(*kind));
        let name = name.as_bytes();
        assert!(name.len() < 100);
        header.as_mut_bytes()[..name.len()].copy_from_slice(name);
        if *kind == b'1' || *kind == b'2' {
            header.set_link_name("bzImage").unwrap();
        }
        header.set_cksum();
        tar.append(&header, bytes.as_slice()).unwrap();
    }
    gzip(&tar.into_inner().unwrap())
}
pub fn gzip(raw: &[u8]) -> Vec<u8> {
    let mut gzip = GzEncoder::new(Vec::new(), Compression::fast());
    gzip.write_all(raw).unwrap();
    gzip.finish().unwrap()
}

pub fn package() -> Vec<u8> {
    let files = files();
    archive(&files, &manifest(&files))
}
