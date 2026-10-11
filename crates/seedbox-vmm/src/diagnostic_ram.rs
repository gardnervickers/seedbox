//! Temporary capture for #304. Remove before an engine repair is merged.

use crate::machine::Machine;
use sha2::{Digest, Sha256};
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

pub(super) fn capture(machine: &mut Machine, out: &str) -> Result<(), String> {
    let root = Path::new("target/fullstate-probe");
    // Only the application acceptance fixture creates this directory.
    if !root.is_dir() {
        return Ok(());
    }
    let name = machine.world.name.clone();
    let base = root.join(&name);
    let hashes = std::fs::read(format!("{out}/{name}.pages")).map_err(|e| e.to_string())?;
    if !base.with_extension("pages").exists() {
        std::fs::write(base.with_extension("ram"), machine.memory()).map_err(|e| e.to_string())?;
        return std::fs::write(base.with_extension("pages"), hashes).map_err(|e| e.to_string());
    }
    let before = std::fs::read(base.with_extension("pages")).map_err(|e| e.to_string())?;
    if before.len() != hashes.len() {
        return Err("diagnostic baseline RAM size changed".into());
    }
    let mut saved = std::fs::File::open(base.with_extension("ram")).map_err(|e| e.to_string())?;
    let mut changed = Vec::new();
    let mut total = 0usize;
    for (pfn, (a, b)) in before
        .chunks_exact(8)
        .zip(hashes.chunks_exact(8))
        .enumerate()
    {
        if a == b {
            continue;
        }
        total += 1;
        if changed.len() >= 32 {
            continue;
        }
        let current = &machine.memory()[pfn * 4096..(pfn + 1) * 4096];
        saved
            .seek(SeekFrom::Start((pfn * 4096) as u64))
            .map_err(|e| e.to_string())?;
        let mut old = [0u8; 4096];
        saved.read_exact(&mut old).map_err(|e| e.to_string())?;
        let differences: Vec<_> = old
            .iter()
            .zip(current)
            .enumerate()
            .filter(|(_, (a, b))| a != b)
            .map(|(offset, (a, b))| serde_json::json!({"offset":offset,"before":a,"after":b}))
            .collect();
        std::fs::write(format!("{out}/{name}.pfn{pfn}.baseline"), old)
            .map_err(|e| e.to_string())?;
        std::fs::write(format!("{out}/{name}.pfn{pfn}.current"), current)
            .map_err(|e| e.to_string())?;
        changed.push(serde_json::json!({
            "pfn": pfn,
            "baseline_page_hash": hex::encode(a),
            "current_page_hash": hex::encode(b),
            "baseline_bytes_hash": hex::encode(&Sha256::digest(old)[..8]),
            "current_bytes_hash": hex::encode(&Sha256::digest(current)[..8]),
            "different_bytes": differences.len(),
            "first_byte_changes": &differences[..differences.len().min(64)],
        }));
    }
    std::fs::write(
        format!("{out}/{name}.changed-pages.json"),
        serde_json::to_vec_pretty(
            &serde_json::json!({"differing_pages":total,"first_pages":changed}),
        )
        .map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())
}
