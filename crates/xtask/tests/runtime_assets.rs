mod support;
use std::{fs, io::Read, os::unix::fs::PermissionsExt, path::Path, process::Command};
use support::{json, python_api, root, success};

fn archive(path: &Path, component: &str, kind: &str) {
    let mut archive = tar::Builder::new(fs::File::create(path).unwrap());
    let data = format!("Synthetic {component} parser-test notice; no upstream code.\n");
    let mut header = tar::Header::new_gnu();
    header.set_mode(0o644);
    header.set_size(data.len() as u64);
    if kind == "symlink" {
        header.set_entry_type(tar::EntryType::Symlink);
        header.set_link_name("/outside").unwrap();
        header.set_size(0);
    }
    header.set_cksum();
    archive
        .append_data(
            &mut header,
            format!(
                "{component}-fixture/{}",
                if kind == "missing" {
                    "README"
                } else {
                    "COPYING"
                }
            ),
            if kind == "symlink" {
                &b""[..]
            } else {
                data.as_bytes()
            },
        )
        .unwrap();
    archive.finish().unwrap();
}

#[test]
fn upstream_notices_are_retained_without_extracting_missing_or_linked_members() {
    let directory = tempfile::tempdir().unwrap();
    for component in ["musl", "e2fsprogs"] {
        archive(
            &directory.path().join(format!("{component}.source")),
            component,
            "file",
        );
    }
    let text = json(&python_api(
        "tools/runtime/notices.py",
        "subject.notices(Path(sys.argv[2])).decode()",
        &[directory.path()],
    ));
    for component in ["musl", "e2fsprogs"] {
        assert!(text
            .as_str()
            .unwrap()
            .contains(&format!("Synthetic {component} parser-test notice")));
    }
    for kind in ["missing", "symlink"] {
        let rejected = tempfile::tempdir().unwrap();
        archive(&rejected.path().join("musl.source"), "musl", kind);
        let output = python_api(
            "tools/runtime/notices.py",
            "subject.notices(Path(sys.argv[2]), ('musl',)).decode()",
            &[rejected.path()],
        );
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr)
            .contains("missing or invalid actual upstream notice"));
        assert_eq!(fs::read_dir(rejected.path()).unwrap().count(), 1);
    }
}

#[test]
fn packaging_normalizes_export_metadata_and_accepts_read_only_inputs() {
    let directory = tempfile::tempdir().unwrap();
    let mut bytes = Vec::new();
    for (index, mode) in [0o600, 0o755, 0o444].into_iter().enumerate() {
        let assets = directory.path().join(index.to_string());
        fs::create_dir(&assets).unwrap();
        for name in ["NOTICE.txt", "compose-init", "mke2fs", "progress.tar"] {
            let path = assets.join(name);
            fs::write(&path, name).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(mode)).unwrap();
            let file = fs::File::open(&path).unwrap();
            file.set_times(fs::FileTimes::new().set_modified(
                std::time::UNIX_EPOCH + std::time::Duration::from_secs(1234 + index as u64 * 100),
            ))
            .unwrap();
        }
        fs::write(assets.join("runtime.json"), "obsolete manifest").unwrap();
        fs::write(assets.join("provenance.json"), "obsolete provenance").unwrap();
        fs::set_permissions(&assets, fs::Permissions::from_mode(0o555)).unwrap();
        let output = directory.path().join(format!("{index}.tar.gz"));
        success(
            Command::new("python3")
                .arg(root().join("tools/runtime/package.py"))
                .arg(&assets)
                .arg(&output),
        );
        assert_eq!(
            fs::metadata(&assets).unwrap().permissions().mode() & 0o777,
            0o555
        );
        fs::set_permissions(&assets, fs::Permissions::from_mode(0o700)).unwrap();
        bytes.push(fs::read(&output).unwrap());
        let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(
            fs::File::open(output).unwrap(),
        ));
        let mut names = Vec::new();
        for entry in archive.entries().unwrap() {
            let mut entry = entry.unwrap();
            let name = entry.path().unwrap().to_string_lossy().into_owned();
            assert_eq!(
                entry.header().mode().unwrap(),
                if ["compose-init", "mke2fs"].contains(&name.as_str()) {
                    0o755
                } else {
                    0o644
                }
            );
            assert_eq!(
                (
                    entry.header().uid().unwrap(),
                    entry.header().gid().unwrap(),
                    entry.header().mtime().unwrap()
                ),
                (0, 0, 0)
            );
            let mut data = Vec::new();
            entry.read_to_end(&mut data).unwrap();
            assert_eq!(data, name.as_bytes());
            names.push(name);
        }
        names.sort();
        assert_eq!(
            names,
            ["NOTICE.txt", "compose-init", "mke2fs", "progress.tar"]
        );
    }
    assert!(bytes.windows(2).all(|pair| pair[0] == pair[1]));
}

#[cfg(target_os = "macos")]
#[test]
fn linker_entitlement_is_given_only_to_selected_executables() {
    let directory = tempfile::tempdir().unwrap();
    let source = directory.path().join("main.c");
    fs::write(&source, "int main(void) { return 0; }\n").unwrap();
    for selected in [false, true] {
        let binary = directory
            .path()
            .join(if selected { "seedbox" } else { "dependency" });
        let mut command = Command::new("python3");
        command
            .arg(root().join("tools/runtime/linker.py"))
            .arg(&source)
            .arg("-o")
            .arg(&binary);
        if selected {
            command.arg("--seedbox-sign-hvf");
        }
        success(&mut command);
        let output = success(
            Command::new("/usr/bin/codesign")
                .args(["--display", "--entitlements", ":-"])
                .arg(binary),
        );
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(text.contains("com.apple.security.hypervisor"), selected);
    }
}
