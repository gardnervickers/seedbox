//! Opt-in run-wide backend semantics. Scheduling remains with the Driver.
use serde::{Deserialize, Serialize};

/// Configured backend behavior, retained and checked by backend snapshots.
///
/// An explicitly selected `Default` policy requests no timer randomness, extra
/// captures, speed spreading, boot-end outputs or controller socket. Absence of
/// this object is not an explicit default object. A service explicitly supplies
/// its resolved policy; factories select the controller socket by comparing
/// `controller` with the machine definition's name.
///
/// `seed` is authority for independent named policy streams, never guest
/// entropy or the Driver admission/network RNG. Timer domains are
/// `timer-jitter/<name>` for the initial boot, then
/// `timer-jitter/<name>/<boot>`; disk loss uses `disk-loss/<name>/<boot>`;
/// speed uses `pv-speed/<name>` and survives reboots. Backends retain stream
/// positions, policy and derived values rather than reseeding at restore.
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

impl BackendPolicy {
    /// Reject unsupported ranges before constructing a native machine.
    pub fn validate(&self) -> Result<(), &'static str> {
        const HOUR: u64 = 3_600_000_000_000;
        if self.timer_jitter_ns > HOUR {
            return Err("timer_jitter_ns exceeds one hour; reduce the configured timer jitter");
        }
        if self.stop_tolerance_ns > HOUR {
            return Err("stop_tolerance_ns exceeds one hour; reduce the configured stop tolerance");
        }
        if self.controller.as_ref().is_some_and(|name| name.is_empty()) {
            return Err("controller name is empty; supply the configured controller machine name");
        }
        Ok(())
    }
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
        assert!(serde_json::to_value(Option::<BackendPolicy>::None)
            .unwrap()
            .is_null());
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
    #[test]
    fn default_has_no_optional_semantics_or_policy_draws() {
        let p = BackendPolicy::default();
        assert_eq!(p.validate(), Ok(()));
        assert_eq!((p.seed, p.timer_jitter_ns, p.stop_tolerance_ns), (0, 0, 0));
        assert!(!p.torn_writes && !p.full_state && !p.seeded_pv_speed && !p.boot_end_evidence);
        assert!(p.controller.is_none());
    }
    #[test]
    fn validates_boundaries_and_actionable_errors() {
        let mut p = BackendPolicy {
            timer_jitter_ns: 3_600_000_000_000,
            stop_tolerance_ns: 3_600_000_000_000,
            ..Default::default()
        };
        assert!(p.validate().is_ok());
        p.timer_jitter_ns += 1;
        assert!(p.validate().unwrap_err().contains("timer_jitter_ns"));
        p.timer_jitter_ns = 0;
        p.stop_tolerance_ns += 1;
        assert!(p.validate().unwrap_err().contains("stop_tolerance_ns"));
        p.stop_tolerance_ns = 0;
        p.controller = Some(String::new());
        assert!(p.validate().unwrap_err().contains("controller"));
    }
}
