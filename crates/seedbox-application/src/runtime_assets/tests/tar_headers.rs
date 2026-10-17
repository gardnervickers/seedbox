//! Host-only adversarial tar framing and genuine GNU/BSD writer fixtures.
use super::*;
use seedbox_test::process::Guarded;

fn header(kind: tar::EntryType, name: &str, size: u64) -> tar::Header {
    let mut header = tar::Header::new_gnu();
    header.set_path(name).unwrap();
    header.set_entry_type(kind);
    header.set_mode(0o644);
    header.set_size(size);
    header.set_cksum();
    header
}
fn frame(output: &mut impl Write, kind: tar::EntryType, name: &str, bytes: &[u8]) {
    output
        .write_all(header(kind, name, bytes.len() as u64).as_bytes())
        .unwrap();
    output.write_all(bytes).unwrap();
    let padding = (512 - bytes.len() % 512) % 512;
    output.write_all(&[0; 512][..padding]).unwrap();
}
fn gzip() -> flate2::write::GzEncoder<Vec<u8>> {
    flate2::write::GzEncoder::new(vec![], flate2::Compression::fast())
}
fn pax(key: &str, value: &[u8]) -> Vec<u8> {
    let mut length = key.len() + value.len() + 4;
    loop {
        let actual = length.to_string().len() + key.len() + value.len() + 3;
        if actual == length {
            break;
        }
        length = actual;
    }
    let mut bytes = format!("{length} {key}=").into_bytes();
    bytes.extend(value);
    bytes.push(b'\n');
    bytes
}
fn error(bytes: &[u8]) -> String {
    let scratch = tempfile::tempdir().unwrap();
    let error = Runtime::from_bytes(bytes, scratch.path())
        .unwrap_err()
        .to_string();
    assert_eq!(
        fs::read_dir(scratch.path()).unwrap().count(),
        0,
        "failed extraction leaked files"
    );
    error
}

#[test]
fn oversized_extension_headers_fail_before_any_body_is_available() {
    for kind in [
        tar::EntryType::GNULongName,
        tar::EntryType::GNULongLink,
        tar::EntryType::XHeader,
        tar::EntryType::XGlobalHeader,
    ] {
        let mut output = gzip();
        output
            .write_all(header(kind, "extension", archive::EXTENSION_LIMIT + 1).as_bytes())
            .unwrap();
        let bytes = output.finish().unwrap();
        assert!(
            bytes.len() < 512,
            "fixture is header-only, not a giant metadata payload"
        );
        assert!(
            error(&bytes).contains("extension exceeds 64 KiB"),
            "{kind:?}"
        );
    }
}
#[test]
fn raw_sparse_header_is_rejected_without_reading_its_sparse_extension_blocks() {
    let mut sparse = header(tar::EntryType::GNUSparse, "host-system", 0);
    sparse.as_gnu_mut().unwrap().set_is_extended(true);
    sparse.as_gnu_mut().unwrap().set_real_size(1 << 50);
    sparse.set_cksum();
    let mut output = gzip();
    output.write_all(sparse.as_bytes()).unwrap();
    let bytes = output.finish().unwrap();
    assert!(bytes.len() < 512);
    assert!(error(&bytes).contains("sparse files are not allowed"));
    for key in [
        "GNU.sparse.map",
        "GNU.sparse.major",
        "SCHILY.realsize",
        "SCHILY.filetype",
    ] {
        let mut output = gzip();
        frame(
            &mut output,
            tar::EntryType::XHeader,
            "extension",
            &pax(
                key,
                if key == "SCHILY.filetype" {
                    b"sparse"
                } else {
                    b"0"
                },
            ),
        );
        assert!(
            error(&output.finish().unwrap()).contains("PAX sparse files are not allowed"),
            "{key}"
        );
    }
}
#[test]
fn extension_headers_count_toward_the_header_and_metadata_budgets() {
    let mut output = gzip();
    for _ in 0..=MEMBER_LIMIT {
        // No body at all; ordinary tar iteration used to consume these without
        // exposing any entry to the caller's member counter.
        output
            .write_all(header(tar::EntryType::XGlobalHeader, "extension", 0).as_bytes())
            .unwrap();
    }
    assert!(error(&output.finish().unwrap()).contains("exceeds 16384 headers"));
    let payload = pax("comment", &vec![b'a'; 65_521]);
    assert_eq!(payload.len() as u64, archive::EXTENSION_LIMIT);
    let mut output = gzip();
    for _ in 0..archive::METADATA_LIMIT / archive::EXTENSION_LIMIT {
        frame(
            &mut output,
            tar::EntryType::XGlobalHeader,
            "extension",
            &payload,
        );
    }
    // The budget violation is detected from this final header, whose declared
    // payload is deliberately absent. The live fixture payload is only 64 KiB.
    output
        .write_all(header(tar::EntryType::XGlobalHeader, "extension", 1).as_bytes())
        .unwrap();
    assert!(error(&output.finish().unwrap()).contains("metadata exceeds 1 MiB"));
}
#[test]
fn tiny_host_system_metadata_is_bounded_before_copy_for_all_name_encodings() {
    for kind in [
        None,
        Some(tar::EntryType::GNULongName),
        Some(tar::EntryType::XHeader),
    ] {
        let mut output = gzip();
        if let Some(kind) = kind {
            let bytes = if kind.is_gnu_longname() {
                b"host-system\0".to_vec()
            } else {
                pax("path", b"host-system")
            };
            frame(&mut output, kind, "extension", &bytes);
        }
        output
            .write_all(
                header(
                    tar::EntryType::Regular,
                    if kind.is_none() {
                        "host-system"
                    } else {
                        "placeholder"
                    },
                    archive::HOST_SYSTEM_LIMIT + 1,
                )
                .as_bytes(),
            )
            .unwrap();
        let bytes = output.finish().unwrap();
        assert!(bytes.len() < 512);
        assert!(error(&bytes).contains("host-system exceeds 64 bytes"));
    }
}
#[test]
fn ambiguous_pax_size_and_orphan_or_link_extensions_fail_closed() {
    let mut output = gzip();
    frame(
        &mut output,
        tar::EntryType::XHeader,
        "extension",
        &pax("size", b"999999999999"),
    );
    output
        .write_all(header(tar::EntryType::Regular, "host-system", 0).as_bytes())
        .unwrap();
    assert!(error(&output.finish().unwrap()).contains("PAX size conflicts with raw header"));
    let mut output = gzip();
    frame(
        &mut output,
        tar::EntryType::XHeader,
        "extension",
        &pax("path", b"host-system"),
    );
    assert!(error(&output.finish().unwrap()).contains("extension has no following file"));
    let mut output = gzip();
    output
        .write_all(header(tar::EntryType::GNULongLink, "extension", 1).as_bytes())
        .unwrap();
    assert!(error(&output.finish().unwrap()).contains("long-link extensions are not allowed"));
    for value in [b"18446744073709551616".as_slice(), b"-1", b"a"] {
        let mut output = gzip();
        frame(
            &mut output,
            tar::EntryType::XHeader,
            "extension",
            &pax("size", value),
        );
        assert!(error(&output.finish().unwrap()).contains("PAX integer"));
    }
}
#[test]
fn gnu_long_names_and_bounded_pax_global_local_overrides_preserve_exact_files() {
    let scratch = tempfile::tempdir().unwrap();
    for global in [false, true] {
        let mut output = gzip();
        let system = host_system();
        if global {
            frame(
                &mut output,
                tar::EntryType::XGlobalHeader,
                "extension",
                &pax("path", b"unused-global-path"),
            );
            frame(
                &mut output,
                tar::EntryType::XHeader,
                "extension",
                &pax("path", b"host-system"),
            );
        } else {
            frame(
                &mut output,
                tar::EntryType::GNULongName,
                "extension",
                b"host-system\0",
            );
        }
        frame(
            &mut output,
            tar::EntryType::Regular,
            "placeholder",
            system.as_bytes(),
        );
        let runtime = Runtime::from_bytes(&output.finish().unwrap(), scratch.path()).unwrap();
        assert_eq!(
            fs::read_to_string(runtime.directory.path().join("assets/host-system")).unwrap(),
            system
        );
    }
}
#[test]
fn actual_gnu_and_bsd_pax_writers_preserve_long_library_names_bytes_and_modes() {
    use std::process::{Command, Stdio};
    let fixture = tempfile::tempdir().unwrap();
    let input = fixture.path().join("input");
    fs::create_dir(&input).unwrap();
    let mut members = files();
    let long_name = format!("host/{}/lib/lib{}.so.1", host_system(), "x".repeat(220));
    members.push((
        long_name.clone(),
        b"long filename library fixture".to_vec(),
        0o755,
    ));
    let mut list = Vec::new();
    for (name, bytes, mode) in &members {
        let target = input.join(name);
        fs::create_dir_all(target.parent().unwrap()).unwrap();
        fs::write(&target, bytes).unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(*mode)).unwrap();
        list.extend_from_slice(name.as_bytes());
        list.push(0);
    }
    let list_path = fixture.path().join("files");
    fs::write(&list_path, list).unwrap();
    for program in ["tar", "bsdtar"] {
        let version = Command::new(program)
            .arg("--version")
            .guarded_output()
            .unwrap();
        assert!(version.status.success());
        eprintln!(
            "actual host writer: {}",
            String::from_utf8_lossy(&version.stdout)
                .lines()
                .next()
                .unwrap()
        );
        let archive_path = fixture.path().join(format!("{program}.tar.gz"));
        // Exact producer options, not a hand-built model of the BSD/GNU format.
        let output = Command::new(program)
            .args(["--format=pax", "-czf"])
            .arg(&archive_path)
            .arg("-C")
            .arg(&input)
            .args(["--null", "-T", "-"])
            .env("COPYFILE_DISABLE", "1")
            .stdin(Stdio::from(fs::File::open(&list_path).unwrap()))
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // Not guarded_output: that forces stdin to null.
            .guarded_spawn()
            .and_then(|child| child.wait_with_output())
            .unwrap();
        assert!(
            output.status.success(),
            "{program}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let runtime = Runtime::from_archive(&archive_path, fixture.path()).unwrap();
        let inputs = runtime.select(Backend::Tcg, Architecture::X86_64).unwrap();
        assert!(inputs.qemu.is_some());
        for (name, bytes, mode) in &members {
            let path = runtime.directory.path().join("assets").join(name);
            assert_eq!(fs::read(&path).unwrap(), *bytes, "{program}: {name}");
            assert_eq!(
                fs::metadata(path).unwrap().permissions().mode() & 0o777,
                *mode,
                "{program}: {name}"
            );
        }
    }
}
