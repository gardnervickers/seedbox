//! One service authority for native socket exposure and Driver control queues.
use crate::{Error, Result};
use seedbox_driver::driver::{ControllerSetup, MachineId, NetworkSetup};
use seedbox_engine_protocol::driver_world::Inventory;
use std::time::Duration;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Resolved {
    controller: Option<MachineId>,
    latency: Duration,
}
impl Resolved {
    pub(super) fn configure(self, network: NetworkSetup) -> Result<NetworkSetup> {
        network
            .with_controller(self.controller, self.latency)
            .map_err(Error::Integrity)
    }
    pub(super) fn validate_saved(self, saved: Option<ControllerSetup>) -> Result<()> {
        if saved.is_none_or(|saved| {
            saved.controller() != self.controller || saved.latency() != self.latency
        }) {
            return Err(Error::Integrity(
                "Driver checkpoint controller authority or control latency differs from resolved service controller".into(),
            ));
        }
        Ok(())
    }
}

pub(super) fn resolve(
    inventory: &Inventory,
    oracle: &str,
    network_latency_ns: u64,
) -> Result<Resolved> {
    let policy =
        super::policy::decode_optional(inventory.backend_policy()).map_err(Error::Integrity)?;
    let controller = policy
        .as_ref()
        .and_then(|policy| policy.controller.as_deref());
    // Driver assigns ORACLE to the dedicated oracle, not an arbitrary retained
    // machine whose name happens to match a controller string.
    if controller.is_some_and(|name| name != oracle) {
        return Err(Error::Integrity(
            "Driver controller must name the retained oracle, not another retained machine".into(),
        ));
    }
    let stop_tolerance = policy.as_ref().map_or(0, |policy| policy.stop_tolerance_ns);
    let latency = network_latency_ns
        .checked_mul(2)
        .and_then(|network| {
            stop_tolerance
                .checked_mul(2)
                .and_then(|stop| network.checked_add(stop))
        })
        .and_then(|delay| delay.checked_add(1_000_000))
        .ok_or_else(|| Error::Integrity("Driver service control latency overflow".into()))?;
    Ok(Resolved {
        controller: controller.map(|_| MachineId::ORACLE),
        latency: Duration::from_nanos(latency),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use seedbox_driver::driver::NetworkConfig;
    use seedbox_engine_protocol::driver_world::{Artifact, BackendPolicy};

    fn inventory(controller: Option<&str>) -> Inventory {
        let artifact = Artifact {
            sha256: "a".repeat(64),
            length: 1,
            artifact: "unused".into(),
        };
        Inventory::Pv {
            policy: Some(BackendPolicy {
                controller: controller.map(str::to_owned),
                stop_tolerance_ns: 91,
                ..Default::default()
            }),
            executable: artifact.clone(),
            kernel: artifact.clone(),
            init: artifact.clone(),
            command_line: artifact.clone(),
            progress: artifact.clone(),
            runtime: artifact,
            formatter: None,
            environments: Default::default(),
            boot_archives: Default::default(),
            blocks_per_progress_call: std::num::NonZeroU32::new(1).unwrap(),
            epoch_seconds: 0,
        }
    }
    #[test]
    fn named_oracle_and_disabled_mode_use_exact_checked_service_latency() {
        let enabled = resolve(&inventory(Some("named-oracle")), "named-oracle", 127).unwrap();
        assert_eq!(enabled.controller, Some(MachineId::ORACLE));
        assert_eq!(enabled.latency.as_nanos(), 1_000_436);
        let disabled = resolve(&inventory(None), "named-oracle", 127).unwrap();
        assert_eq!(disabled.controller, None);
        assert_eq!(disabled.latency, enabled.latency);
        // The SDK's omitted policy is never the service's disabled mode.
        let network = NetworkSetup::from(
            NetworkConfig::new(Duration::from_nanos(127), Duration::ZERO, 0).unwrap(),
        );
        assert!(disabled.configure(network).is_ok());
        assert!(resolve(&inventory(Some("worker")), "named-oracle", 127).is_err());
        assert!(resolve(&inventory(None), "named-oracle", u64::MAX).is_err());
    }
    #[test]
    fn checkpoint_binding_rejects_omission_before_native_restore() {
        let authority = resolve(&inventory(Some("named-oracle")), "named-oracle", 127).unwrap();
        assert!(authority.validate_saved(None).is_err());
    }
}
