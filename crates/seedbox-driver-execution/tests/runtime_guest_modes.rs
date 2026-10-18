//! Portable composition regression: host modes on guest ELF input files do not
//! determine the executable modes of the resulting guest initramfs entries.
//! All binaries here are structural fixtures; none is executed.
use seedbox_driver_execution::runtime_assets::{Backend, Runtime};
use seedbox_machine::{
    application::{boot_archive_configured, GuestBootstrap, GuestEnvironment, Kernel, NamedVolume},
    definition::{FilesystemArchive, Launch, PreparedApplication},
};
use seedbox_spec::definition::Architecture;
use std::{collections::BTreeMap, fs, os::unix::fs::PermissionsExt};

type Member = (String, Vec<u8>, u32);
fn elf(machine: u16) -> Vec<u8> {
    let mut bytes = vec![0; 120];
    bytes[..7].copy_from_slice(b"\x7fELF\x02\x01\x01");
    bytes[16..18].copy_from_slice(&2u16.to_le_bytes());
    bytes[18..20].copy_from_slice(&machine.to_le_bytes());
    bytes[20..24].copy_from_slice(&1u32.to_le_bytes());
    bytes[32..40].copy_from_slice(&64u64.to_le_bytes());
    bytes[52..54].copy_from_slice(&64u16.to_le_bytes());
    bytes[54..56].copy_from_slice(&56u16.to_le_bytes());
    bytes[56..58].copy_from_slice(&1u16.to_le_bytes());
    bytes[64..68].copy_from_slice(&1u32.to_le_bytes()); // PT_LOAD
    bytes[96..104].copy_from_slice(&120u64.to_le_bytes());
    bytes[104..112].copy_from_slice(&120u64.to_le_bytes());
    bytes
}
fn host() -> String {
    format!(
        "{}-{}",
        std::env::consts::ARCH,
        if cfg!(target_os = "macos") {
            "darwin"
        } else {
            "linux"
        }
    )
}
fn native() -> Architecture {
    if cfg!(target_arch = "aarch64") {
        Architecture::Aarch64
    } else {
        Architecture::X86_64
    }
}
fn layout() -> Vec<Member> {
    let mut members = vec![("host-system".into(), host().into_bytes(), 0o644)];
    for (backend, arch) in [("pv", std::env::consts::ARCH), ("tcg", "x86_64")] {
        let prefix = format!("{backend}/{arch}");
        for name in ["init", "mke2fs"] {
            members.push((
                format!("{prefix}/{name}"),
                elf(if arch == "aarch64" { 183 } else { 62 }),
                0o644,
            ));
        }
        for name in [
            "kernel",
            "command-line",
            if backend == "pv" {
                "progress.tar"
            } else {
                "firmware"
            },
        ] {
            members.push((format!("{prefix}/{name}"), name.as_bytes().to_vec(), 0o644));
        }
    }
    let executable = if cfg!(target_os = "linux") {
        elf(if cfg!(target_arch = "aarch64") {
            183
        } else {
            62
        })
    } else {
        [
            0xfeed_facf_u32.to_le_bytes(),
            (if cfg!(target_arch = "aarch64") {
                0x0100_000c_u32
            } else {
                0x0100_0007_u32
            })
            .to_le_bytes(),
        ]
        .concat()
    };
    for (name, bytes, mode) in [
        (
            "bin/qemu-system-x86_64",
            b"host launcher fixture".to_vec(),
            0o755,
        ),
        ("libexec/qemu-system-x86_64", executable, 0o755),
        ("lib/loader", b"host library fixture".to_vec(), 0o644),
        ("share/qemu/qboot.rom", b"firmware fixture".to_vec(), 0o644),
    ] {
        members.push((format!("host/{}/{name}", host()), bytes, mode));
    }
    members
}
fn package(members: &[Member]) -> Vec<u8> {
    let mut archive = tar::Builder::new(flate2::write::GzEncoder::new(
        Vec::new(),
        flate2::Compression::fast(),
    ));
    for (name, bytes, mode) in members {
        let mut header = tar::Header::new_gnu();
        header.set_mode(*mode);
        header.set_size(bytes.len() as u64);
        archive
            .append_data(&mut header, name, bytes.as_slice())
            .unwrap();
    }
    archive.into_inner().unwrap().finish().unwrap()
}
fn application() -> PreparedApplication {
    let mut archive = tar::Builder::new(Vec::new());
    let mut header = tar::Header::new_gnu();
    header.set_mode(0o755);
    header.set_uid(0);
    header.set_gid(0);
    header.set_mtime(0);
    header.set_size(3);
    archive
        .append_data(&mut header, "application", b"app".as_slice())
        .unwrap();
    PreparedApplication {
        filesystem: FilesystemArchive::new(archive.into_inner().unwrap()).unwrap(),
        launch: Launch {
            argv: vec!["/application".into()],
            environment: BTreeMap::new(),
            working_directory: "/".into(),
            user: "0:0".into(),
        },
    }
}
fn cpio_member<'a>(bytes: &'a [u8], expected: &[u8]) -> (u32, &'a [u8]) {
    let mut offset = 0;
    loop {
        assert_eq!(&bytes[offset..offset + 6], b"070701");
        let field = |index: usize| {
            u32::from_str_radix(
                std::str::from_utf8(&bytes[offset + 6 + index * 8..offset + 14 + index * 8])
                    .unwrap(),
                16,
            )
            .unwrap() as usize
        };
        let mode = field(1) as u32;
        let size = field(6);
        let name_size = field(11);
        assert!(name_size > 0);
        let name = &bytes[offset + 110..offset + 110 + name_size - 1];
        assert_eq!(bytes[offset + 110 + name_size - 1], 0);
        offset = (offset + 110 + name_size + 3) & !3;
        let data = &bytes[offset..offset + size];
        offset = (offset + size + 3) & !3;
        if name == expected {
            return (mode, data);
        }
        assert_ne!(name, b"TRAILER!!!", "missing expected guest file");
    }
}

#[test]
fn read_only_guest_elf_inputs_select_and_become_executable_in_each_guest_archive() {
    let scratch = tempfile::tempdir().unwrap();
    let runtime = Runtime::from_bytes(&package(&layout()), scratch.path()).unwrap();
    for (backend, arch) in [
        (Backend::Pv, native()),
        (Backend::Tcg, Architecture::X86_64),
    ] {
        let inputs = runtime.select(backend, arch).unwrap();
        for name in ["init", "mke2fs"] {
            assert_eq!(
                fs::metadata(inputs.guest_assets.join(name))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o644
            );
        }
        let init = fs::read(inputs.guest_assets.join("init")).unwrap();
        let formatter = fs::read(inputs.guest_assets.join("mke2fs")).unwrap();
        let bootstrap = GuestBootstrap {
            kernel: Kernel {
                image: vec![],
                command_line: String::new(),
            },
            init: init.clone(),
            base_initramfs: vec![],
            epoch_seconds: 86400,
        };
        let mut environment = GuestEnvironment::new("node", "127.0.0.1 localhost\n", false);
        environment.named_volumes.push(NamedVolume {
            name: "data".into(),
            target: "/data".into(),
        });
        let boot =
            boot_archive_configured(&application(), &bootstrap, &environment, Some(&formatter))
                .unwrap();
        assert_eq!(cpio_member(&boot, b"init"), (0o100755, init.as_slice()));
        assert_eq!(
            cpio_member(&boot, b"seedbox/mke2fs"),
            (0o100755, formatter.as_slice())
        );
    }
}
#[test]
fn read_only_guest_files_still_require_the_requested_elf_architecture() {
    let scratch = tempfile::tempdir().unwrap();
    for name in ["init", "mke2fs"] {
        let mut members = layout();
        members
            .iter_mut()
            .find(|(path, _, _)| path == &format!("tcg/x86_64/{name}"))
            .unwrap()
            .1 = elf(183);
        let runtime = Runtime::from_bytes(&package(&members), scratch.path()).unwrap();
        assert!(runtime
            .select(Backend::Tcg, Architecture::X86_64)
            .unwrap_err()
            .to_string()
            .contains("wrong architecture"));
    }
}
#[test]
fn host_qemu_launcher_and_binary_still_require_execute_permission() {
    let scratch = tempfile::tempdir().unwrap();
    for directory in ["bin", "libexec"] {
        let mut members = layout();
        members
            .iter_mut()
            .find(|(path, _, _)| path == &format!("host/{}/{directory}/qemu-system-x86_64", host()))
            .unwrap()
            .2 = 0o644;
        let runtime = Runtime::from_bytes(&package(&members), scratch.path()).unwrap();
        assert!(runtime
            .select(Backend::Tcg, Architecture::X86_64)
            .unwrap_err()
            .to_string()
            .contains("not executable"));
    }
}
