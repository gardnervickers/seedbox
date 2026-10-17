use super::*;
use seedbox_test::process::Guarded;

type TarEntry<'a> = (
    &'a str,
    tar::EntryType,
    u32,
    u64,
    u64,
    &'a [u8],
    Option<&'a str>,
);

fn tar(entries: &[TarEntry<'_>]) -> Vec<u8> {
    let mut archive = tar::Builder::new(vec![]);
    for (name, kind, mode, uid, gid, data, link) in entries {
        let mut h = tar::Header::new_gnu();
        h.set_entry_type(*kind);
        h.set_mode(*mode);
        h.set_uid(*uid);
        h.set_gid(*gid);
        h.set_mtime(123);
        h.set_size(data.len() as u64);
        if let Some(link) = link {
            archive.append_link(&mut h, name, link).unwrap();
        } else {
            archive.append_data(&mut h, name, *data).unwrap();
        }
    }
    archive.into_inner().unwrap()
}
fn app(files: Vec<u8>) -> PreparedApplication {
    PreparedApplication {
        filesystem: FilesystemArchive::new(files).unwrap(),
        launch: Launch {
            argv: vec!["program".into(), "one two".into(), "".into()],
            environment: BTreeMap::from([
                ("PATH".into(), "/bin:/usr/bin".into()),
                ("VALUE".into(), "a=b\nc".into()),
            ]),
            working_directory: "/work".into(),
            user: "1234:2345".into(),
        },
    }
}
fn bootstrap() -> GuestBootstrap {
    GuestBootstrap {
        kernel: Kernel {
            image: vec![],
            command_line: String::new(),
        },
        init: b"static-init".to_vec(),
        base_initramfs: vec![],
        epoch_seconds: 0,
    }
}
#[derive(Debug)]
struct Cpio {
    name: Vec<u8>,
    fields: [u32; 13],
    data: Vec<u8>,
}
fn cpio(bytes: &[u8]) -> Vec<Cpio> {
    let mut out = vec![];
    let mut at = 0;
    while at < bytes.len() {
        assert_eq!(&bytes[at..at + 6], b"070701");
        let mut fields = [0; 13];
        for (i, value) in fields.iter_mut().enumerate() {
            *value = u32::from_str_radix(
                std::str::from_utf8(&bytes[at + 6 + i * 8..at + 14 + i * 8]).unwrap(),
                16,
            )
            .unwrap();
        }
        let name = bytes[at + 110..at + 110 + fields[11] as usize - 1].to_vec();
        at = (at + 110 + fields[11] as usize + 3) & !3;
        let data = bytes[at..at + fields[6] as usize].to_vec();
        at = (at + fields[6] as usize + 3) & !3;
        let done = name == b"TRAILER!!!";
        out.push(Cpio { name, fields, data });
        if done {
            break;
        }
    }
    out
}
#[test]
fn metadata_links_content_and_recipe_are_archive_native() {
    let input = tar(&[
        (
            "data",
            tar::EntryType::Directory,
            0o750,
            1234,
            2345,
            b"",
            None,
        ),
        (
            "data/owned",
            tar::EntryType::Regular,
            0o640,
            1234,
            2345,
            b"real contents",
            None,
        ),
        (
            "data/hard",
            tar::EntryType::Link,
            0,
            0,
            0,
            b"",
            Some("data/owned"),
        ),
        (
            "link",
            tar::EntryType::Symlink,
            0o777,
            1234,
            2345,
            b"",
            Some("/data/owned"),
        ),
    ]);
    let application = app(input);
    let bytes = boot_archive(&application, &bootstrap()).unwrap();
    let entries = cpio(&bytes);
    let find = |name: &[u8]| entries.iter().find(|e| e.name == name).unwrap();
    let a = find(b"rootfs/data/owned");
    let b = find(b"rootfs/data/hard");
    assert_eq!(a.fields[0], b.fields[0]);
    assert_eq!(a.fields[4], 2);
    assert_eq!(b.fields[1], 0o100640);
    assert_eq!((b.fields[2], b.fields[3]), (1234, 2345));
    assert_eq!(b.data, b"real contents");
    assert!(a.data.is_empty());
    assert_eq!(find(b"rootfs/link").data, b"/data/owned");
    assert_eq!(find(b"rootfs/data").fields[1], 0o40750);
    assert_eq!(find(b"rootfs/data/owned").fields[5], 123);
    assert_eq!(
        (
            find(b"dev/console").fields[9],
            find(b"dev/console").fields[10]
        ),
        (5, 1)
    );
    assert_eq!(
        find(b"seedbox/application.bin").data,
        recipe(&application.launch, &[]).unwrap()
    );
}
#[test]
fn recipe_preserves_argument_boundaries_environment_and_identity() {
    let application = app(tar(&[]));
    let bytes = recipe(&application.launch, &[]).unwrap();
    assert_eq!(&bytes[..8], RECIPE_MAGIC);
    fn number(bytes: &[u8], at: &mut usize) -> u32 {
        let n = u32::from_le_bytes(bytes[*at..*at + 4].try_into().unwrap());
        *at += 4;
        n
    }
    fn string(bytes: &[u8], at: &mut usize) -> String {
        let n = number(bytes, at) as usize;
        let text = String::from_utf8(bytes[*at..*at + n].to_vec()).unwrap();
        *at += n;
        text
    }
    fn strings(bytes: &[u8], at: &mut usize) -> Vec<String> {
        (0..number(bytes, at)).map(|_| string(bytes, at)).collect()
    }
    let mut at = 8;
    assert_eq!(strings(&bytes, &mut at), application.launch.argv);
    assert!(strings(&bytes, &mut at).is_empty());
    assert_eq!(
        strings(&bytes, &mut at),
        ["PATH=/bin:/usr/bin", "VALUE=a=b\nc"]
    );
    assert_eq!(string(&bytes, &mut at), "/work");
    assert_eq!(string(&bytes, &mut at), "1234:2345");
    assert_eq!(at, bytes.len());
}
#[test]
fn malformed_trees_and_links_are_rejected_before_guest_boot() {
    for entries in [
        vec![
            (
                "a",
                tar::EntryType::Symlink,
                0o777,
                0,
                0,
                &b""[..],
                Some("/outside"),
            ),
            (
                "a/file",
                tar::EntryType::Regular,
                0o644,
                0,
                0,
                &b"data"[..],
                None,
            ),
        ],
        vec![
            ("a", tar::EntryType::Link, 0, 0, 0, &b""[..], Some("b")),
            ("b", tar::EntryType::Link, 0, 0, 0, &b""[..], Some("a")),
        ],
        vec![(
            "a",
            tar::EntryType::Link,
            0,
            0,
            0,
            &b""[..],
            Some("missing"),
        )],
    ] {
        assert!(FilesystemArchive::new(tar(&entries)).is_err());
    }
    assert!(validate_filesystem(b"not a tar archive").is_err());
    assert!(path(b"../outside").is_err());
    assert!(path(b"/outside").is_err());
}
#[test]
fn backend_runtime_tree_and_wrapper_leave_original_reusable() {
    let original = app(tar(&[
        ("work", tar::EntryType::Directory, 0o755, 0, 0, b"", None),
        (
            "seedbox/progress/old",
            tar::EntryType::Regular,
            0o755,
            0,
            0,
            b"old",
            None,
        ),
        (
            "bin/program",
            tar::EntryType::Regular,
            0o755,
            0,
            0,
            b"app",
            None,
        ),
    ]));
    let before = original.clone();
    let runtime = FilesystemArchive::new(tar(&[
        (
            "seedbox/progress/bin/drrun",
            tar::EntryType::Regular,
            0o755,
            0,
            0,
            b"loader",
            None,
        ),
        (
            "seedbox/progress/lib/client.so",
            tar::EntryType::Regular,
            0o644,
            0,
            0,
            b"client",
            None,
        ),
    ]))
    .unwrap();
    let derived = with_runtime(
        &original,
        &runtime,
        &["/seedbox/progress/bin/drrun".into(), "--".into()],
    )
    .unwrap();
    assert_eq!(original, before);
    assert_eq!(
        &derived.execution_prefix[..2],
        ["/seedbox/progress/bin/drrun", "--"]
    );
    assert_eq!(derived.application.launch.argv, original.launch.argv);
    assert_eq!(
        derived.application.launch.environment,
        original.launch.environment
    );
    assert_eq!(derived.application.launch.user, original.launch.user);
    assert_eq!(
        derived.application.launch.working_directory,
        original.launch.working_directory
    );
    let nodes = filesystem(&derived.application.filesystem.bytes).unwrap();
    assert!(!nodes.contains_key(b"seedbox/progress/old".as_slice()));
    assert!(nodes.contains_key(b"seedbox/progress/lib/client.so".as_slice()));
    assert!(nodes.contains_key(b"bin/program".as_slice()));
}
#[test]
fn bad_launch_and_escaped_runtime_are_explicit_errors() {
    let mut application = app(tar(&[]));
    application.launch.argv.clear();
    assert!(boot_archive(&application, &bootstrap()).is_err());
    application.launch.argv = vec!["x\0y".into()];
    assert!(boot_archive(&application, &bootstrap()).is_err());
    application.launch.argv = vec!["x".into()];
    application
        .launch
        .environment
        .insert("BAD=NAME".into(), "x".into());
    assert!(boot_archive(&application, &bootstrap()).is_err());
    let runtime = FilesystemArchive::new(tar(&[(
        "bin/replace",
        tar::EntryType::Regular,
        0o755,
        0,
        0,
        b"bad",
        None,
    )]))
    .unwrap();
    assert!(with_runtime(&app(tar(&[])), &runtime, &[]).is_err());
}
#[test]
fn base_bootstrap_bytes_are_retained_without_extracting_them() {
    let mut bootstrap = bootstrap();
    bootstrap.base_initramfs = b"opaque backend archive".to_vec();
    let bytes = boot_archive(&app(tar(&[])), &bootstrap).unwrap();
    assert!(bytes.starts_with(&bootstrap.base_initramfs));
    let start = (bootstrap.base_initramfs.len() + 3) & !3;
    assert_eq!(cpio(&bytes[start..])[0].name, b"dev");
}

#[test]
fn runtime_derivation_preserves_long_paths_link_targets_and_inode_groups() {
    let name = format!("dir/{}/program", "x".repeat(120));
    let hard = format!("copies/{}", "h".repeat(140));
    let link = format!("links/{}", "s".repeat(130));
    let target = format!("/{name}");
    let original = app(tar(&[
        (
            &name,
            tar::EntryType::Regular,
            0o751,
            1234,
            2345,
            b"long content",
            None,
        ),
        (&hard, tar::EntryType::Link, 0, 0, 0, b"", Some(&name)),
        (
            &link,
            tar::EntryType::Symlink,
            0o777,
            1234,
            2345,
            b"",
            Some(&target),
        ),
    ]));
    let runtime = FilesystemArchive::new(tar(&[(
        "seedbox/progress/bin/drrun",
        tar::EntryType::Regular,
        0o755,
        0,
        0,
        b"runtime",
        None,
    )]))
    .unwrap();
    let derived = with_runtime(
        &original,
        &runtime,
        &["/seedbox/progress/bin/drrun".into(), "--".into()],
    )
    .unwrap();
    let entries = cpio(&boot_derived(&derived, &bootstrap()).unwrap());
    let find = |path: &str| {
        entries
            .iter()
            .find(|e| e.name == format!("rootfs/{path}").as_bytes())
            .unwrap()
    };
    assert_eq!(find(&name).fields[0], find(&hard).fields[0]);
    assert_eq!(find(&hard).data, b"long content");
    assert_eq!(find(&link).data, target.as_bytes());
    assert_eq!((find(&name).fields[2], find(&name).fields[3]), (1234, 2345));
}
#[test]
fn flattened_snapshot_rejects_duplicate_target_hardlink_overwrite() {
    let input = tar(&[
        ("target", tar::EntryType::Regular, 0o644, 0, 0, b"old", None),
        ("alias", tar::EntryType::Link, 0, 0, 0, b"", Some("target")),
        ("target", tar::EntryType::Regular, 0o644, 0, 0, b"new", None),
    ]);
    assert!(validate_filesystem(&input)
        .unwrap_err()
        .to_string()
        .contains("duplicate canonical"));
}

#[cfg(unix)]
#[test]
fn retained_recipe_decodes_and_linux_executes_path_script_wrapper_and_restart() {
    use std::{fs, process::Command};
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    let script = root.join("program");
    // Deliberately no shebang: preserve the existing /bin/sh fallback, along
    // with execvp-style PATH lookup and exact empty/spaced argument boundaries.
    seedbox_test::process::install_executable(&script,b"printf 'cwd=%s\\nfirst=<%s>\\nempty=<%s>\\nvalue=<%s>\\n' \"$PWD\" \"$1\" \"$2\" \"$VALUE\"\n", 0o755).unwrap();
    let launch = Launch {
        argv: vec!["program".into(), "one two".into(), "".into()],
        environment: BTreeMap::from([
            ("PATH".into(), root.to_str().unwrap().into()),
            ("VALUE".into(), "a=b\nc".into()),
        ]),
        working_directory: root.to_str().unwrap().into(),
        user: String::new(),
    };
    let recipe_path = root.join("recipe.bin");
    fs::write(&recipe_path, recipe(&launch, &[]).unwrap()).unwrap();
    let source = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/pv");
    let executable = root.join("recipe-test");
    let compiled = Command::new("cc")
        .args(["-std=c11", "-Wall", "-Wextra", "-Werror"])
        .arg("-I")
        .arg(&source)
        .arg(source.join("guest_application.c"))
        .arg(source.join("guest_supervisor.c"))
        .arg(source.join("guest_application_test.c"))
        .arg("-o")
        .arg(&executable)
        .guarded_output()
        .unwrap();
    assert!(
        compiled.status.success(),
        "{}",
        String::from_utf8_lossy(&compiled.stderr)
    );
    let inspected = Command::new(&executable)
        .arg(&recipe_path)
        .arg("--inspect")
        .guarded_output()
        .unwrap();
    assert!(inspected.status.success());
    let mut expected = String::new();
    for value in &launch.argv {
        expected.push_str(&format!("arg:{}:{value}\n", value.len()));
    }
    for (key, value) in &launch.environment {
        let value = format!("{key}={value}");
        expected.push_str(&format!("env:{}:{value}\n", value.len()));
    }
    expected.push_str(&format!("cwd:{}\nuser:\n", launch.working_directory));
    assert_eq!(inspected.stdout, expected.as_bytes());
    // The production interpreter resolver implements Linux ELF/shebang rules;
    // macOS validates the real C decoder, without pretending Mach-O is ELF.
    if cfg!(target_os = "linux") {
        let once = format!(
            "cwd={}\nfirst=<one two>\nempty=<>\nvalue=<a=b\nc>\n",
            root.display()
        );
        for prefix in [
            vec![],
            vec![
                executable.to_str().unwrap().into(),
                "--wrapper".into(),
                "--".into(),
            ],
        ] {
            fs::write(&recipe_path, recipe(&launch, &prefix).unwrap()).unwrap();
            let result = Command::new(&executable)
                .arg(&recipe_path)
                .guarded_output()
                .unwrap();
            assert!(
                result.status.success(),
                "status {:?}: {}",
                result.status,
                String::from_utf8_lossy(&result.stderr)
            );
            assert!(result.stderr.is_empty());
            assert_eq!(result.stdout, [once.as_bytes(), once.as_bytes()].concat());
        }
    }
    let mut bad = recipe(&launch, &[]).unwrap();
    bad.pop();
    fs::write(&recipe_path, bad).unwrap();
    assert!(!Command::new(executable)
        .arg(recipe_path)
        .arg("--inspect")
        .guarded_status()
        .unwrap()
        .success());
}

#[cfg(target_os = "linux")]
#[test]
fn nonroot_execute_only_and_directory_path_candidates_plain_and_wrapped() {
    use std::{fs, os::unix::fs::PermissionsExt, process::Command};
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    fs::set_permissions(root, fs::Permissions::from_mode(0o755)).unwrap();
    let source = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/pv");
    let executable = root.join("launcher");
    let compiled = Command::new("cc")
        .args(["-std=c11", "-Wall", "-Wextra", "-Werror", "-I"])
        .arg(&source)
        .arg(source.join("guest_application.c"))
        .arg(source.join("guest_launch_permissions_test.c"))
        .arg("-o")
        .arg(&executable)
        .guarded_output()
        .unwrap();
    assert!(
        compiled.status.success(),
        "{}",
        String::from_utf8_lossy(&compiled.stderr)
    );
    let first = root.join("first");
    let last = root.join("last");
    fs::create_dir(&first).unwrap();
    fs::create_dir(&last).unwrap();
    fs::create_dir(first.join("program")).unwrap();
    seedbox_test::process::install_executable(
        &last.join("program"),
        &fs::read(&executable).unwrap(),
        0o111,
    )
    .unwrap();
    let launch = Launch {
        argv: vec!["program".into(), "--child".into(), "permissions".into()],
        environment: BTreeMap::from([(
            "PATH".into(),
            format!("{}:{}", first.display(), last.display()),
        )]),
        working_directory: root.to_str().unwrap().into(),
        user: String::new(),
    };
    for prefix in [
        vec![],
        vec![executable.to_str().unwrap().into(), "--wrapper".into()],
    ] {
        let path = root.join("recipe");
        fs::write(&path, recipe(&launch, &prefix).unwrap()).unwrap();
        let result = Command::new(&executable)
            .arg(path)
            .guarded_output()
            .unwrap();
        assert!(
            result.status.success(),
            "{:?}: {}",
            result.status,
            String::from_utf8_lossy(&result.stderr)
        );
        let text = String::from_utf8(result.stdout).unwrap();
        assert!(text.ends_with(" arg=permissions\n"), "{text}");
        assert!(
            !text.starts_with("uid=0 "),
            "fixture must execute without root read bypass"
        );
    }
    fs::set_permissions(last.join("program"), fs::Permissions::from_mode(0o755)).unwrap();
}
