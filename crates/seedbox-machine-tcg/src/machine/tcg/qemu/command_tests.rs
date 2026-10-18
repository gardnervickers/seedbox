use super::super::{Boot, Manifest};
use std::ffi::OsStr;

fn arguments(boot: Boot) -> (tempfile::TempDir, Vec<String>) {
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
        7,
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
    assert_eq!(value(&arguments, "-append"), ["console=ttyS0"]);
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
