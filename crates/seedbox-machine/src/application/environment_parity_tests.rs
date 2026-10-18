//! Optional setup must not change a single byte of unconfigured archives.
//! This private reference is the pre-environment package boundary, using the
//! unchanged filesystem/recipe/newc primitives. No guest binary is executed,
//! hashed/pinned or translated, and there is no public test inspection API.
use super::*;
fn previous_unconfigured_package(
    application: &PreparedApplication,
    bootstrap: &GuestBootstrap,
    prefix: &[String],
) -> Result<Vec<u8>, DefinitionError> {
    if bootstrap.init.is_empty() {
        return Err(invalid("guest bootstrap init is empty"));
    }
    let nodes = filesystem(&application.filesystem.bytes)?;
    let recipe = recipe(&application.launch, prefix)?;
    let mut archive = Newc {
        output: vec![],
        next: 0,
    };
    for name in [b"dev".as_slice(), b"proc", b"sys", b"seedbox"] {
        archive.owned(name, 0o40755, &[])?;
    }
    // Device identity is Linux console 5:1, not a normal empty file.
    let console_inode = archive.inode()?;
    archive.entry(
        b"dev/console",
        console_inode,
        &Node {
            mode: 0o20600,
            uid: 0,
            gid: 0,
            mtime: 0,
            kind: Kind::Character(5, 1),
        },
        1,
        &[],
    )?;
    archive.owned(b"init", 0o100755, &bootstrap.init)?;
    archive.owned(b"seedbox/application.bin", 0o100400, &recipe)?;
    // Realtime epoch for every boot; the shared PID 1 sets CLOCK_REALTIME
    // to epoch plus the machine counter from it before Ready.
    let mut clock = b"SBXCLK1\0".to_vec();
    clock.extend(bootstrap.epoch_seconds.to_le_bytes());
    archive.owned(b"seedbox/clock.bin", 0o100400, &clock)?;
    let mut groups: BTreeMap<Vec<u8>, (u32, u32)> = BTreeMap::new();
    for name in nodes.keys() {
        let (key, node) = resolve(&nodes, name)?;
        if matches!(node.kind, Kind::File(_)) {
            let group = groups.entry(key.to_vec()).or_insert((0, 0));
            group.1 = group
                .1
                .checked_add(1)
                .ok_or_else(|| invalid("hardlink count exceeds newc range"))?;
        }
    }
    let mut written = BTreeSet::new();
    for name in nodes.keys() {
        let (key, node) = resolve(&nodes, name)?;
        let mut path = b"rootfs".to_vec();
        if !name.is_empty() {
            path.push(b'/');
            path.extend(name);
        }
        let (inode, nlink, data) = match &node.kind {
            Kind::File(data) => {
                let group = groups.get_mut(key).unwrap();
                if group.0 == 0 {
                    group.0 = archive.inode()?;
                }
                (
                    group.0,
                    group.1,
                    if written.insert(key.to_vec()) {
                        data.as_slice()
                    } else {
                        &[]
                    },
                )
            }
            Kind::Symlink(data) => (archive.inode()?, 1, data.as_slice()),
            _ => (
                archive.inode()?,
                if matches!(node.kind, Kind::Directory) {
                    2
                } else {
                    1
                },
                &[][..],
            ),
        };
        archive.entry(&path, inode, node, nlink, data)?;
    }
    archive.owned(b"TRAILER!!!", 0, &[])?;
    let mut out = bootstrap.base_initramfs.clone();
    while !out.len().is_multiple_of(4) {
        out.push(0);
    }
    out.extend(archive.output);
    Ok(out)
}

#[test]
fn absent_environment_matches_previous_package_bytes_for_default_and_derived_inputs() {
    let mut tar = tar::Builder::new(vec![]);
    for (name, data) in [
        ("application", &[0, 255, 3][..]),
        ("seedbox/progress/runtime", &[255, 0, 9][..]),
    ] {
        let mut h = tar::Header::new_gnu();
        h.set_mode(0o755);
        h.set_uid(42);
        h.set_gid(43);
        h.set_mtime(1729);
        h.set_size(data.len() as u64);
        tar.append_data(&mut h, name, data).unwrap();
    }
    for (name, kind, mode) in [
        ("hard", tar::EntryType::Link, 0o755),
        ("sym", tar::EntryType::Symlink, 0o777),
    ] {
        let mut h = tar::Header::new_gnu();
        h.set_entry_type(kind);
        h.set_mode(mode);
        h.set_uid(42);
        h.set_gid(43);
        h.set_mtime(1729);
        h.set_size(0);
        h.set_link_name("application").unwrap();
        tar.append_data(&mut h, name, &[][..]).unwrap();
    }
    let app = PreparedApplication {
        filesystem: FilesystemArchive::new(tar.into_inner().unwrap()).unwrap(),
        launch: Launch {
            argv: vec!["/application".into(), "space argument".into()],
            environment: BTreeMap::from([
                ("A".into(), "x=y".into()),
                ("B".into(), "two words".into()),
            ]),
            working_directory: "/".into(),
            user: "42:43".into(),
        },
    };
    for base in [vec![], vec![91, 0, 92], vec![1, 2, 3, 4]] {
        let boot = GuestBootstrap {
            kernel: Kernel {
                image: vec![1],
                command_line: "explicit".into(),
            },
            init: vec![2],
            base_initramfs: base,
            epoch_seconds: 1729,
        };
        assert_eq!(
            boot_archive(&app, &boot).unwrap(),
            previous_unconfigured_package(&app, &boot, &[]).unwrap()
        );
        for prefix in [
            vec![],
            vec!["/seedbox/progress/bin64/drrun".into(), "-quiet".into()],
        ] {
            let derived = DerivedApplication {
                application: app.clone(),
                execution_prefix: prefix.clone(),
            };
            assert_eq!(
                boot_derived(&derived, &boot).unwrap(),
                previous_unconfigured_package(&app, &boot, &prefix).unwrap()
            );
        }
    }
}
