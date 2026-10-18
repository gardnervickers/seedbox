//! Checkpoint serialization helpers for policy state. No host state or RNG draws.

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

#[cfg(test)]
mod tests {
    use super::ordered_map;

    #[test]
    fn ordered_map_rejects_duplicate_and_reordered_keys() {
        assert_eq!(
            ordered_map::collect(vec![(1, "a"), (2, "b")])
                .unwrap()
                .len(),
            2
        );
        assert!(ordered_map::collect(vec![(1, "a"), (1, "b")]).is_err());
        assert!(ordered_map::collect(vec![(2, "a"), (1, "b")]).is_err());
    }
}
