//! Synthetic host-only worker replies, not native/physical RAM acceptance.
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    ffi::OsString,
    fs,
    io::Write,
    path::{Path, PathBuf},
};
fn hex(b: &[u8]) -> String {
    b.iter().map(|v| format!("{v:02x}")).collect()
}
pub(super) fn arguments(args: &mut Vec<OsString>) -> Result<(), Box<dyn std::error::Error>> {
    let executable = PathBuf::from(args.first().ok_or("missing argv0")?);
    if !executable
        .file_name()
        .is_some_and(|n| n.to_string_lossy().starts_with("pv-policy-fixture-"))
    {
        return Ok(());
    }
    let c: Value = serde_json::from_slice(&fs::read(executable.with_extension("config"))?)?;
    args.extend([
        "--scratch-fixture".into(),
        "--scratch-mode".into(),
        c["mode"].as_str().ok_or("fixture mode")?.into(),
        "--scratch-record".into(),
        c["record"].as_str().ok_or("fixture record")?.into(),
    ]);
    Ok(())
}
pub(super) fn evidence(
    path: &Path,
    root: &Path,
    memory: u64,
) -> Result<Value, Box<dyn std::error::Error>> {
    assert!(path.starts_with(root));
    assert!(
        memory <= 0x0a00_0000,
        "fixture models only the documented low-RAM layout"
    );
    let regions = [(0u64, memory), (0x0a00_0000, 16384)];
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?;
    let mut ram = Sha256::new();
    let mut layout = Sha256::new();
    let mut pages = Sha256::new();
    let mut count = 0;
    for (base, len) in regions {
        layout.update(base.to_le_bytes());
        layout.update(len.to_le_bytes());
        let mut left = len;
        while left > 0 {
            let n = left.min(4096) as usize;
            let chunk = vec![0x42; n];
            ram.update(&chunk);
            let d = Sha256::digest(chunk);
            file.write_all(&d[..8])?;
            pages.update(&d[..8]);
            count += 8;
            left -= n as u64;
        }
    }
    let hash = hex(&Sha256::digest(b"synthetic host worker part"));
    let mut parts: Vec<(String, String)> = [
        "regs",
        "sregs",
        "vectors",
        "xcrs",
        "msrs",
        "debugregs",
        "events",
        "vcpu",
        "cpuid",
        "clock",
        "devices",
        "console",
        "network",
        "pending_writes",
        "block_transport",
        "timer_policy",
    ]
    .into_iter()
    .map(|name| (name.into(), hash.clone()))
    .collect();
    parts.extend([
        ("ram".into(), hex(&ram.finalize())),
        ("ram_regions".into(), hex(&layout.finalize())),
        ("ram_pages".into(), hex(&pages.finalize())),
    ]);
    Ok(json!({"Evidence":{"parts":parts,"regions":regions,"page_bytes":count}}))
}
