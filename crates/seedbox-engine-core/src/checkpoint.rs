//! Checkpoint serialization helpers and branch identity. No host state or RNG draws.
use sha2::{Digest, Sha256};

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

pub fn branch_identity(parent: &str, at: u64, seed: u64) -> String {
    let mut h = Sha256::new();
    h.update(b"seedbox-checkpoint-branch-v1\0");
    h.update(parent.as_bytes());
    h.update(at.to_le_bytes());
    h.update(seed.to_le_bytes());
    hex::encode(h.finalize())
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

    #[test]
    fn branch_identity_binds_parent_time_and_seed() {
        let id = branch_identity("parent", 10, 20);
        assert_eq!(id, branch_identity("parent", 10, 20));
        assert_ne!(id, branch_identity("parent", 11, 20));
        assert_ne!(id, branch_identity("parent", 10, 21));
        assert_ne!(id, branch_identity("other", 10, 20));
    }
}
