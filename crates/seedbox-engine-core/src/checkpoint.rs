//! Semantic checkpoint selection and branch identity. No host state or RNG draws.
use crate::world_config::WorldConfig;
use sha2::{Digest, Sha256};
use std::path::Path;

/// Check runtime checkpoint times: strictly increasing and before `duration`.
pub fn check_times(times: &[u64], duration: u64) -> Result<(), String> {
    let mut last = None;
    for &t in times {
        if t >= duration || last.is_some_and(|last| last >= t) {
            return Err(
                "--checkpoint-at-ns must be strictly increasing and before duration_ns".into(),
            );
        }
        last = Some(t);
    }
    Ok(())
}

pub fn branch_identity(parent: &str, at: u64, seed: u64) -> String {
    let mut h = Sha256::new();
    h.update(b"seedbox-checkpoint-branch-v1\0");
    h.update(parent.as_bytes());
    h.update(at.to_le_bytes());
    h.update(seed.to_le_bytes());
    format!("{:x}", h.finalize())
}

/// Replace artifact paths with their bytes' hashes: relocation is not semantic.
/// Performance settings and output paths live outside the world config.
/// Deferred TX and the page timer are semantic, so both bind the parent.
///
/// The CLI and the engine both hash the typed world document, without its
/// `checkpoint`, so they agree whatever the document's bytes on disk.
///
/// `engine` is the engine executable's digest. Binding a compiled binary
/// means a rebuild changes the identity; whether to keep it is the
/// checkpoint design's decision, not this function's.
pub fn parent_identity(
    cfg: &WorldConfig,
    seed: u64,
    engine: &str,
    deferred_tx: bool,
    page_timer: bool,
    mut digest: impl FnMut(&Path) -> Result<String, String>,
) -> Result<String, String> {
    let mut cfg = cfg.clone();
    cfg.checkpoint = None;
    cfg.kernel = digest(&cfg.kernel)?.into();
    for m in &mut cfg.machines {
        m.initrd = digest(&m.initrd)?.into();
        if let Some(image) = m.disk.as_mut().and_then(|d| d.image.as_mut()) {
            *image = digest(image)?.into();
        }
    }
    let mut h = Sha256::new();
    h.update(b"seedbox-world-parent-v3\0");
    h.update(engine.as_bytes());
    h.update(seed.to_le_bytes());
    h.update(serde_json::to_vec(&cfg).map_err(|e| e.to_string())?);
    if deferred_tx {
        h.update(b"\0deferred_tx=true");
    }
    if page_timer {
        h.update(b"\0page_timer=true");
    }
    Ok(format!("{:x}", h.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn world(kernel: &str, initrd: &str, image: &str) -> WorldConfig {
        serde_json::from_value(serde_json::json!({"kernel":kernel,
            "machines":[{"name":"a","initrd":initrd,"disk":{"image":image}}]}))
        .unwrap()
    }

    #[test]
    fn times_reject_duplicate_reversed_and_duration_boundaries() {
        check_times(&[0, 5, 9], 10).unwrap();
        check_times(&[], 10).unwrap();
        for bad in [&[5, 5][..], &[9, 5], &[10]] {
            assert!(check_times(bad, 10).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn identity_binds_parent_time_seed_and_content_not_paths() {
        let cfg = world("a", "b", "c");
        let content = |_: &Path| Ok("content".into());
        let parent = parent_identity(&cfg, 7, "engine", false, false, content).unwrap();
        let moved = world("x", "y", "z");
        assert_eq!(
            parent,
            parent_identity(&moved, 7, "engine", false, false, content).unwrap()
        );
        assert_ne!(
            parent,
            parent_identity(&cfg, 8, "engine", false, false, content).unwrap()
        );
        assert_ne!(
            parent,
            parent_identity(&cfg, 7, "changed", false, false, content).unwrap()
        );
        assert_ne!(
            parent,
            parent_identity(&cfg, 7, "engine", false, false, |_| Ok("changed".into())).unwrap()
        );
        let deferred = parent_identity(&cfg, 7, "engine", true, false, content).unwrap();
        assert_ne!(parent, deferred);
        assert_ne!(
            branch_identity(&parent, 10, 20),
            branch_identity(&deferred, 10, 20)
        );
        assert_eq!(
            deferred,
            parent_identity(&moved, 7, "engine", true, false, content).unwrap()
        );
        let id = branch_identity(&parent, 10, 20);
        assert_eq!(id, branch_identity(&parent, 10, 20));
        assert_ne!(id, branch_identity(&parent, 11, 20));
        assert_ne!(id, branch_identity(&parent, 10, 21));
        assert_ne!(id, branch_identity("other", 10, 20));
    }

    #[test]
    fn deferred_tx_and_page_timer_bind_distinct_parents() {
        let cfg = world("a", "b", "c");
        let content = |_: &Path| Ok("content".into());
        let mut parents = std::collections::BTreeSet::new();
        let mut branches = std::collections::BTreeSet::new();
        for page_timer in [false, true] {
            for deferred_tx in [false, true] {
                let parent =
                    parent_identity(&cfg, 7, "engine", deferred_tx, page_timer, content).unwrap();
                assert_eq!(
                    parent,
                    parent_identity(&cfg, 7, "engine", deferred_tx, page_timer, content).unwrap()
                );
                branches.insert(branch_identity(&parent, 10, 20));
                parents.insert(parent);
            }
        }
        assert_eq!(parents.len(), 4);
        assert_eq!(branches.len(), 4);
    }
}
