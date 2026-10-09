//! Semantic checkpoint selection and branch identity. No host state or RNG draws.
use crate::world_config::WorldConfig;
use sha2::{Digest, Sha256};
use std::path::Path;

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
    if deferred_tx {
        format!("{timebase}+deferred-tx")
    } else {
        timebase.to_owned()
    }
}

/// Replace artifact paths with their bytes' hashes: relocation is not semantic.
/// Performance settings and output paths live outside the world config.
/// Page timer is semantic but adds hash bytes only when enabled.
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
    timebase: &str,
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
    h.update(b"seedbox-world-parent-v2\0");
    h.update(engine.as_bytes());
    h.update(timebase.as_bytes());
    h.update(seed.to_le_bytes());
    h.update(serde_json::to_vec(&cfg).map_err(|e| e.to_string())?);
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
    fn times_reject_invalid_duplicate_reversed_and_duration_boundaries() {
        assert_eq!(times("0,5,9", 10).unwrap(), vec![0, 5, 9]);
        for bad in ["", "-1", "5,5", "9,5", "10", "5,", "18446744073709551616"] {
            assert!(times(bad, 10).is_err(), "{bad}");
        }
    }

    #[test]
    fn identity_binds_parent_time_seed_and_content_not_paths() {
        let cfg = world("a", "b", "c");
        let parent = parent_identity(
            &cfg,
            7,
            "engine",
            "counter",
            false,
            |_| Ok("content".into()),
        )
        .unwrap();
        let moved = world("x", "y", "z");
        assert_eq!(
            parent,
            parent_identity(&moved, 7, "engine", "counter", false, |_| Ok(
                "content".into()
            ))
            .unwrap()
        );
        assert_ne!(
            parent,
            parent_identity(
                &cfg,
                8,
                "engine",
                "counter",
                false,
                |_| Ok("content".into())
            )
            .unwrap()
        );
        assert_ne!(
            parent,
            parent_identity(&cfg, 7, "changed", "counter", false, |_| Ok(
                "content".into()
            ))
            .unwrap()
        );
        assert_ne!(
            parent,
            parent_identity(
                &cfg,
                7,
                "engine",
                "counter",
                false,
                |_| Ok("changed".into())
            )
            .unwrap()
        );
        assert_ne!(
            parent,
            parent_identity(&cfg, 7, "engine", "pv", false, |_| Ok("content".into())).unwrap()
        );
        for timebase in ["counter", "pv"] {
            let off = identity_timebase(timebase, false);
            assert_eq!(off, timebase);
            let on = identity_timebase(timebase, true);
            assert_eq!(on, format!("{timebase}+deferred-tx"));
            let ordinary =
                parent_identity(&cfg, 7, "engine", &off, false, |_| Ok("content".into())).unwrap();
            let deferred =
                parent_identity(&cfg, 7, "engine", &on, false, |_| Ok("content".into())).unwrap();
            assert_ne!(ordinary, deferred);
            assert_ne!(
                branch_identity(&ordinary, 10, 20),
                branch_identity(&deferred, 10, 20)
            );
            assert_eq!(
                deferred,
                parent_identity(&moved, 7, "engine", &on, false, |_| Ok("content".into())).unwrap()
            );
        }
        let id = branch_identity(&parent, 10, 20);
        assert_eq!(id, branch_identity(&parent, 10, 20));
        assert_ne!(id, branch_identity(&parent, 11, 20));
        assert_ne!(id, branch_identity(&parent, 10, 21));
        assert_ne!(id, branch_identity("other", 10, 20));
    }

    #[test]
    fn page_timer_and_deferred_tx_bind_the_parent() {
        let cfg = world("a", "b", "c");
        for timebase in ["counter", "pv"] {
            let off = parent_identity(
                &cfg,
                7,
                "engine",
                &identity_timebase(timebase, false),
                false,
                |_| Ok("content".into()),
            )
            .unwrap();
            let on = parent_identity(&cfg, 7, "engine", timebase, true, |_| Ok("content".into()))
                .unwrap();
            assert_ne!(on, off, "{timebase}");
            assert_ne!(
                branch_identity(&on, 10, 20),
                branch_identity(&off, 10, 20),
                "{timebase}"
            );
            let mut parents = std::collections::BTreeSet::new();
            let mut branches = std::collections::BTreeSet::new();
            for page_timer in [false, true] {
                for deferred_tx in [false, true] {
                    let parent = parent_identity(
                        &cfg,
                        7,
                        "engine",
                        &identity_timebase(timebase, deferred_tx),
                        page_timer,
                        |_| Ok("content".into()),
                    )
                    .unwrap();
                    branches.insert(branch_identity(&parent, 10, 20));
                    parents.insert(parent);
                }
            }
            assert_eq!(parents.len(), 4, "{timebase}");
            assert_eq!(branches.len(), 4, "{timebase}");
        }
    }
}
