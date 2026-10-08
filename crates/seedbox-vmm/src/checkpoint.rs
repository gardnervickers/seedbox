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

/// Bind deferred TX to the parent while retaining the legacy timebase when off.
pub fn identity_timebase(timebase: &str, deferred_tx: bool) -> String {
    if deferred_tx { format!("{timebase}+deferred-tx") } else { timebase.to_owned() }
}

/// Replace artifact paths with their bytes' hashes: relocation is not semantic.
/// Performance settings and output paths live outside the world config.
/// Page timer is semantic but adds hash bytes only when enabled for compatibility.
pub fn parent_identity(
    cfg: &Value,
    seed: u64,
    engine: &str,
    timebase: &str,
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
    h.update(b"seedbox-world-parent-v2\0");
    h.update(engine.as_bytes());
    h.update(timebase.as_bytes());
    h.update(seed.to_le_bytes());
    h.update(serde_json::to_vec(&cfg).map_err(|e| e.to_string())?);
    if page_timer { h.update(b"\0page_timer=true"); }
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
        let parent = parent_identity(&cfg, 7, "engine", "counter", false, |_| Ok("content".into())).unwrap();
        let moved = json!({"kernel":"x", "machines":[{"initrd":"y","disk":{"image":"z"}}]});
        assert_eq!(
            parent,
            parent_identity(&moved, 7, "engine", "counter", false, |_| Ok("content".into())).unwrap()
        );
        assert_ne!(
            parent,
            parent_identity(&cfg, 8, "engine", "counter", false, |_| Ok("content".into())).unwrap()
        );
        assert_ne!(
            parent,
            parent_identity(&cfg, 7, "changed", "counter", false, |_| Ok("content".into())).unwrap()
        );
        assert_ne!(
            parent,
            parent_identity(&cfg, 7, "engine", "counter", false, |_| Ok("changed".into())).unwrap()
        );
        assert_ne!(parent, parent_identity(&cfg, 7, "engine", "pv", false, |_| Ok("content".into())).unwrap());
        for timebase in ["counter", "pv"] {
            let off = identity_timebase(timebase, false);
            assert_eq!(off, timebase);
            let on = identity_timebase(timebase, true);
            assert_eq!(on, format!("{timebase}+deferred-tx"));
            let ordinary = parent_identity(&cfg, 7, "engine", &off, false, |_| Ok("content".into())).unwrap();
            let deferred = parent_identity(&cfg, 7, "engine", &on, false, |_| Ok("content".into())).unwrap();
            assert_ne!(ordinary, deferred);
            assert_ne!(branch_identity(&ordinary, 10, 20), branch_identity(&deferred, 10, 20));
            assert_eq!(deferred, parent_identity(&moved, 7, "engine", &on, false, |_| Ok("content".into())).unwrap());
        }
        let id = branch_identity(&parent, 10, 20);
        assert_eq!(id, branch_identity(&parent, 10, 20));
        assert_ne!(id, branch_identity(&parent, 11, 20));
        assert_ne!(id, branch_identity(&parent, 10, 21));
        assert_ne!(id, branch_identity("other", 10, 20));
    }

    #[test]
    fn page_timer_binds_parent_and_preserves_pre_change_off_identity() {
        let cfg = json!({"kernel":"a", "machines":[{"initrd":"b","disk":{"image":"c"}}]});
        // Pre-page-timer parent hashes with the same engine and artifact bytes.
        for (timebase, legacy) in [
            ("counter", "ddc73738e0276b920d0f971f0434f5a348956718de53493db605d4df7b4dc935"),
            ("pv", "b8a5964827e10b048a4aad62e9628615146b70078ee369b9bfcf4f2f41590cfc"),
        ] {
            let off = parent_identity(&cfg, 7, "engine", &identity_timebase(timebase, false), false, |_| Ok("content".into())).unwrap();
            assert_eq!(off, legacy, "{timebase}");
            let on = parent_identity(&cfg, 7, "engine", timebase, true, |_| Ok("content".into())).unwrap();
            assert_ne!(on, off, "{timebase}");
            assert_ne!(branch_identity(&on, 10, 20), branch_identity(&off, 10, 20), "{timebase}");
            let mut parents = std::collections::BTreeSet::new();
            let mut branches = std::collections::BTreeSet::new();
            for page_timer in [false, true] {
                for deferred_tx in [false, true] {
                    let parent = parent_identity(&cfg, 7, "engine", &identity_timebase(timebase, deferred_tx), page_timer, |_| Ok("content".into())).unwrap();
                    branches.insert(branch_identity(&parent, 10, 20));
                    parents.insert(parent);
                }
            }
            assert_eq!(parents.len(), 4, "{timebase}");
            assert_eq!(branches.len(), 4, "{timebase}");
        }
    }
}
