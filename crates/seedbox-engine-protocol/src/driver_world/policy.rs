//! Plain retained policy data; native semantic validation belongs to the application.
use serde::{Deserialize, Serialize};

/// Wire representation of selected backend semantics. Absence of this object
/// is not an explicit default object. The application converts every field and
/// validates native policy before materialization or restore.
// TODO: Re-evaluate: this transport boundary must not depend on a machine crate;
// explicit application conversion keeps wire schema and execution ownership separate.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BackendPolicy {
    pub seed: u64,
    pub timer_jitter_ns: u64,
    pub stop_tolerance_ns: u64,
    pub torn_writes: bool,
    pub full_state: bool,
    pub seeded_pv_speed: bool,
    pub boot_end_evidence: bool,
    pub controller: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn optional_policy_roundtrip_does_not_supply_defaults() {
        for policy in [
            None,
            Some(BackendPolicy::default()),
            Some(BackendPolicy {
                seed: 73,
                timer_jitter_ns: 127,
                stop_tolerance_ns: 91,
                torn_writes: true,
                full_state: true,
                seeded_pv_speed: true,
                boot_end_evidence: true,
                controller: Some("oracle".into()),
            }),
        ] {
            let encoded = serde_json::to_vec(&policy).unwrap();
            assert_eq!(
                serde_json::from_slice::<Option<BackendPolicy>>(&encoded).unwrap(),
                policy
            );
        }
        assert_ne!(
            serde_json::to_value(Option::<BackendPolicy>::None).unwrap(),
            serde_json::to_value(Some(BackendPolicy::default())).unwrap()
        );
    }
    #[test]
    fn policy_schema_rejects_unknown_missing_and_wrong_typed_fields() {
        let original = serde_json::to_value(BackendPolicy::default()).unwrap();
        let mut unknown = original.clone();
        unknown["future_policy"] = true.into();
        assert!(serde_json::from_value::<BackendPolicy>(unknown).is_err());
        let mut missing = original.clone();
        missing.as_object_mut().unwrap().remove("seed");
        assert!(serde_json::from_value::<BackendPolicy>(missing).is_err());
        let mut wrong = original;
        wrong["seed"] = (-1).into();
        assert!(serde_json::from_value::<BackendPolicy>(wrong).is_err());
    }
}
