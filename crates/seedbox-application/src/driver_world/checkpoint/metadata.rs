//! Two-pass metadata decode: reject oversized collection structure before
//! derived serde allocates owned vectors. The input byte cap also bounds serde's
//! transient escaped-string scratch; logical quotas are not an RSS guarantee.
use serde::de::{self, DeserializeSeed, MapAccess, SeqAccess, Visitor};
use std::fmt;
const BYTES: usize = seedbox_engine_protocol::driver_world::MAX_ENVELOPE_BYTES as usize;
// Derived from the whole wire budget, not arbitrary semantic collection caps:
// a value costs >=1 encoded byte, an array item >=2 including separators, and
// an object entry >=4 (even an empty key and one-digit value). Fixed envelope
// schema nesting is below 32; recursive arbitrary JSON is not a valid field.
const NODES: usize = BYTES;
const ITEMS: usize = BYTES / 2;
const FIELDS: usize = BYTES / 4;
const DEPTH: usize = 32;
// Named handles have no semantic byte cap. A string may consume the complete
// existing envelope budget; an independent 64 KiB cap rejected valid programs.
const STRING: usize = seedbox_engine_protocol::driver_world::MAX_ENVELOPE_BYTES as usize;

pub(super) fn validate(bytes: &[u8]) -> Result<(), serde_json::Error> {
    let mut remaining = NODES;
    let mut deserializer = serde_json::Deserializer::from_slice(bytes);
    Value {
        remaining: &mut remaining,
        depth: 0,
    }
    .deserialize(&mut deserializer)?;
    deserializer.end()
}
pub(super) fn validate_reader(reader: impl std::io::Read) -> Result<(), serde_json::Error> {
    let mut remaining = NODES;
    let mut deserializer = serde_json::Deserializer::from_reader(reader);
    Value {
        remaining: &mut remaining,
        depth: 0,
    }
    .deserialize(&mut deserializer)?;
    deserializer.end()
}
struct Value<'a> {
    remaining: &'a mut usize,
    depth: usize,
}
impl<'de> DeserializeSeed<'de> for Value<'_> {
    type Value = ();
    fn deserialize<D: de::Deserializer<'de>>(self, deserializer: D) -> Result<(), D::Error> {
        if *self.remaining == 0 || self.depth > DEPTH {
            return Err(de::Error::custom(
                "Driver service metadata node/depth limit",
            ));
        }
        *self.remaining -= 1;
        deserializer.deserialize_any(self)
    }
}
impl<'de> Visitor<'de> for Value<'_> {
    type Value = ();
    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("bounded Driver service metadata")
    }
    fn visit_bool<E: de::Error>(self, _: bool) -> Result<(), E> {
        Ok(())
    }
    fn visit_i64<E: de::Error>(self, _: i64) -> Result<(), E> {
        Ok(())
    }
    fn visit_u64<E: de::Error>(self, _: u64) -> Result<(), E> {
        Ok(())
    }
    fn visit_f64<E: de::Error>(self, _: f64) -> Result<(), E> {
        Ok(())
    }
    fn visit_unit<E: de::Error>(self) -> Result<(), E> {
        Ok(())
    }
    fn visit_str<E: de::Error>(self, value: &str) -> Result<(), E> {
        if value.len() > STRING {
            return Err(E::custom("Driver service metadata string limit"));
        }
        Ok(())
    }
    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<(), A::Error> {
        let mut count = 0;
        while seq
            .next_element_seed(Value {
                remaining: self.remaining,
                depth: self.depth + 1,
            })?
            .is_some()
        {
            count += 1;
            if count > ITEMS {
                return Err(de::Error::custom(
                    "Driver service metadata collection limit",
                ));
            }
        }
        Ok(())
    }
    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<(), A::Error> {
        let mut count = 0;
        while map
            .next_key_seed(Value {
                remaining: self.remaining,
                depth: self.depth + 1,
            })?
            .is_some()
        {
            count += 1;
            if count > FIELDS {
                return Err(de::Error::custom("Driver service metadata field limit"));
            }
            map.next_value_seed(Value {
                remaining: self.remaining,
                depth: self.depth + 1,
            })?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_collection_depth_string_and_trailing_bytes_before_owned_decode() {
        assert!(validate(format!("[{}0]", "0,".repeat(ITEMS)).as_bytes())
            .unwrap_err()
            .to_string()
            .contains("collection limit"));
        assert!(validate(
            format!("{}0{}", "[".repeat(DEPTH + 1), "]".repeat(DEPTH + 1)).as_bytes()
        )
        .unwrap_err()
        .to_string()
        .contains("depth limit"));
        assert!(
            validate(format!("\"{}\"", "x".repeat(STRING + 1)).as_bytes())
                .unwrap_err()
                .to_string()
                .contains("string limit")
        );
        assert!(validate(b"{} {}").is_err());
        assert!(validate(b"{\"a\": [0, null, true, \"text\"]}").is_ok());
    }
}
