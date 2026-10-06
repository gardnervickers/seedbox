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
        .expect("cpio from nix develop ./sim");
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
    };
    let recipe = launch_recipe(
        &launch,
        "10.0.0.1/24",
        "10.0.0.1 app\n",
        &BTreeMap::from([("/config".into(), vec![0, 255, 10])]),
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
    assert_eq!(&recipe[..10], b"SBXLAUNCH1");
    for expected in ["10.0.0.1/24", "10.0.0.1 app\n", "123:456", "/"] {
        assert_eq!(field(&recipe, &mut cursor), expected.as_bytes());
    }
    assert_eq!(count(&recipe, &mut cursor), 2);
    for expected in &launch.argv {
        assert_eq!(field(&recipe, &mut cursor), expected.as_bytes());
    }
    assert_eq!(count(&recipe, &mut cursor), 1);
    assert_eq!(field(&recipe, &mut cursor), b"NAME=a=b\nc");
    assert_eq!(count(&recipe, &mut cursor), 1);
    assert_eq!(field(&recipe, &mut cursor), b"/data");
    assert_eq!(count(&recipe, &mut cursor), 1);
    assert_eq!(field(&recipe, &mut cursor), b"/config");
    assert_eq!(field(&recipe, &mut cursor), [0, 255, 10]);
    assert_eq!(cursor, recipe.len());
}
