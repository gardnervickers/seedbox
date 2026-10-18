//! Lifecycle status shared by Driver fault evidence and observations.

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FaultStatus {
    /// Planned; its time has not come.
    Unrequested,
    /// Its time came; it waits for its guest effect.
    Requested,
    Applied,
    Failed,
    Unavailable,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_wire_names_round_trip() {
        for (status, name) in [
            (FaultStatus::Unrequested, "unrequested"),
            (FaultStatus::Requested, "requested"),
            (FaultStatus::Applied, "applied"),
            (FaultStatus::Failed, "failed"),
            (FaultStatus::Unavailable, "unavailable"),
        ] {
            assert_eq!(serde_json::to_value(status).unwrap(), name);
            assert_eq!(
                serde_json::from_value::<FaultStatus>(name.into()).unwrap(),
                status
            );
        }
    }
}
