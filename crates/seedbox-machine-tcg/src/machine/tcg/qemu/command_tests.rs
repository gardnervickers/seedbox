use super::super::{Boot, Initramfs, Manifest};
use std::ffi::OsStr;

fn arguments(boot: Boot) -> (tempfile::TempDir, Vec<String>) {
    seeded_arguments(boot, 7)
}

fn seeded_arguments(boot: Boot, seed: u64) -> (tempfile::TempDir, Vec<String>) {
    let directory = tempfile::tempdir().unwrap();
    let manifest = Manifest {
        boot,
        memory_mib: 64,
        disk: None,
        network: None,
    };
    let command = super::command(
        OsStr::new("qemu").as_ref(),
        &manifest,
        directory.path(),
        false,
        seed,
        None,
    )
    .unwrap();
    let arguments = command
        .get_args()
        .map(|argument| argument.to_str().unwrap().to_owned())
        .collect();
    (directory, arguments)
}

fn value<'a>(arguments: &'a [String], flag: &str) -> Vec<&'a str> {
    arguments
        .windows(2)
        .filter(|pair| pair[0] == flag)
        .map(|pair| pair[1].as_str())
        .collect()
}

#[test]
fn linux_boots_owned_inputs_on_the_fixed_x86_microvm_board() {
    let (directory, arguments) = arguments(Boot::Linux {
        kernel: vec![1; 4].into(),
        initramfs: vec![2; 4].into(),
        firmware: vec![3; 4].into(),
        command_line: "console=ttyS0".into(),
        epoch_seconds: 0,
    });
    assert_eq!(
        value(&arguments, "-machine"),
        [
            "microvm,acpi=on,pic=on,pit=on,rtc=on,isa-serial=on,pcie=off,ioapic2=off,\
              usb=off,x-option-roms=off,auto-kernel-cmdline=off"
        ]
    );
    assert_eq!(value(&arguments, "-cpu"), ["qemu64,arat=on"]);
    assert_eq!(value(&arguments, "-smp"), ["1"]);
    assert_eq!(
        value(&arguments, "-rtc"),
        ["base=1970-01-01T00:00:00,clock=vm"]
    );
    // `-seed` seeds only QEMU's own PRNG. The guest's entropy and realtime
    // offset both derive from the boot seed (7) on its kernel command line.
    assert_eq!(value(&arguments, "-seed"), ["7"]);
    assert_eq!(
        value(&arguments, "-append"),
        [format!(
            "console=ttyS0 seedbox.entropy=0000000000000007 seedbox.clock_offset_ns={}",
            seedbox_machine::clock::boot_realtime_offset_ns(7)
        )]
    );
    for (flag, name, bytes) in [
        ("-bios", "firmware", [3; 4]),
        ("-kernel", "kernel", [1; 4]),
        ("-initrd", "initramfs", [2; 4]),
    ] {
        let path = directory.path().join(name);
        assert_eq!(value(&arguments, flag), [path.to_str().unwrap()]);
        assert_eq!(std::fs::read(path).unwrap(), bytes);
    }
    let globals = value(&arguments, "-global");
    for global in [
        "virtio-mmio.ioeventfd=off",
        "virtio-mmio.force-legacy=true",
        "serial.seedbox-console=on",
    ] {
        assert!(globals.contains(&global), "{global}");
    }
    assert!(!arguments
        .iter()
        .any(|argument| argument.starts_with("loader")));
    assert!(!arguments.iter().any(|argument| argument == "-incoming"));
}

fn linux(command_line: &str) -> Boot {
    Boot::Linux {
        kernel: vec![1; 4].into(),
        initramfs: vec![2; 4].into(),
        firmware: vec![3; 4].into(),
        command_line: command_line.into(),
        epoch_seconds: 0,
    }
}

/// The board gives the guest no seeded RNG: before the boot seed reached the
/// kernel command line, every seed booted byte-identical guest entropy.
#[test]
fn each_boot_seed_reaches_the_guest_kernel_command_line() {
    let append = |seed| {
        let (_directory, arguments) = seeded_arguments(linux("console=ttyS0"), seed);
        value(&arguments, "-append")[0].to_owned()
    };
    assert_eq!(append(7), append(7));
    assert_ne!(append(7), append(8));
    assert_eq!(
        append(u64::MAX),
        format!(
            "console=ttyS0 seedbox.entropy=ffffffffffffffff seedbox.clock_offset_ns={}",
            seedbox_machine::clock::boot_realtime_offset_ns(u64::MAX)
        )
    );
    // The manifest reserves the width of both per-boot arguments, so the
    // longest accepted base line still fits the kernel's 2047 bytes, exactly
    // when the offset has the widest form (a minus sign and seven digits).
    let longest = "x".repeat(super::super::COMMAND_LINE_CAPACITY - 1);
    let widest = (0..)
        .find(|&seed| seedbox_machine::clock::boot_realtime_offset_ns(seed) <= -1_000_000)
        .unwrap();
    let length = |seed| value(&seeded_arguments(linux(&longest), seed).1, "-append")[0].len();
    assert_eq!(length(widest), super::super::COMMAND_LINE_SIZE - 1);
    for seed in 0..64 {
        assert!(length(seed) < super::super::COMMAND_LINE_SIZE);
    }
}

#[test]
fn a_retained_initramfs_file_is_passed_to_qemu_in_place() {
    let retained = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(retained.path(), [2; 4]).unwrap();
    let (directory, arguments) = arguments(Boot::Linux {
        kernel: vec![1; 4].into(),
        initramfs: Initramfs::File {
            path: retained.path().into(),
            length: 4,
        },
        firmware: vec![3; 4].into(),
        command_line: "console=ttyS0".into(),
        epoch_seconds: 0,
    });
    assert_eq!(
        value(&arguments, "-initrd"),
        [retained.path().to_str().unwrap()]
    );
    assert!(!directory.path().join("initramfs").exists());
    for (flag, name) in [("-kernel", "kernel"), ("-bios", "firmware")] {
        let path = directory.path().join(name);
        assert_eq!(value(&arguments, flag), [path.to_str().unwrap()]);
    }
}

#[test]
fn a_test_program_is_the_bios_image_with_no_kernel() {
    let (directory, arguments) =
        arguments(Boot::Program(vec![9; super::super::PROGRAM_BYTES].into()));
    let path = directory.path().join("program");
    assert_eq!(value(&arguments, "-bios"), [path.to_str().unwrap()]);
    assert_eq!(
        std::fs::read(path).unwrap().len(),
        super::super::PROGRAM_BYTES
    );
    for flag in ["-kernel", "-initrd", "-append"] {
        assert!(value(&arguments, flag).is_empty(), "{flag}");
    }
    assert!(value(&arguments, "-device")
        .iter()
        .all(|device| !device.starts_with("loader")));
}
