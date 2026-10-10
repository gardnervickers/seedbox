use serde::{Deserialize, Serialize};
use std::{fmt, str::FromStr};

/// Short catalogue identity. Allocation and collision detection belong to the
/// server; an ID is neither a content digest nor an authorization credential.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct SimulationId(String);

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("simulation ID must contain eight characters from {alphabet}", alphabet = SimulationId::ALPHABET)]
pub struct InvalidSimulationId;

impl SimulationId {
    pub const LENGTH: usize = 8;
    pub const ALPHABET: &'static str = "23456789abcdefghjkmnpqrstuvwxyz";

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for SimulationId {
    type Error = InvalidSimulationId;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        if value.len() == Self::LENGTH
            && value
                .bytes()
                .all(|b| Self::ALPHABET.as_bytes().contains(&b))
        {
            Ok(Self(value))
        } else {
            Err(InvalidSimulationId)
        }
    }
}

impl FromStr for SimulationId {
    type Err = InvalidSimulationId;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        value.to_owned().try_into()
    }
}

impl From<SimulationId> for String {
    fn from(value: SimulationId) -> Self {
        value.0
    }
}

impl fmt::Display for SimulationId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_ids_are_validated_before_becoming_paths_or_catalogue_keys() {
        for invalid in [
            "../abcde",
            "ABCDEFGH",
            "abcdefgh/",
            "abc",
            "00000000",
            "éabcdef",
        ] {
            assert!(invalid.parse::<SimulationId>().is_err());
            assert!(
                serde_json::from_str::<SimulationId>(&serde_json::to_string(invalid).unwrap())
                    .is_err()
            );
        }
        let id: SimulationId = "k7m2p9qx".parse().unwrap();
        assert_eq!(serde_json::to_string(&id).unwrap(), "\"k7m2p9qx\"");
    }
}
