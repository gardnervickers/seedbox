//! Shared legacy named-volume setup for one prepared machine.
//! This chooses initial capacity and guest mount metadata; it allocates no disk
//! and performs no filesystem, formatter, worker or hypervisor operations.
use seedbox_engine_protocol::driver_world::NamedVolume;
use seedbox_spec::VolumeMount;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VolumeSetup {
    /// One initially zero-filled disk, only when named volumes are mounted.
    pub disk_bytes: Option<u64>,
    pub named_volumes: Vec<NamedVolume>,
}

/// Pass the named mounts of every service placed on this machine, in retained
/// order. Multiple mounts share the legacy single 1 GiB disk; they do not add
/// capacities. Anonymous OCI volumes are ordinary guest directories and must
/// not be passed as named mounts or cause a persistent disk to be allocated.
pub fn volume_setup<'a>(mounts: impl IntoIterator<Item = &'a VolumeMount>) -> VolumeSetup {
    let named_volumes: Vec<_> = mounts
        .into_iter()
        .map(|mount| NamedVolume {
            name: mount.source.clone(),
            target: mount.target.clone(),
        })
        .collect();
    VolumeSetup {
        disk_bytes: (!named_volumes.is_empty())
            .then_some(crate::engine_adapter::VOLUME_DISK_MIB * 1024 * 1024),
        named_volumes,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_named_mounts_means_no_persistent_disk() {
        assert_eq!(
            volume_setup(std::iter::empty()),
            VolumeSetup {
                disk_bytes: None,
                named_volumes: vec![]
            }
        );
    }

    #[test]
    fn one_named_mount_keeps_the_legacy_zeroed_disk_capacity() {
        let mounts = [VolumeMount {
            source: "data".into(),
            target: "/data".into(),
        }];
        let setup = volume_setup(&mounts);
        assert_eq!(setup.disk_bytes, Some(1_073_741_824));
        assert_eq!(
            setup.named_volumes,
            vec![NamedVolume {
                name: "data".into(),
                target: "/data".into()
            }]
        );
    }

    #[test]
    fn all_placed_services_share_one_disk_and_keep_mount_names_targets_and_order() {
        let first = [VolumeMount {
            source: "logs".into(),
            target: "/var/log/app".into(),
        }];
        let second = [VolumeMount {
            source: "data".into(),
            target: "/data".into(),
        }];
        let services = [&first[..], &second[..]];
        let setup = volume_setup(services.into_iter().flatten());
        assert_eq!(setup.disk_bytes, Some(1_073_741_824));
        assert_eq!(
            setup.named_volumes,
            vec![
                NamedVolume {
                    name: "logs".into(),
                    target: "/var/log/app".into()
                },
                NamedVolume {
                    name: "data".into(),
                    target: "/data".into()
                },
            ]
        );
    }
}
