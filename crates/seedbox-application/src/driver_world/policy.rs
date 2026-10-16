//! Explicit wire/native policy boundary. No native construction or host effects.
use seedbox_engine_protocol::driver_world as wire;
use seedbox_machine::definition::BackendPolicy;

pub(super) const PV_INCOMPLETE: &str = "Driver PV execution on this host is incomplete: only non-controller x86-64 Linux execution and checkpoint continuation are proven; keep the existing legacy service path for other PV paths";
pub(super) const CONTROLLER_INCOMPLETE: &str = "Driver controller execution is incomplete: source authorization and control latency must use the same resolved controller; keep the existing legacy service path until that wiring is proven";

pub(super) fn decode(value: &wire::BackendPolicy) -> Result<BackendPolicy, String> {
    let policy = BackendPolicy {
        seed: value.seed,
        timer_jitter_ns: value.timer_jitter_ns,
        stop_tolerance_ns: value.stop_tolerance_ns,
        torn_writes: value.torn_writes,
        full_state: value.full_state,
        seeded_pv_speed: value.seeded_pv_speed,
        boot_end_evidence: value.boot_end_evidence,
        controller: value.controller.clone(),
    };
    policy.validate().map_err(str::to_owned)?;
    Ok(policy)
}
pub(super) fn decode_optional(
    value: Option<&wire::BackendPolicy>,
) -> Result<Option<BackendPolicy>, String> {
    value.map(decode).transpose()
}
pub(super) fn encode(value: &BackendPolicy) -> Result<wire::BackendPolicy, String> {
    value.validate().map_err(str::to_owned)?;
    Ok(wire::BackendPolicy {
        seed: value.seed,
        timer_jitter_ns: value.timer_jitter_ns,
        stop_tolerance_ns: value.stop_tolerance_ns,
        torn_writes: value.torn_writes,
        full_state: value.full_state,
        seeded_pv_speed: value.seeded_pv_speed,
        boot_end_evidence: value.boot_end_evidence,
        controller: value.controller.clone(),
    })
}
/// Admit the proven native path without changing backend selection.
// TODO: Re-evaluate: ARM PV requires its own native proof. Guest control stays
// refused until service name/Driver authority agree.
pub(super) fn admit(inventory: &wire::Inventory) -> Result<(), String> {
    let policy = decode_optional(inventory.backend_policy())?;
    if policy
        .as_ref()
        .is_some_and(|policy| policy.controller.is_some())
    {
        return Err(CONTROLLER_INCOMPLETE.into());
    }
    if matches!(inventory, wire::Inventory::Pv { .. })
        && !cfg!(all(target_os = "linux", target_arch = "x86_64"))
    {
        return Err(PV_INCOMPLETE.into());
    }
    Ok(())
}
/// PV continuation is independently limited to non-controller worlds. A future
/// fresh-controller admission change must not silently broaden restore.
pub(super) fn admit_restore(inventory: &wire::Inventory) -> Result<(), String> {
    let policy = decode_optional(inventory.backend_policy())?;
    if matches!(inventory, wire::Inventory::Pv { .. })
        && policy
            .as_ref()
            .is_some_and(|policy| policy.controller.is_some())
    {
        return Err("Driver PV controller checkpoint continuation is incomplete; only non-controller x86-64 Linux restore is proven".into());
    }
    admit(inventory)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn conversion_is_lossless_and_optional_absence_is_not_explicit_default() {
        for policy in [
            BackendPolicy::default(),
            BackendPolicy {
                seed: u64::MAX,
                timer_jitter_ns: 3_600_000_000_000,
                stop_tolerance_ns: 91,
                torn_writes: true,
                full_state: true,
                seeded_pv_speed: true,
                boot_end_evidence: true,
                controller: Some("oracle".into()),
            },
        ] {
            let wire = encode(&policy).unwrap();
            assert_eq!(decode(&wire).unwrap(), policy);
            assert_eq!(
                serde_json::to_value(&wire).unwrap(),
                serde_json::to_value(&policy).unwrap()
            );
        }
        assert_eq!(decode_optional(None).unwrap(), None);
        assert_eq!(
            decode_optional(Some(&wire::BackendPolicy::default())).unwrap(),
            Some(BackendPolicy::default())
        );
    }
    #[test]
    fn conversions_reject_native_invalid_policy_in_both_directions() {
        for policy in [
            BackendPolicy {
                timer_jitter_ns: 3_600_000_000_001,
                ..Default::default()
            },
            BackendPolicy {
                stop_tolerance_ns: u64::MAX,
                ..Default::default()
            },
            BackendPolicy {
                controller: Some(String::new()),
                ..Default::default()
            },
        ] {
            assert!(encode(&policy).is_err());
            let wire: wire::BackendPolicy =
                serde_json::from_value(serde_json::to_value(policy).unwrap()).unwrap();
            assert!(decode(&wire).is_err());
            assert!(decode_optional(Some(&wire)).is_err());
        }
    }
}
