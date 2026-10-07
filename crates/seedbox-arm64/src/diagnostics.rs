//! Optional stopped-memory diagnostics. This format is not a witness input.
use crate::{
    witness::{hash, Report},
    Result, RAM_BASE, TIME_ADDR,
};
use serde::Serialize;
use std::io::Write;

// Linux guest granularity, independent of the host's 16 KiB mapping alignment.
const PAGE_BYTES: usize = 4096;
#[derive(Serialize)]
struct Region {
    name: &'static str,
    base: u64,
    length: usize,
    // Entry i covers [base + i * page_size, base + (i+1) * page_size).
    sha256: Vec<String>,
}
fn region(name: &'static str, base: u64, bytes: &[u8]) -> Region {
    Region {
        name,
        base,
        length: bytes.len(),
        sha256: bytes.chunks(PAGE_BYTES).map(hash).collect(),
    }
}
pub fn write_ram_pages(
    output: &mut dyn Write,
    report: &Report,
    ram: &[u8],
    pv: &[u8],
) -> Result<()> {
    #[derive(Serialize)]
    struct Pages<'a> {
        format: u32,
        page_size: usize,
        witness: &'a str,
        ram_hash: &'a str,
        kernel_sha256: &'a str,
        initrd_sha256: &'a str,
        seed: u64,
        regions: [Region; 2],
    }
    let pages = Pages {
        format: 1,
        page_size: PAGE_BYTES,
        witness: &report.witness,
        ram_hash: &report.ram_hash,
        kernel_sha256: &report.kernel_sha256,
        initrd_sha256: &report.initrd_sha256,
        seed: report.seed,
        regions: [
            region("ram", RAM_BASE, ram),
            region("pv_time", TIME_ADDR, pv),
        ],
    };
    serde_json::to_writer_pretty(&mut *output, &pages)
        .map_err(|e| format!("write RAM page diagnostics: {e}"))?;
    output
        .write_all(b"\n")
        .and_then(|_| output.flush())
        .map_err(|e| format!("write RAM page diagnostics: {e}"))
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn page_hashes_locate_changes_including_zero_pages() {
        let mut bytes = vec![0; PAGE_BYTES * 3];
        bytes[PAGE_BYTES + 17] = 42;
        let pages = region("ram", RAM_BASE, &bytes);
        assert_eq!(pages.sha256.len(), 3);
        assert_eq!(pages.sha256[0], hash(&vec![0; PAGE_BYTES]));
        assert_eq!(pages.sha256[0], pages.sha256[2]);
        assert_eq!(pages.sha256[1], hash(&bytes[PAGE_BYTES..2 * PAGE_BYTES]));
        assert_ne!(pages.sha256[0], pages.sha256[1]);
    }
}
