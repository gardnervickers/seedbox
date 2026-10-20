//! Stopped physical-state evidence, independent of snapshot encodings.

/// One actual guest RAM extent, ordered by guest physical address.
/// Holes are not RAM and contribute neither bytes nor page hashes.
// TODO: Re-evaluate: sparse RAM needs address metadata at the common evidence
// boundary; a flat page vector cannot identify pages across MMIO holes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RamRegion {
    pub base: u64,
    pub length: u64,
}
crate::record!(RamRegion {
    base: u64,
    length: u64,
});

/// The lifetime represented by a stopped-state capture.
// TODO: Re-evaluate: teardown destroys native RAM; callers must distinguish a
// retained ended boot from a current live boot without inventing successor RAM.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EvidenceDisposition {
    Live,
    Crashed,
    Shutdown,
    Exited { code: i32 },
}

/// Actual state of the live boot, or the most recently ended boot.
///
/// Parts are ordered, uniquely named lowercase SHA-256 digests. `ram` hashes
/// actual mapped RAM bytes concatenated in ascending guest physical address
/// order, excluding holes. `ram_regions` hashes the region list encoded as
/// consecutive little-endian `(base: u64, length: u64)` pairs. Both participate
/// in `digest`, whose ordered-part encoding is defined by the backend. An
/// attached disk requires a `disk` part covering its pre-loss state.
///
/// `page_hashes` concatenates, per region in order, the first eight SHA-256
/// bytes of each 4096-byte RAM chunk. A final partial chunk is not padded.
/// Each eight-byte record is the legacy little-endian u64 representation.
/// The region list maps these records to guest physical page addresses.
///
/// Evidence is not a snapshot digest and cannot be compared across backends.
/// Ended evidence is captured before teardown and disk loss, and retained in
/// snapshots together with its pages. Earlier ended boots are published through
/// boot-end outputs, not returned again by this getter after a later boot.
// TODO: Re-evaluate: backend-specific CPU/device projections coexist, while
// physical RAM, page addressing and lifetime identity need one service boundary.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StateEvidence {
    pub boot: u64,
    pub disposition: EvidenceDisposition,
    pub parts: Vec<(String, String)>,
    pub regions: Vec<RamRegion>,
    pub page_hashes: Vec<u8>,
    pub digest: String,
}

impl StateEvidence {
    /// Validate common shape. The backend additionally validates its exact part
    /// set and computes the digests from stopped state, never serialised state.
    pub fn validate(&self) -> Result<(), &'static str> {
        fn sha256(value: &str) -> bool {
            value.len() == 64
                && value
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        }
        if !sha256(&self.digest) {
            return Err("state evidence: invalid combined SHA-256");
        }
        let mut names = std::collections::BTreeSet::new();
        for (name, digest) in &self.parts {
            if name.is_empty() || !names.insert(name.as_str()) || !sha256(digest) {
                return Err("state evidence: invalid or repeated part");
            }
        }
        if !names.contains("ram") || !names.contains("ram_regions") {
            return Err("state evidence: missing physical RAM or region digest");
        }
        if self.regions.is_empty() {
            return Err("state evidence: missing physical RAM regions");
        }
        let mut end = 0;
        let mut pages = 0u64;
        for region in &self.regions {
            if region.length == 0 || !region.base.is_multiple_of(4096) || region.base < end {
                return Err("state evidence: invalid physical RAM region");
            }
            end = region
                .base
                .checked_add(region.length)
                .ok_or("state evidence: physical RAM address overflow")?;
            pages = pages
                .checked_add(region.length.div_ceil(4096))
                .ok_or("state evidence: page count overflow")?;
        }
        let bytes = pages
            .checked_mul(8)
            .ok_or("state evidence: page count overflow")?;
        if u64::try_from(self.page_hashes.len()).ok() != Some(bytes) {
            return Err("state evidence: physical RAM page count mismatch");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn evidence() -> StateEvidence {
        StateEvidence {
            boot: 1,
            disposition: EvidenceDisposition::Live,
            parts: vec![
                ("ram".into(), "a".repeat(64)),
                ("ram_regions".into(), "b".repeat(64)),
            ],
            regions: vec![
                RamRegion {
                    base: 0,
                    length: 4096,
                },
                RamRegion {
                    base: 0x10000,
                    length: 17,
                },
            ],
            page_hashes: vec![0; 16],
            digest: "c".repeat(64),
        }
    }
    #[test]
    fn physical_regions_exclude_holes_and_count_partial_pages() {
        assert_eq!(evidence().validate(), Ok(()));
        let mut e = evidence();
        e.page_hashes.truncate(8);
        assert_eq!(
            e.validate(),
            Err("state evidence: physical RAM page count mismatch")
        );
    }
    #[test]
    fn rejects_unsorted_overlapping_empty_unaligned_and_overflowing_regions() {
        for regions in [
            vec![
                RamRegion {
                    base: 0x1000,
                    length: 4096,
                },
                RamRegion {
                    base: 0,
                    length: 4096,
                },
            ],
            vec![
                RamRegion {
                    base: 0,
                    length: 4097,
                },
                RamRegion {
                    base: 4096,
                    length: 4096,
                },
            ],
            vec![RamRegion { base: 0, length: 0 }],
            vec![RamRegion {
                base: 1,
                length: 4096,
            }],
            vec![RamRegion {
                base: u64::MAX - 4095,
                length: 4096,
            }],
            vec![],
        ] {
            let mut e = evidence();
            e.regions = regions;
            assert!(e.validate().is_err());
        }
    }
    #[test]
    fn rejects_missing_repeated_and_malformed_parts() {
        for parts in [
            vec![],
            vec![("ram".into(), "a".repeat(64))],
            vec![("ram".into(), "A".repeat(64))],
            vec![("ram".into(), "a".repeat(64)); 2],
        ] {
            let mut e = evidence();
            e.parts = parts;
            assert!(e.validate().is_err());
        }
    }
}
