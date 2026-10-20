//! Explicit wire/native policy boundary. No native construction or host effects.
use seedbox_engine_protocol::driver_world as wire;
use seedbox_engine_protocol::driver_world::BackendPolicy;

pub(super) const PV_INCOMPLETE: &str = "Driver PV execution requires Linux x86-64 (KVM) or macOS aarch64 (HVF); run this simulation with the TCG backend on this host";
pub(super) const CONTROLLER_INCOMPLETE: &str =
    "Driver controller execution requires Linux x86-64 (KVM) or macOS aarch64 (HVF)";

/// Native machines accept only range-checked policy.
pub(super) fn checked(value: &BackendPolicy) -> Result<BackendPolicy, String> {
    value.validate().map_err(str::to_owned)?;
    Ok(value.clone())
}
pub(super) fn check_optional(
    value: Option<&BackendPolicy>,
) -> Result<Option<&BackendPolicy>, String> {
    value.map(checked).transpose()?;
    Ok(value)
}
/// One execution policy for both parent admission and native inventory.
/// Build binds the controller service name to Driver authority on both CPUs.
pub(super) fn admit_execution(
    backend: seedbox_spec::backend::CpuBackend,
    controller: bool,
) -> Result<(), String> {
    admit_on(super::platform::Host::current(), backend, controller)
}
/// [`admit_execution`] for an explicit host. PV runs only where the host has
/// a native leaf; a controller runs wherever that host runs Driver. TCG
/// without a controller keeps its host-independent admission.
// TODO: Re-evaluate: macOS aarch64 HVF admission is compiled and unit-tested
// only; Gardner verifies it on hardware (bd-877).
pub(super) fn admit_on(
    host: super::platform::Host,
    backend: seedbox_spec::backend::CpuBackend,
    controller: bool,
) -> Result<(), String> {
    if controller && host.guest(backend).is_none() {
        return Err(CONTROLLER_INCOMPLETE.into());
    }
    if backend == seedbox_spec::backend::CpuBackend::Pv && host.guest(backend).is_none() {
        return Err(PV_INCOMPLETE.into());
    }
    Ok(())
}
/// Admit the native inventory without changing backend selection.
pub(super) fn admit(inventory: &wire::Inventory) -> Result<(), String> {
    let policy = check_optional(inventory.backend_policy())?;
    let backend = match inventory {
        wire::Inventory::Pv { .. } => seedbox_spec::backend::CpuBackend::Pv,
        wire::Inventory::Tcg { .. } => seedbox_spec::backend::CpuBackend::Tcg,
    };
    admit_execution(
        backend,
        policy
            .as_ref()
            .is_some_and(|policy| policy.controller.is_some()),
    )
}
/// Keep PV controller continuation scoped independently of fresh admission.
/// build::restore must bind the saved Driver controller and latency before any
/// factory or native restoration. Both Driver hosts are admitted; only Linux
/// x86-64 continuation is exercised (macOS aarch64 is unverified, bd-877).
/// TCG controller continuation goes through the same binding and [`admit`].
pub(super) fn admit_restore(inventory: &wire::Inventory) -> Result<(), String> {
    let policy = check_optional(inventory.backend_policy())?;
    if matches!(inventory, wire::Inventory::Pv { .. })
        && policy
            .as_ref()
            .is_some_and(|policy| policy.controller.is_some())
        && super::platform::Host::current()
            .guest(seedbox_spec::backend::CpuBackend::Pv)
            .is_none()
    {
        return Err("Driver PV controller checkpoint continuation requires Linux x86-64 (KVM) or macOS aarch64 (HVF)".into());
    }
    admit(inventory)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn admission_derives_from_host_not_compile_target() {
        use super::super::platform::Host;
        use seedbox_spec::backend::CpuBackend;
        for (os, arch, driver) in [
            ("linux", "x86_64", true),
            ("macos", "aarch64", true),
            ("linux", "aarch64", false),
            ("macos", "x86_64", false),
        ] {
            let host = Host { os, arch };
            for controller in [false, true] {
                assert_eq!(
                    admit_on(host, CpuBackend::Pv, controller).is_ok(),
                    driver,
                    "{host:?} PV controller={controller}"
                );
                assert_eq!(
                    admit_on(host, CpuBackend::Tcg, controller).is_ok(),
                    driver || !controller,
                    "{host:?} TCG controller={controller}"
                );
            }
        }
        let refused = admit_on(
            Host {
                os: "linux",
                arch: "aarch64",
            },
            CpuBackend::Pv,
            false,
        )
        .unwrap_err();
        assert_eq!(refused, PV_INCOMPLETE);
    }
    #[test]
    fn checks_keep_policy_and_optional_absence_is_not_explicit_default() {
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
            assert_eq!(checked(&policy).unwrap(), policy);
            assert_eq!(check_optional(Some(&policy)).unwrap(), Some(&policy));
        }
        assert_eq!(check_optional(None).unwrap(), None);
    }
    #[test]
    fn checks_reject_native_invalid_policy() {
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
            assert!(checked(&policy).is_err());
            assert!(check_optional(Some(&policy)).is_err());
        }
    }
}
