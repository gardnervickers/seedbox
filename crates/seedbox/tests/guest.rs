use seedbox::{
    guest::{build_initramfs, launch_recipe},
    image::Launch,
};
use std::{
    collections::BTreeMap,
    fs,
    os::unix::fs::{symlink, MetadataExt, PermissionsExt},
    process::{Command, Stdio},
};

#[test]
fn large_image_boot_floor_accounts_for_resident_archive_and_root_tmpfs() {
    assert_eq!(
        seedbox::guest::minimum_memory_mb(176 * 1024 * 1024).unwrap(),
        656
    );
    assert!(seedbox::guest::minimum_memory_mb(200 * 1024 * 1024).unwrap() > 512);
    assert!(seedbox::guest::minimum_memory_mb(u64::MAX).is_err());
}

#[test]
fn initramfs_round_trip_preserves_image_files_modes_and_hardlinks() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("image");
    fs::create_dir(&root).unwrap();
    fs::write(root.join("file"), b"image data").unwrap();
    fs::set_permissions(root.join("file"), fs::Permissions::from_mode(0o751)).unwrap();
    fs::hard_link(root.join("file"), root.join("hardlink")).unwrap();
    fs::hard_link(root.join("file"), root.join("third-link")).unwrap();
    symlink("file", root.join("symlink")).unwrap();
    // macOS may attach provenance attributes to newly created fixture files.
    // The archive fixture represents an OCI rootfs without extended metadata.
    for entry in walkdir::WalkDir::new(&root) {
        let entry = entry.unwrap();
        for name in xattr::list(entry.path()).unwrap() {
            xattr::remove(entry.path(), name).unwrap();
        }
    }
    let init = directory.path().join("init");
    fs::write(&init, b"helper").unwrap();
    let archive = directory.path().join("guest.cpio");
    build_initramfs(&root, &init, &init, b"recipe", &archive).unwrap();
    let archive2 = directory.path().join("repeat.cpio");
    build_initramfs(&root, &init, &init, b"recipe", &archive2).unwrap();
    assert_eq!(fs::read(&archive).unwrap(), fs::read(&archive2).unwrap());
    let output = directory.path().join("unpacked");
    fs::create_dir(&output).unwrap();
    let status = Command::new("cpio")
        .args(["-idm", "--no-absolute-filenames", "--no-preserve-owner"])
        .current_dir(&output)
        .stdin(fs::File::open(archive).unwrap())
        .stderr(Stdio::piped())
        .output()
        .expect("cpio from nix develop");
    assert!(
        status.status.success(),
        "{}",
        String::from_utf8_lossy(&status.stderr)
    );
    for path in ["file", "hardlink", "third-link", "symlink"] {
        assert_eq!(
            fs::read(output.join("rootfs").join(path)).unwrap(),
            b"image data"
        );
    }
    let file = fs::metadata(output.join("rootfs/file")).unwrap();
    assert_eq!(file.mode() & 0o777, 0o751);
    assert_eq!(
        file.ino(),
        fs::metadata(output.join("rootfs/third-link"))
            .unwrap()
            .ino()
    );
    assert_eq!(
        fs::read_link(output.join("rootfs/symlink"))
            .unwrap()
            .to_str(),
        Some("file")
    );
    assert_eq!(
        fs::read(output.join("seedbox/launch.bin")).unwrap(),
        b"recipe"
    );
}

#[test]
fn unsupported_image_capabilities_are_not_silently_dropped() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("image");
    fs::create_dir(&root).unwrap();
    let file = root.join("file");
    fs::write(&file, b"test").unwrap();
    xattr::set(&file, "user.seedbox-test", b"metadata").unwrap();
    let init = directory.path().join("init");
    fs::write(&init, b"helper").unwrap();
    let error = build_initramfs(
        &root,
        &init,
        &init,
        b"recipe",
        &directory.path().join("out"),
    )
    .unwrap_err();
    assert!(error.to_string().contains("extended attributes"));
}

#[test]
fn binary_recipe_keeps_arguments_and_config_bytes_literal() {
    let launch = Launch {
        argv: vec!["/bin/app".into(), "$(false)\nquoted '".into()],
        environment: BTreeMap::from([("NAME".into(), "a=b\nc".into())]),
        working_dir: "/".into(),
        user: "123:456".into(),
        volumes: vec!["/data".into()],
        disk_volumes: vec![seedbox::config::VolumeMount {
            source: "state".into(),
            target: "/var/lib/state".into(),
        }],
    };
    let recipe = launch_recipe(
        &launch,
        "10.0.0.1/24",
        "10.0.0.1 app\n",
        "app",
        &BTreeMap::from([("/config".into(), vec![0, 255, 10])]),
        1791500000,
    )
    .unwrap();
    let mut cursor = 10;
    fn count(bytes: &[u8], cursor: &mut usize) -> usize {
        let n = u32::from_le_bytes(bytes[*cursor..*cursor + 4].try_into().unwrap()) as usize;
        *cursor += 4;
        n
    }
    fn field<'a>(bytes: &'a [u8], cursor: &mut usize) -> &'a [u8] {
        let n = count(bytes, cursor);
        let data = &bytes[*cursor..*cursor + n];
        *cursor += n;
        data
    }
    assert_eq!(&recipe[..10], b"SBXLAUNCH4");
    for expected in ["10.0.0.1/24", "10.0.0.1 app\n", "app", "123:456", "/"] {
        assert_eq!(field(&recipe, &mut cursor), expected.as_bytes());
    }
    assert_eq!(count(&recipe, &mut cursor), 262144);
    assert_eq!(count(&recipe, &mut cursor), 1791500000);
    assert_eq!(count(&recipe, &mut cursor), 2);
    for expected in &launch.argv {
        assert_eq!(field(&recipe, &mut cursor), expected.as_bytes());
    }
    assert_eq!(count(&recipe, &mut cursor), 1);
    assert_eq!(field(&recipe, &mut cursor), b"NAME=a=b\nc");
    assert_eq!(count(&recipe, &mut cursor), 1);
    assert_eq!(field(&recipe, &mut cursor), b"/data");
    // Named volumes as name, target pairs, read before the image root.
    assert_eq!(count(&recipe, &mut cursor), 2);
    assert_eq!(field(&recipe, &mut cursor), b"state");
    assert_eq!(field(&recipe, &mut cursor), b"/var/lib/state");
    assert_eq!(count(&recipe, &mut cursor), 1);
    assert_eq!(field(&recipe, &mut cursor), b"/config");
    assert_eq!(field(&recipe, &mut cursor), [0, 255, 10]);
    assert_eq!(cursor, recipe.len());
}

#[test]
fn linux_tar_preserves_ownership_modes_hardlinks_and_symlinks_without_host_extraction() {
    let temporary = tempfile::tempdir().unwrap();
    let input = temporary.path().join("linux.tar");
    let mut tar = tar::Builder::new(fs::File::create(&input).unwrap());
    let mut entry = |name: &str, kind, link: Option<&str>, bytes: &[u8]| {
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(kind);
        header.set_uid(1234);
        header.set_gid(5678);
        header.set_mode(0o751);
        header.set_mtime(1234567);
        header.set_size(bytes.len() as u64);
        if let Some(link) = link {
            header.set_link_name(link).unwrap();
        }
        header.set_cksum();
        tar.append_data(&mut header, name, bytes).unwrap();
    };
    entry("rootfs", tar::EntryType::Directory, None, b"");
    entry("rootfs/file", tar::EntryType::Regular, None, b"payload");
    entry(
        "rootfs/hardlink",
        tar::EntryType::Link,
        Some("rootfs/file"),
        b"",
    );
    entry("rootfs/symlink", tar::EntryType::Symlink, Some("file"), b"");
    tar.finish().unwrap();
    let helper = temporary.path().join("helper");
    fs::write(&helper, b"helper").unwrap();
    let output = temporary.path().join("guest.cpio");
    seedbox::guest::build_initramfs_from_tar(&input, &helper, &helper, None, b"recipe", &output)
        .unwrap();
    let bytes = fs::read(&output).unwrap();
    let mut at = 0;
    let mut entries = std::collections::BTreeMap::new();
    while at < bytes.len() {
        assert_eq!(&bytes[at..at + 6], b"070701");
        let number = |field: usize| {
            u32::from_str_radix(
                std::str::from_utf8(&bytes[at + 6 + field * 8..at + 14 + field * 8]).unwrap(),
                16,
            )
            .unwrap()
        };
        let inode = number(0);
        let mode = number(1);
        let uid = number(2);
        let gid = number(3);
        let links = number(4);
        let mtime = number(5);
        let size = number(6) as usize;
        let length = number(11) as usize;
        let name = std::str::from_utf8(&bytes[at + 110..at + 110 + length - 1])
            .unwrap()
            .to_owned();
        at = (at + 110 + length + 3) & !3;
        let data = bytes[at..at + size].to_vec();
        entries.insert(name, (inode, mode, uid, gid, links, mtime, data));
        at = (at + size + 3) & !3;
    }
    let file = &entries["rootfs/file"];
    assert_eq!(
        (file.1, file.2, file.3, file.4, file.5),
        (0o100751, 1234, 5678, 2, 1234567)
    );
    assert_eq!(file.6, b"payload");
    assert_eq!(file.0, entries["rootfs/hardlink"].0);
    assert_eq!(entries["rootfs/symlink"].6, b"file");
}

#[test]
fn linux_tar_cannot_replace_seedbox_bootstrap_or_reference_missing_hardlinks() {
    let temporary = tempfile::tempdir().unwrap();
    for (name, link) in [("init", None), ("rootfs/link", Some("rootfs/absent"))] {
        let input = temporary.path().join("bad.tar");
        let mut archive = tar::Builder::new(fs::File::create(&input).unwrap());
        let mut header = tar::Header::new_gnu();
        header.set_mode(0o755);
        header.set_size(0);
        header.set_mtime(0);
        if let Some(link) = link {
            header.set_entry_type(tar::EntryType::Link);
            header.set_link_name(link).unwrap();
        }
        header.set_cksum();
        archive.append_data(&mut header, name, &[][..]).unwrap();
        archive.finish().unwrap();
        let helper = temporary.path().join("helper");
        fs::write(&helper, b"helper").unwrap();
        let error = seedbox::guest::build_initramfs_from_tar(
            &input,
            &helper,
            &helper,
            None,
            b"",
            &temporary.path().join("bad.cpio"),
        )
        .unwrap_err();
        assert!(error
            .to_string()
            .contains("invalid Linux root filesystem archive"));
    }
}
