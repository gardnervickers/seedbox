//! Bounded validation owns no expanded per-file payloads; archive bytes stay exact.
use seedbox_machine::definition::{ArchiveValidationLimits, FilesystemArchive};
fn generous() -> ArchiveValidationLimits {
    ArchiveValidationLimits {
        entries: 1000,
        path_bytes: 1 << 20,
        expanded_bytes: 16 << 20,
        metadata_bytes: 1 << 20,
    }
}
fn archive(name: &str, bytes: &[u8]) -> Vec<u8> {
    let mut b = tar::Builder::new(Vec::new());
    let mut h = tar::Header::new_gnu();
    h.set_mode(0o644);
    h.set_uid(0);
    h.set_gid(0);
    h.set_mtime(0);
    h.set_size(bytes.len() as u64);
    h.set_cksum();
    b.append_data(&mut h, name, bytes).unwrap();
    b.into_inner().unwrap()
}
#[test]
fn validation_preserves_exact_bytes_and_bounds_each_logical_resource() {
    let bytes = archive("parent/file", &vec![7; 2 << 20]);
    let a = FilesystemArchive::new_bounded(bytes.clone(), generous()).unwrap();
    assert_eq!(a.as_bytes(), bytes);
    for field in 0..4 {
        let mut l = generous();
        match field {
            0 => l.entries = 1,
            1 => l.path_bytes = 3,
            2 => l.expanded_bytes = 1024,
            _ => l.metadata_bytes = 0,
        };
        assert!(FilesystemArchive::new_bounded(bytes.clone(), l).is_err());
    }
    let empty = tar::Builder::new(Vec::new()).into_inner().unwrap();
    assert!(FilesystemArchive::new_bounded(empty, generous()).is_ok());
}
#[test]
fn streamed_validation_preserves_regular_hardlink_and_rejection_semantics() {
    let mut b = tar::Builder::new(Vec::new());
    let mut h = tar::Header::new_gnu();
    h.set_size(4);
    h.set_mode(0o644);
    h.set_uid(0);
    h.set_gid(0);
    h.set_mtime(0);
    h.set_cksum();
    b.append_data(&mut h, "file", &b"data"[..]).unwrap();
    let mut h = tar::Header::new_gnu();
    h.set_entry_type(tar::EntryType::Link);
    h.set_size(0);
    h.set_link_name("file").unwrap();
    h.set_mode(0o644);
    h.set_uid(0);
    h.set_gid(0);
    h.set_mtime(0);
    h.set_cksum();
    b.append_data(&mut h, "alias", &[][..]).unwrap();
    let bytes = b.into_inner().unwrap();
    assert!(FilesystemArchive::new_bounded(bytes.clone(), generous()).is_ok());
    assert!(FilesystemArchive::new(bytes).is_ok());
    let mut b = tar::Builder::new(Vec::new());
    let mut h = tar::Header::new_gnu();
    h.set_size(0);
    h.set_mode(0o644);
    h.set_uid(0);
    h.set_gid(0);
    h.set_mtime(0);
    h.set_cksum();
    b.append_data(&mut h, "same", &[][..]).unwrap();
    b.append_data(&mut h, "same", &[][..]).unwrap();
    let bytes = b.into_inner().unwrap();
    assert!(FilesystemArchive::new(bytes.clone()).is_err());
    assert!(FilesystemArchive::new_bounded(bytes, generous()).is_err());
    let bytes = archive("file", b"data");
    assert!(FilesystemArchive::new(bytes[..513].to_vec()).is_err());
    assert!(FilesystemArchive::new_bounded(bytes[..513].to_vec(), generous()).is_err());
}
#[test]
fn pax_size_override_survives_intermediate_gnu_headers_and_is_bounded_first() {
    fn append(bytes: &mut Vec<u8>, mut h: tar::Header, data: &[u8]) {
        h.set_uid(0);
        h.set_gid(0);
        h.set_mtime(0);
        h.set_mode(0o644);
        h.set_cksum();
        bytes.extend(h.as_bytes());
        bytes.extend(data);
        while !bytes.len().is_multiple_of(512) {
            bytes.push(0);
        }
    }
    let mut b = tar::Builder::new(Vec::new());
    b.append_pax_extensions([("size", &b"4"[..])]).unwrap();
    let mut bytes = b.into_inner().unwrap();
    bytes.truncate(bytes.len() - 1024);
    let name = "x".repeat(150);
    let mut h = tar::Header::new_gnu();
    h.set_path("././@LongLink").unwrap();
    h.set_entry_type(tar::EntryType::new(b'L'));
    h.set_size(151);
    let mut long = name.as_bytes().to_vec();
    long.push(0);
    append(&mut bytes, h, &long);
    let mut h = tar::Header::new_gnu();
    h.set_path("short").unwrap();
    h.set_size(0);
    append(&mut bytes, h, b"data");
    bytes.extend([0; 1024]);
    assert!(FilesystemArchive::new(bytes.clone()).is_ok());
    assert!(FilesystemArchive::new_bounded(bytes.clone(), generous()).is_ok());
    let mut small = generous();
    small.path_bytes = 100;
    assert!(FilesystemArchive::new_bounded(bytes, small)
        .unwrap_err()
        .to_string()
        .contains("extension path limit"));
    let mut b = tar::Builder::new(Vec::new());
    let value = vec![b'a'; 4096];
    b.append_pax_extensions([("comment", &value[..])]).unwrap();
    let mut h = tar::Header::new_gnu();
    h.set_size(0);
    h.set_uid(0);
    h.set_gid(0);
    h.set_mtime(0);
    h.set_mode(0o644);
    h.set_cksum();
    b.append_data(&mut h, "file", &[][..]).unwrap();
    let bytes = b.into_inner().unwrap();
    assert!(FilesystemArchive::new(bytes.clone()).is_ok());
    let mut small = generous();
    small.metadata_bytes = 100;
    assert!(FilesystemArchive::new_bounded(bytes, small)
        .unwrap_err()
        .to_string()
        .contains("extension metadata limit"));
}
#[test]
fn bounded_validation_does_not_discard_payload_when_packaging_later() {
    use seedbox_machine::{
        application::{boot_archive, GuestBootstrap, Kernel},
        definition::{Launch, PreparedApplication},
    };
    let payload = b"full\0binary\xffcontents";
    let app = PreparedApplication {
        filesystem: FilesystemArchive::new_bounded(archive("app", payload), generous()).unwrap(),
        launch: Launch {
            argv: vec!["/app".into()],
            environment: Default::default(),
            working_directory: "/".into(),
            user: "0:0".into(),
        },
    };
    let bytes = boot_archive(
        &app,
        &GuestBootstrap {
            kernel: Kernel {
                image: vec![],
                command_line: String::new(),
            },
            init: vec![1],
            base_initramfs: vec![],
            epoch_seconds: 0,
        },
    )
    .unwrap();
    let mut at = 0;
    loop {
        assert_eq!(&bytes[at..at + 6], b"070701");
        let number = |field: usize| {
            usize::from_str_radix(
                std::str::from_utf8(&bytes[at + 6 + field * 8..at + 14 + field * 8]).unwrap(),
                16,
            )
            .unwrap()
        };
        let size = number(6);
        let end = at + 110 + number(11);
        let name = &bytes[at + 110..end - 1];
        let data = (end + 3) & !3;
        if name == b"rootfs/app" {
            assert_eq!(&bytes[data..data + size], payload);
            break;
        }
        assert_ne!(name, b"TRAILER!!!");
        at = (data + size + 3) & !3;
    }
}
#[test]
fn gnu_sparse_file_is_still_rejected_without_expanding_its_holes() {
    // tar 0.4 distinguishes EntryType::GNUSparse from Regular: the application
    // reader has always rejected the former. Declare a huge logical hole without
    // allocating it; neither validator may turn it into an accepted regular file.
    let mut h = tar::Header::new_gnu();
    h.set_path("sparse").unwrap();
    h.set_entry_type(tar::EntryType::new(b'S'));
    h.set_size(0);
    h.set_mode(0o644);
    h.set_uid(0);
    h.set_gid(0);
    h.set_mtime(0);
    h.as_gnu_mut().unwrap().set_real_size(1 << 40);
    h.as_gnu_mut().unwrap().sparse[0].set_offset(1 << 40);
    h.as_gnu_mut().unwrap().sparse[0].set_length(0);
    h.set_cksum();
    let mut bytes = h.as_bytes().to_vec();
    bytes.extend([0; 1024]);
    assert!(FilesystemArchive::new(bytes.clone())
        .unwrap_err()
        .to_string()
        .contains("unsupported filesystem tar entry type"));
    assert!(FilesystemArchive::new_bounded(bytes, generous()).is_err());
}
