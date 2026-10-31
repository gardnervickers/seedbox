//! Named-volume setup for one prepared machine.
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
/// order. Multiple mounts share one disk: unsized mounts share one 1 GiB
/// allotment and each declared size adds to it.
/// Anonymous OCI volumes are ordinary guest directories and must not be
/// passed as named mounts or cause a persistent disk to be allocated.
pub fn volume_setup<'a>(
    mounts: impl IntoIterator<Item = &'a VolumeMount>,
) -> Result<VolumeSetup, String> {
    let mounts: Vec<_> = mounts.into_iter().collect();
    let named_volumes: Vec<_> = mounts
        .iter()
        .map(|mount| NamedVolume {
            name: mount.source.clone(),
            target: mount.target.clone(),
        })
        .collect();
    let disk_bytes =
        seedbox_spec::project::machine_disk_bytes(mounts.iter().map(|m| m.size_bytes))?;
    Ok(VolumeSetup {
        disk_bytes: (!named_volumes.is_empty()).then_some(disk_bytes),
        named_volumes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_named_mounts_means_no_persistent_disk() {
        assert_eq!(
            volume_setup(std::iter::empty()).unwrap(),
            VolumeSetup {
                disk_bytes: None,
                named_volumes: vec![]
            }
        );
    }

    #[test]
    fn one_unsized_named_mount_gets_the_default_disk_capacity() {
        let mounts = [VolumeMount {
            source: "data".into(),
            target: "/data".into(),
            size_bytes: None,
        }];
        let setup = volume_setup(&mounts).unwrap();
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
    fn all_placed_services_share_one_disk_of_their_summed_sizes_in_mount_order() {
        let first = [VolumeMount {
            source: "logs".into(),
            target: "/var/log/app".into(),
            size_bytes: Some(64 << 20),
        }];
        let second = [VolumeMount {
            source: "data".into(),
            target: "/data".into(),
            size_bytes: None,
        }];
        let services = [&first[..], &second[..]];
        let setup = volume_setup(services.into_iter().flatten()).unwrap();
        assert_eq!(setup.disk_bytes, Some((1 << 30) + (64 << 20)));
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

    #[test]
    fn unsized_mounts_share_one_default_disk() {
        let mounts = ["a", "b"].map(|name| VolumeMount {
            source: name.into(),
            target: format!("/{name}"),
            size_bytes: None,
        });
        assert_eq!(volume_setup(&mounts).unwrap().disk_bytes, Some(1 << 30));
    }

    #[test]
    fn a_machine_disk_above_the_limit_is_rejected() {
        let mounts = [32u64, 33].map(|gib| VolumeMount {
            source: format!("v{gib}"),
            target: format!("/v{gib}"),
            size_bytes: Some(gib << 30),
        });
        let error = volume_setup(&mounts).unwrap_err();
        assert!(error.contains("above the 64GiB limit"), "{error}");
    }
}
