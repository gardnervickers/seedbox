//! Semantic checkpoint selection and branch identity. No host state or RNG draws.
use serde_json::Value;
use sha2::{Digest, Sha256};

pub fn times(list: &str, duration: u64) -> Result<Vec<u64>, String> {
    let mut times = Vec::new();
    for text in list.split(',') {
        let t = text
            .parse::<u64>()
            .map_err(|_| "--checkpoint-at-ns must be comma-separated unsigned nanoseconds")?;
        if t >= duration || times.last().is_some_and(|last| *last >= t) {
            return Err(
                "--checkpoint-at-ns must be strictly increasing and before duration_ns".into(),
            );
        }
        times.push(t);
    }
    Ok(times)
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
pub fn parent_identity(
    cfg: &Value,
    seed: u64,
    engine: &str,
    deferred_tx: bool,
    page_timer: bool,
    mut digest: impl FnMut(&str) -> Result<String, String>,
) -> Result<String, String> {
    let mut cfg = cfg.clone();
    cfg.as_object_mut()
        .ok_or("config must be an object")?
        .remove("checkpoint");
    let kernel = cfg["kernel"].as_str().ok_or("kernel required")?;
    cfg["kernel"] = Value::String(digest(kernel)?);
    for m in cfg["machines"].as_array_mut().ok_or("machines required")? {
        let initrd = m["initrd"].as_str().ok_or("initrd required")?;
        m["initrd"] = Value::String(digest(initrd)?);
        if let Some(path) = m["disk"]["image"].as_str() {
            m["disk"]["image"] = Value::String(digest(path)?);
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
    use serde_json::json;

    #[test]
    fn times_reject_invalid_duplicate_reversed_and_duration_boundaries() {
        assert_eq!(times("0,5,9", 10).unwrap(), vec![0, 5, 9]);
        for bad in ["", "-1", "5,5", "9,5", "10", "5,", "18446744073709551616"] {
            assert!(times(bad, 10).is_err(), "{bad}");
        }
    }

    #[test]
    fn identity_binds_parent_time_seed_and_content_not_paths() {
        let cfg = json!({"kernel":"a", "machines":[{"initrd":"b","disk":{"image":"c"}}]});
        let content = |_: &str| Ok("content".into());
        let parent = parent_identity(&cfg, 7, "engine", false, false, content).unwrap();
        let moved = json!({"kernel":"x", "machines":[{"initrd":"y","disk":{"image":"z"}}]});
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
        let cfg = json!({"kernel":"a", "machines":[{"initrd":"b","disk":{"image":"c"}}]});
        let content = |_: &str| Ok("content".into());
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
