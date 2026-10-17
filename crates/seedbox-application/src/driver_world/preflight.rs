//! Parent-side admission reads bounded metadata, never native bodies or assets.
use crate::{Error, Result};
use seedbox_engine_protocol::driver_world as wire;
use std::{fs, path::Path};

pub(crate) fn restore_manifest(value: &serde_json::Value, controller: Option<&str>) -> Result<()> {
    let bytes = serde_json::to_vec(value)?;
    restore_bytes(&bytes, controller)
}
fn restore_bytes(bytes: &[u8], controller: Option<&str>) -> Result<()> {
    super::checkpoint::validate_metadata(bytes)?;
    let mut checkpoint: wire::Checkpoint = serde_json::from_slice(bytes)?;
    checkpoint.validate().map_err(Error::Integrity)?;
    // The native restore edge calls this same policy. Do not duplicate its
    // controller/host rules here: supported non-controller restores stay valid.
    super::policy::admit_restore(&checkpoint.inventory).map_err(Error::Setup)?;
    if let Some(controller) = controller {
        // An edited resolved definition can request control even when the saved
        // checkpoint had none. Admit that requested authority through the SAME
        // policy too, without rewriting the saved envelope or native state.
        let policy = match &mut checkpoint.inventory {
            wire::Inventory::Pv { policy, .. } | wire::Inventory::Tcg { policy, .. } => policy,
        };
        policy.get_or_insert_with(Default::default).controller = Some(controller.into());
        super::policy::admit_restore(&checkpoint.inventory).map_err(Error::Setup)?;
    }
    Ok(())
}
pub(super) fn restore_file(path: &Path, controller: Option<&str>) -> Result<()> {
    let bytes = crate::limits::read_bounded(
        fs::File::open(path)?,
        wire::MAX_ENVELOPE_BYTES,
        "Driver checkpoint metadata",
    )
    .map_err(|error| error.or_too_large(Error::Integrity))?;
    restore_bytes(&bytes, controller)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    pub(crate) fn manifest(policy: Option<wire::BackendPolicy>) -> serde_json::Value {
        let artifact = wire::Artifact {
            sha256: "00".repeat(32),
            length: 0,
            artifact: "asset:body".into(),
        };
        serde_json::to_value(wire::Checkpoint {
            contract: wire::CONTRACT.into(),
            version: wire::ENVELOPE_VERSION,
            inventory: wire::Inventory::Pv {
                policy,
                executable: artifact.clone(),
                kernel: artifact.clone(),
                init: artifact.clone(),
                command_line: artifact.clone(),
                progress: artifact.clone(),
                runtime: artifact.clone(),
                formatter: None,
                environments: Default::default(),
                boot_archives: Default::default(),
                blocks_per_progress_call: std::num::NonZeroU32::new(32).unwrap(),
                epoch_seconds: 86400,
            },
            driver: artifact.clone(),
            machines: vec![artifact],
            state: wire::State {
                world: wire::World {
                    seed: 123,
                    machine_names: vec!["oracle".into()],
                    input_program: Default::default(),
                    intervention_next: 0,
                    readiness: wire::Readiness {
                        initial: vec!["oracle".into()],
                        seen: vec!["oracle".into()],
                        latched: true,
                    },
                    publication: Default::default(),
                    position: seedbox_engine_protocol::host_control::BoundaryPosition {
                        boundary_index: 1,
                        completed_rounds: 1,
                        simulated_time_ns: 1_000_000,
                    },
                },
            },
        })
        .unwrap()
    }
    #[test]
    fn tcg_parent_restore_uses_the_native_controller_admission_scope() {
        for controller in [None, Some("oracle")] {
            let mut checkpoint: wire::Checkpoint = serde_json::from_value(manifest(None)).unwrap();
            let wire::Inventory::Pv {
                executable,
                kernel,
                init,
                command_line,
                runtime,
                formatter,
                environments,
                boot_archives,
                epoch_seconds,
                ..
            } = checkpoint.inventory
            else {
                unreachable!()
            };
            checkpoint.inventory = wire::Inventory::Tcg {
                policy: Some(wire::BackendPolicy {
                    controller: controller.map(str::to_owned),
                    ..Default::default()
                }),
                executable,
                firmware: kernel.clone(),
                kernel,
                init,
                command_line,
                runtime,
                formatter,
                environments,
                boot_archives,
                epoch_seconds,
            };
            let value = serde_json::to_value(&checkpoint).unwrap();
            assert_eq!(
                crate::driver_world::policy::admit(&checkpoint.inventory).is_ok(),
                controller.is_none() || cfg!(all(target_os = "linux", target_arch = "x86_64"))
            );
            assert_eq!(
                crate::driver_world::policy::admit_restore(&checkpoint.inventory).is_ok(),
                controller.is_none() || cfg!(all(target_os = "linux", target_arch = "x86_64"))
            );
            assert_eq!(
                restore_manifest(&value, None).is_ok(),
                controller.is_none() || cfg!(all(target_os = "linux", target_arch = "x86_64"))
            );
            assert_eq!(
                restore_manifest(&value, Some("oracle")).is_ok(),
                cfg!(all(target_os = "linux", target_arch = "x86_64"))
            );
        }
    }
    #[test]
    fn parent_restore_uses_native_policy_without_reading_any_body() {
        for selected in [
            None,
            Some(wire::BackendPolicy::default()),
            Some(wire::BackendPolicy {
                controller: Some("oracle".into()),
                ..Default::default()
            }),
        ] {
            for requested in [None, Some("oracle")] {
                let value = manifest(selected.clone());
                let mut checkpoint: wire::Checkpoint =
                    serde_json::from_value(value.clone()).unwrap();
                let mut native = crate::driver_world::policy::admit_restore(&checkpoint.inventory);
                if native.is_ok() {
                    if let Some(controller) = requested {
                        let wire::Inventory::Pv { policy, .. } = &mut checkpoint.inventory else {
                            unreachable!()
                        };
                        policy.get_or_insert_with(Default::default).controller =
                            Some(controller.into());
                        native = crate::driver_world::policy::admit_restore(&checkpoint.inventory);
                    }
                }
                let parent = restore_manifest(&value, requested);
                match native {
                    Ok(()) => parent.unwrap(),
                    Err(expected) => assert!(parent.unwrap_err().to_string().contains(&expected)),
                }
                // asset:body has not been opened, resolved, or materialized.
            }
        }
    }

    #[test]
    fn oversized_metadata_is_bounded_before_parsing() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("checkpoint.json");
        fs::File::create(&path)
            .unwrap()
            .set_len(wire::MAX_ENVELOPE_BYTES + 1)
            .unwrap();
        assert!(restore_file(&path, None)
            .unwrap_err()
            .to_string()
            .contains("Driver checkpoint metadata"));
        assert_eq!(fs::read_dir(root.path()).unwrap().count(), 1);
    }
}
