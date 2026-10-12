//! Semantic checkpoint selection and branch identity. No host state or RNG draws.
use crate::world_config::WorldConfig;
use sha2::{Digest, Sha256};
use std::path::Path;

/// Incremental SHA-256 state, using RustCrypto's versioned 0.11 state format.
/// The containing checkpoint version must change if this format changes.
pub mod sha256 {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    use sha2::{
        digest::common::hazmat::{SerializableState, SerializedState},
        Sha256,
    };

    pub fn serialize<S: Serializer>(hash: &Sha256, serializer: S) -> Result<S::Ok, S::Error> {
        SerializableState::serialize(hash)
            .as_slice()
            .serialize(serializer)
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Sha256, D::Error> {
        let bytes = Vec::<u8>::deserialize(deserializer)?;
        let mut state = SerializedState::<Sha256>::default();
        if bytes.len() != state.len() {
            return Err(serde::de::Error::custom(
                "invalid SHA-256 checkpoint state length",
            ));
        }
        state.copy_from_slice(&bytes);
        <Sha256 as SerializableState>::deserialize(&state).map_err(serde::de::Error::custom)
    }
}

pub(crate) mod ordered_maps {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    use std::collections::BTreeMap;

    pub fn serialize<K: Serialize, V: Serialize, S: Serializer>(
        maps: &[BTreeMap<K, V>],
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        maps.iter()
            .map(|map| map.iter().collect::<Vec<_>>())
            .collect::<Vec<_>>()
            .serialize(serializer)
    }
    pub fn deserialize<
        'de,
        K: Deserialize<'de> + Ord,
        V: Deserialize<'de>,
        D: Deserializer<'de>,
    >(
        deserializer: D,
    ) -> Result<Vec<BTreeMap<K, V>>, D::Error> {
        Vec::<Vec<(K, V)>>::deserialize(deserializer)?
            .into_iter()
            .map(super::ordered_map::collect)
            .collect::<Result<_, _>>()
            .map_err(serde::de::Error::custom)
    }
}

/// JSON-compatible ordered maps with compound keys. Reject duplicate or
/// reordered entries instead of silently replacing retained state.
pub(crate) mod ordered_map {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    use std::collections::BTreeMap;

    pub fn serialize<K: Serialize, V: Serialize, S: Serializer>(
        map: &BTreeMap<K, V>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        map.iter().collect::<Vec<_>>().serialize(serializer)
    }

    pub fn collect<K: Ord, V>(entries: Vec<(K, V)>) -> Result<BTreeMap<K, V>, String> {
        let mut map = BTreeMap::new();
        for (key, value) in entries {
            if map.last_key_value().is_some_and(|(last, _)| last >= &key) {
                return Err("checkpoint map keys must be strictly ordered".into());
            }
            map.insert(key, value);
        }
        Ok(map)
    }

    pub fn deserialize<
        'de,
        K: Deserialize<'de> + Ord,
        V: Deserialize<'de>,
        D: Deserializer<'de>,
    >(
        deserializer: D,
    ) -> Result<BTreeMap<K, V>, D::Error> {
        collect(Vec::<(K, V)>::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

/// Captured execution state retained by a live engine owner. Durable server
/// resources must additionally persist their coordinator and replay metadata.
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(
    deny_unknown_fields,
    bound(
        serialize = "M::Snapshot: serde::Serialize",
        deserialize = "M::Snapshot: serde::Deserialize<'de>"
    )
)]
pub struct LiveCheckpoint<M: crate::world::WorldMachine> {
    pub world: crate::world::WorldSnapshot<M>,
    pub replay: seedbox_engine_protocol::intervention::ReplayCheckpoint,
    pub position: seedbox_engine_protocol::host_control::BoundaryPosition,
}

pub struct LiveCheckpoints<M: crate::world::WorldMachine> {
    held: std::collections::BTreeMap<String, LiveCheckpoint<M>>,
}
impl<M: crate::world::WorldMachine> Default for LiveCheckpoints<M> {
    fn default() -> Self {
        Self {
            held: Default::default(),
        }
    }
}
impl<M: crate::world::WorldMachine> LiveCheckpoints<M> {
    /// Fixed admission bound while the engine owns full machine snapshots.
    pub fn configure(&self, host: &mut crate::host_control::HostControl) {
        if self.held.len() < 8 {
            host.enable_checkpoints();
        } else {
            host.disable_checkpoints();
        }
    }

    pub fn get(&self, request_id: &str) -> Option<&LiveCheckpoint<M>> {
        self.held.get(request_id)
    }

    pub fn capture_pending(
        &mut self,
        world: &mut crate::world::World<M>,
        replay: &crate::intervention::Replay,
        host: &mut crate::host_control::HostControl,
    ) -> Result<bool, String> {
        self.capture_pending_with(world, replay, host, |_, _| Ok(()))
    }

    /// The owner publishes backend-specific durable state before capture is
    /// acknowledged. Publication failure stops the run without a success reply.
    pub fn capture_pending_with(
        &mut self,
        world: &mut crate::world::World<M>,
        replay: &crate::intervention::Replay,
        host: &mut crate::host_control::HostControl,
        publish: impl FnOnce(&str, &LiveCheckpoint<M>) -> Result<(), String>,
    ) -> Result<bool, String> {
        let Some(request) = host.pending_checkpoint().cloned() else {
            return Ok(false);
        };
        if self.held.len() >= 8 || self.held.contains_key(&request.request_id) {
            return Err("checkpoint retention capacity or identity conflict".into());
        }
        if world.slices != request.position.completed_rounds
            || world.boundaries != request.position.boundary_index
        {
            return Err("checkpoint capture moved past the requested boundary".into());
        }
        let replay = replay.checkpoint(request.position)?;
        let snapshot = crate::world::WorldSnapshot::take_checkpoint(world)?;
        let checkpoint = LiveCheckpoint {
            world: snapshot,
            replay,
            position: request.position,
        };
        publish(&request.request_id, &checkpoint)?;
        self.held.insert(request.request_id.clone(), checkpoint);
        host.checkpoint_captured(&request.request_id)
            .map_err(|e| e.to_string())?;
        self.configure(host);
        Ok(true)
    }
}

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
    hex::encode(h.finalize())
}

/// Replace artifact paths with their bytes' hashes: relocation is not semantic.
/// Performance settings and output paths live outside the world config.
/// Deferred TX and the page timer are semantic, so both bind the parent.
///
/// The CLI and the engine both hash the typed world document, without its
/// `checkpoint`, so they agree whatever the document's bytes on disk.
///
/// The engine build is not part of it: a checkpoint continues on whatever
/// engine runs it, and an incompatible snapshot fails to restore.
pub fn parent_identity(
    cfg: &WorldConfig,
    seed: u64,
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
    h.update(b"seedbox-world-parent-v4\0");
    h.update(seed.to_le_bytes());
    h.update(serde_json::to_vec(&cfg).map_err(|e| e.to_string())?);
    if deferred_tx {
        h.update(b"\0deferred_tx=true");
    }
    if page_timer {
        h.update(b"\0page_timer=true");
    }
    Ok(hex::encode(h.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_state_restoration_preserves_partial_blocks_and_future_bytes() {
        #[derive(serde::Serialize, serde::Deserialize)]
        struct Saved(#[serde(with = "super::sha256")] Sha256);
        let input: Vec<u8> = (0..1000).map(|n| (n % 251) as u8).collect();
        for split in [0, 1, 55, 56, 63, 64, 65, 127, 128, 999, 1000] {
            let saved = Saved(Sha256::new_with_prefix(&input[..split]));
            let bytes = serde_json::to_vec(&saved).unwrap();
            let mut restored: Saved = serde_json::from_slice(&bytes).unwrap();
            restored.0.update(&input[split..]);
            assert_eq!(restored.0.finalize(), Sha256::digest(&input));
        }
        assert!(serde_json::from_str::<Saved>("[1,2,3]").is_err());
        assert_eq!(
            hex::encode(Sha256::digest(b"abc")),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

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
        let parent = parent_identity(&cfg, 7, false, false, content).unwrap();
        let moved = world("x", "y", "z");
        assert_eq!(
            parent,
            parent_identity(&moved, 7, false, false, content).unwrap()
        );
        assert_ne!(
            parent,
            parent_identity(&cfg, 8, false, false, content).unwrap()
        );
        assert_ne!(
            parent,
            parent_identity(&cfg, 7, false, false, |_| Ok("changed".into())).unwrap()
        );
        let deferred = parent_identity(&cfg, 7, true, false, content).unwrap();
        assert_ne!(parent, deferred);
        assert_ne!(
            branch_identity(&parent, 10, 20),
            branch_identity(&deferred, 10, 20)
        );
        assert_eq!(
            deferred,
            parent_identity(&moved, 7, true, false, content).unwrap()
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
                let parent = parent_identity(&cfg, 7, deferred_tx, page_timer, content).unwrap();
                assert_eq!(
                    parent,
                    parent_identity(&cfg, 7, deferred_tx, page_timer, content).unwrap()
                );
                branches.insert(branch_identity(&parent, 10, 20));
                parents.insert(parent);
            }
        }
        assert_eq!(parents.len(), 4);
        assert_eq!(branches.len(), 4);
    }
}
