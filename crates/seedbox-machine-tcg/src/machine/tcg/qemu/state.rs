//! Specific stopped QMP operations. No spawn, lifetime, GDB or guest execution.
use super::{hex, json, path_text, Error, Process, Qmp, Value};
use seedbox_machine::machine::RamRegion;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::{fs, io::Read, path::Path};

/// Explicit per-boot authority supplied before execution or incoming restore.
pub(in crate::machine::tcg) struct TimerSetup {
    pub seed: u64,
    pub domain: String,
    pub jitter: u64,
}

pub(super) fn configure(qmp: &mut Qmp, setup: &TimerSetup) -> Result<(), Error> {
    let result = qmp.command(
        "seedbox-timer-policy",
        json!({
            "seed": setup.seed, "domain": setup.domain, "jitter": setup.jitter,
        }),
    )?;
    if result.as_object().is_none_or(|fields| !fields.is_empty()) {
        return Err(Error(
            "TCG timer-policy reply is not an empty object".into(),
        ));
    }
    Ok(())
}

/// Native parts only. The machine adds its pending-I/O and disk evidence,
/// lifetime identity and backend-defined combined digest at the same stop.
pub(in crate::machine::tcg) struct NativeEvidence {
    pub parts: Vec<(String, String)>,
    pub regions: Vec<RamRegion>,
    pub page_hashes: Vec<u8>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireRegion {
    base: u64,
    length: u64,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireEvidence {
    regs: String,
    vectors: String,
    events: String,
    msrs: String,
    timers: String,
    #[serde(rename = "timer-policy")]
    timer_policy: String,
    ram: String,
    #[serde(rename = "ram-regions")]
    ram_regions: String,
    regions: Vec<WireRegion>,
    #[serde(rename = "page-bytes")]
    page_bytes: u64,
}

fn metadata(value: Value, memory_bytes: u64) -> Result<(NativeEvidence, u64), Error> {
    let wire: WireEvidence = serde_json::from_value(value)
        .map_err(|e| Error(format!("TCG stopped-state metadata: {e}")))?;
    let bad = |detail| Error(format!("TCG stopped-state evidence: {detail}"));
    if wire.regions.is_empty() {
        return Err(bad("missing physical RAM regions"));
    }
    let mut end = 0;
    let mut capacity = 0u64;
    let mut page_bytes = 0u64;
    let mut layout = Sha256::new();
    for region in &wire.regions {
        if region.length == 0 || !region.base.is_multiple_of(4096) || region.base < end {
            return Err(bad("invalid or unordered physical RAM region"));
        }
        end = region
            .base
            .checked_add(region.length)
            .ok_or_else(|| bad("RAM address overflow"))?;
        capacity = capacity
            .checked_add(region.length)
            .ok_or_else(|| bad("RAM capacity overflow"))?;
        if capacity > memory_bytes {
            return Err(bad("mapped RAM exceeds configured capacity"));
        }
        page_bytes = region
            .length
            .div_ceil(4096)
            .checked_mul(8)
            .and_then(|n| page_bytes.checked_add(n))
            .ok_or_else(|| bad("RAM page count overflow"))?;
        layout.update(region.base.to_le_bytes());
        layout.update(region.length.to_le_bytes());
    }
    if page_bytes != wire.page_bytes {
        return Err(bad("RAM page count mismatch"));
    }
    if hex(&layout.finalize()) != wire.ram_regions {
        return Err(bad("physical RAM region digest mismatch"));
    }
    let parts = vec![
        ("regs".into(), wire.regs),
        ("vectors".into(), wire.vectors),
        ("events".into(), wire.events),
        ("msrs".into(), wire.msrs),
        ("timers".into(), wire.timers),
        ("timer_policy".into(), wire.timer_policy),
        ("ram".into(), wire.ram),
        ("ram_regions".into(), wire.ram_regions),
    ];
    for (_, hash) in &parts {
        if hash.len() != 64
            || !hash
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(bad("invalid component SHA-256"));
        }
    }
    Ok((
        NativeEvidence {
            parts,
            regions: wire
                .regions
                .into_iter()
                .map(|r| RamRegion {
                    base: r.base,
                    length: r.length,
                })
                .collect(),
            page_hashes: Vec::new(),
        },
        page_bytes,
    ))
}

fn read_pages(path: &Path, expected: u64) -> Result<Vec<u8>, Error> {
    let bad = |e| Error(format!("TCG stopped-state pages {}: {e}", path.display()));
    let meta = fs::symlink_metadata(path).map_err(|e| bad(e.to_string()))?;
    if !meta.file_type().is_file() || meta.len() != expected {
        return Err(bad(
            "not a regular file of the expected page-record length".into()
        ));
    }
    let length = usize::try_from(expected)
        .map_err(|_| bad("page-record length exceeds address space".into()))?;
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(length)
        .map_err(|e| bad(format!("cannot allocate page records: {e}")))?;
    let limit = expected
        .checked_add(1)
        .ok_or_else(|| bad("page-record length overflow".into()))?;
    fs::File::open(path)
        .map_err(|e| bad(e.to_string()))?
        .take(limit)
        .read_to_end(&mut bytes)
        .map_err(|e| bad(e.to_string()))?;
    if bytes.len() != length {
        return Err(bad("page-record file changed while reading".into()));
    }
    Ok(bytes)
}

impl Process {
    /// Read actual state at a stopped boundary without touching guest outputs.
    /// Page records use caller-owned scratch, not an enlarged QMP packet limit.
    pub(in crate::machine::tcg) fn state_evidence(
        &mut self,
        memory_bytes: u64,
    ) -> Result<NativeEvidence, Error> {
        let directory = tempfile::Builder::new()
            .prefix("state-evidence-")
            .tempdir_in(self.directory.path())
            .map_err(|e| Error(format!("TCG state-evidence scratch: {e}")))?;
        let path = directory.path().join("pages");
        let value = self.qmp.command(
            "seedbox-x86-evidence",
            json!({
                "page-file": path_text(&path, "TCG evidence page path")?,
            }),
        )?;
        let (mut evidence, expected) = metadata(value, memory_bytes)?;
        evidence.page_hashes = read_pages(&path, expected)?;
        Ok(evidence)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn wire() -> Value {
        let mut layout = Sha256::new();
        layout.update(0u64.to_le_bytes());
        layout.update(17u64.to_le_bytes());
        json!({"regs":"a".repeat(64),"vectors":"b".repeat(64),"events":"c".repeat(64),
            "msrs":"d".repeat(64),"timers":"e".repeat(64),"timer-policy":"f".repeat(64),
            "ram":"0".repeat(64),"ram-regions":hex(&layout.finalize()),
            "regions":[{"base":0,"length":17}],"page-bytes":8})
    }
    #[test]
    fn validates_native_metadata_before_reading_or_allocating_pages() {
        let (evidence, bytes) = metadata(wire(), 17).unwrap();
        assert_eq!(bytes, 8);
        assert_eq!(
            evidence.regions,
            vec![RamRegion {
                base: 0,
                length: 17
            }]
        );
        assert_eq!(evidence.parts.len(), 8);
        assert!(metadata(wire(), 16).is_err());
        for (key, value) in [
            ("page-bytes", json!(16)),
            ("ram-regions", json!("0".repeat(64))),
            ("regs", json!("A".repeat(64))),
            ("regions", json!([])),
            ("unknown", json!(0)),
        ] {
            let mut bad = wire();
            bad[key] = value;
            assert!(metadata(bad, 17).is_err(), "{key}");
        }
    }
    #[test]
    fn rejects_unordered_overlap_unaligned_and_overflowing_physical_ram() {
        for regions in [
            json!([{"base":1,"length":17}]),
            json!([{"base":4096,"length":1},{"base":0,"length":1}]),
            json!([{"base":0,"length":4097},{"base":4096,"length":1}]),
            json!([{"base":u64::MAX - 4095,"length":4096}]),
        ] {
            let mut bad = wire();
            bad["regions"] = regions;
            assert!(metadata(bad, u64::MAX).is_err());
        }
    }
    #[test]
    fn validates_exact_page_file_length_and_refuses_symlinks() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("pages");
        fs::write(&path, [1; 8]).unwrap();
        assert_eq!(read_pages(&path, 8).unwrap(), [1; 8]);
        assert!(read_pages(&path, 7).is_err());
        assert!(read_pages(&path, 9).is_err());
        let link = directory.path().join("link");
        std::os::unix::fs::symlink(&path, &link).unwrap();
        assert!(read_pages(&link, 8).is_err());
    }
}
