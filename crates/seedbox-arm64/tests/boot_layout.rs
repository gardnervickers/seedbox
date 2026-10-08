use seedbox_arm64::{
    backend::{Backend, CoreReg, DirtyPages, RunStop, SysReg},
    boot::{
        BootSpec, COMMAND_LINE_SIZE, DEFAULT_COMMAND_LINE, DTB_RESERVATION, GPA_LIMIT,
        MAX_RAM_BYTES,
    },
    fdt,
    machine::Machine,
    memory::Memory,
    Result, ABI, DTB_ADDR, INITRD_ADDR, PAGE_SIZE, RAM_BASE, RAM_SIZE, TIME_ADDR,
};
use sha2::{Digest, Sha256};
use std::{cell::RefCell, collections::BTreeMap, rc::Rc};

fn kernel(offset: u64, image_size: u64) -> Vec<u8> {
    let mut bytes = vec![0; 4096];
    bytes[8..16].copy_from_slice(&offset.to_le_bytes());
    bytes[16..24].copy_from_slice(&image_size.to_le_bytes());
    bytes[56..60].copy_from_slice(b"ARM\x64");
    bytes
}
fn property_u64(fdt: &::fdt::Fdt<'_>, name: &str) -> u64 {
    u64::from_be_bytes(
        fdt.find_node("/chosen")
            .unwrap()
            .property(name)
            .unwrap()
            .value
            .try_into()
            .unwrap(),
    )
}

#[test]
fn historical_fixture_fdt_bytes_and_layout() {
    let bytes = fdt::build(1, 4096).unwrap();
    // Captured from the original writer before the configurable layout change.
    assert_eq!(
        format!("{:x}", Sha256::digest(&bytes)),
        "0cf36f12100ac2955b46bd4dd4babae9b85c70ce3c0d92107b4249fa4f9fb713"
    );
    let kernel = kernel(0x80000, 8192);
    let initrd = vec![0; 4096];
    let layout = BootSpec::fixture(&kernel, &initrd, 1).validate().unwrap();
    assert_eq!(layout.memory_bytes(), RAM_SIZE);
    assert_eq!(
        layout.kernel_range(),
        RAM_BASE + 0x80000..RAM_BASE + 0x82000
    );
    assert_eq!(layout.initrd_range(), INITRD_ADDR..INITRD_ADDR + 4096);
    assert_eq!(layout.dtb_range(), DTB_ADDR..DTB_ADDR + DTB_RESERVATION);
    assert_eq!(layout.command_line(), DEFAULT_COMMAND_LINE);
    assert_eq!(fdt::build_for_layout(&layout).unwrap(), bytes);
    assert_eq!(
        fdt::build(1, 0).unwrap_err(),
        "initrd is empty or overlaps the device tree"
    );
    assert!(fdt::build(1, (DTB_ADDR - INITRD_ADDR) as usize).is_ok());
    assert!(fdt::build(1, (DTB_ADDR - INITRD_ADDR) as usize + 1).is_err());
}

#[test]
fn automatic_layout_accommodates_archive_beyond_fixture_gap() {
    let kernel = kernel(0x80000, 0x0200_0001);
    let initrd = vec![0x5a; 20 << 20];
    let spec = BootSpec::for_application(&kernel, &initrd, 17, 256 << 20, "rdinit=/init app=test")
        .unwrap();
    let layout = spec.validate().unwrap();
    assert_eq!(layout.kernel_range().end, RAM_BASE + 0x0208_0001);
    // Image.image_size reserves kernel BSS, not just the 4 KiB file copied in.
    assert_eq!(layout.initrd_range().start, RAM_BASE + 0x0208_4000);
    assert_eq!(layout.dtb_range().end, RAM_BASE + (256 << 20));
    assert_eq!(
        layout.dtb_range().start,
        layout.ram_range().end - DTB_RESERVATION
    );
    assert!(BootSpec::fixture(&kernel, &initrd, 17).validate().is_err());
    let bytes = fdt::build_for_layout(&layout).unwrap();
    assert!(bytes.len() as u64 <= DTB_RESERVATION);
    let tree = ::fdt::Fdt::new(&bytes).unwrap();
    let memory = tree.memory().regions().next().unwrap();
    assert_eq!(memory.starting_address as u64, RAM_BASE);
    assert_eq!(memory.size, Some(256 << 20));
    assert_eq!(property_u64(&tree, "linux,initrd-start"), spec.initrd_addr);
    assert_eq!(
        property_u64(&tree, "linux,initrd-end"),
        spec.initrd_addr + initrd.len() as u64
    );
    assert_eq!(tree.chosen().bootargs(), Some("rdinit=/init app=test"));
    assert_eq!(bytes, fdt::build_for_layout(&layout).unwrap());
    let changed = BootSpec { seed: 18, ..spec }.validate().unwrap();
    assert_ne!(bytes, fdt::build_for_layout(&changed).unwrap());
}

#[test]
fn minimum_layout_is_based_on_input_reservations_not_runtime_readiness() {
    let kernel = kernel(0, PAGE_SIZE as u64);
    let initrd = vec![0; PAGE_SIZE];
    let required = PAGE_SIZE as u64 * 2 + DTB_RESERVATION;
    let spec = BootSpec::for_application(&kernel, &initrd, 1, required, "").unwrap();
    assert_eq!(spec.initrd_addr, RAM_BASE + PAGE_SIZE as u64);
    assert_eq!(spec.dtb_addr, RAM_BASE + PAGE_SIZE as u64 * 2);
    assert!(
        BootSpec::for_application(&kernel, &initrd, 1, required - PAGE_SIZE as u64, "").is_err()
    );
    assert!(BootSpec::for_application(&kernel, &initrd, 1, PAGE_SIZE as u64, "").is_err());
}

#[test]
fn gpa_translation_checks_complete_span_and_excludes_mmio() {
    let kernel = kernel(0, 4096);
    let layout = BootSpec::fixture(&kernel, &[1], 1).validate().unwrap();
    assert_eq!(layout.ram_offset(RAM_BASE, 1).unwrap(), 0);
    assert_eq!(layout.ram_offset(INITRD_ADDR, 1).unwrap(), 0x0600_0000);
    assert_eq!(
        layout.ram_offset(layout.ram_range().end, 0).unwrap(),
        RAM_SIZE
    );
    for (addr, len) in [
        (RAM_BASE - 1, 1),
        (TIME_ADDR, PAGE_SIZE),
        (layout.ram_range().end, 1),
        (layout.ram_range().end - 1, 2),
        (u64::MAX, 2),
        (RAM_BASE, usize::MAX),
    ] {
        assert!(layout.ram_offset(addr, len).is_err(), "{addr:#x}, {len}");
    }
}

#[test]
fn maximum_conservative_ram_range_is_checked_without_allocating_it() {
    let kernel = kernel(0, 4096);
    let spec = BootSpec::for_application(&kernel, &[1], 1, MAX_RAM_BYTES, "").unwrap();
    assert_eq!(spec.validate().unwrap().ram_range().end, GPA_LIMIT);
    let bytes = fdt::build_for_layout(&spec.validate().unwrap()).unwrap();
    assert_eq!(
        ::fdt::Fdt::new(&bytes)
            .unwrap()
            .memory()
            .regions()
            .next()
            .unwrap()
            .size,
        Some(MAX_RAM_BYTES as usize)
    );
    for bytes in [
        0,
        1,
        RAM_SIZE as u64 + 1,
        MAX_RAM_BYTES + PAGE_SIZE as u64,
        !(PAGE_SIZE as u64 - 1),
    ] {
        assert!(BootSpec::for_application(&kernel, &[1], 1, bytes, "").is_err());
    }
}

#[test]
fn invalid_specs_fail_before_backend_creation() {
    CAPTURE.with(|capture| *capture.borrow_mut() = Capture::default());
    let kernel = kernel(0x80000, 4 * PAGE_SIZE as u64);
    let spec = BootSpec::fixture(&kernel, &[1], 1);
    let cases = [
        (
            BootSpec {
                initrd: &[],
                ..spec
            },
            "initrd is empty",
        ),
        (
            BootSpec {
                initrd_addr: RAM_BASE + 0x80000,
                ..spec
            },
            "kernel overlaps initrd",
        ),
        (
            BootSpec {
                initrd_addr: RAM_BASE + 0x84000,
                ..spec
            },
            "kernel overlaps initrd",
        ),
        (
            BootSpec {
                dtb_addr: RAM_BASE + 0x80000,
                ..spec
            },
            "kernel overlaps device tree",
        ),
        (
            BootSpec {
                initrd_addr: DTB_ADDR,
                ..spec
            },
            "initrd overlaps device tree",
        ),
        (
            BootSpec {
                initrd_addr: TIME_ADDR,
                ..spec
            },
            "outside guest RAM",
        ),
        (
            BootSpec {
                initrd_addr: spec.initrd_addr + 1,
                ..spec
            },
            "aligned to 16 KiB",
        ),
        (
            BootSpec {
                dtb_addr: spec.dtb_addr + 8,
                ..spec
            },
            "aligned to 16 KiB",
        ),
        (
            BootSpec {
                dtb_addr: RAM_BASE + RAM_SIZE as u64 - PAGE_SIZE as u64,
                ..spec
            },
            "outside guest RAM",
        ),
        (
            BootSpec {
                initrd_addr: RAM_BASE + RAM_SIZE as u64,
                ..spec
            },
            "outside guest RAM",
        ),
        (
            BootSpec {
                dtb_addr: !(PAGE_SIZE as u64 - 1),
                ..spec
            },
            "address overflow",
        ),
        (
            BootSpec {
                memory_bytes: !(PAGE_SIZE as u64 - 1),
                ..spec
            },
            "RAM address overflow",
        ),
        (
            BootSpec {
                memory_bytes: MAX_RAM_BYTES + PAGE_SIZE as u64,
                ..spec
            },
            "32-bit guest physical",
        ),
        (
            BootSpec {
                command_line: "a\0b",
                ..spec
            },
            "contains NUL",
        ),
    ];
    for (spec, message) in cases {
        let error = Machine::<CaptureBackend>::from_spec(spec).err().unwrap();
        assert!(error.contains(message), "{message}: {error}");
    }
    CAPTURE.with(|capture| assert_eq!(capture.borrow().created, 0));
}

#[test]
fn invalid_image_size_offset_and_endianness_are_rejected_before_vm() {
    CAPTURE.with(|capture| *capture.borrow_mut() = Capture::default());
    let mut cases = vec![
        b"bad".to_vec(),
        kernel(3, 4096),
        kernel(0, 0),
        kernel(0x20_0004, 4096),
        kernel(0x80000, u64::MAX),
        kernel(0, RAM_SIZE as u64 + 1),
    ];
    let mut big_endian = kernel(0, 4096);
    big_endian[24..32].copy_from_slice(&1u64.to_le_bytes());
    cases.push(big_endian);
    for kernel in cases {
        assert!(Machine::<CaptureBackend>::from_spec(BootSpec::fixture(&kernel, &[1], 1)).is_err());
    }
    CAPTURE.with(|capture| assert_eq!(capture.borrow().created, 0));
}

#[test]
fn command_line_is_not_silently_truncated_and_fdt_stays_in_reservation() {
    let kernel = kernel(0, 4096);
    let accepted = "x".repeat(COMMAND_LINE_SIZE - 1);
    let spec = BootSpec::for_application(&kernel, &[1], 1, 1 << 20, &accepted).unwrap();
    let bytes = fdt::build_for_layout(&spec.validate().unwrap()).unwrap();
    assert!(bytes.len() as u64 <= DTB_RESERVATION);
    assert_eq!(
        ::fdt::Fdt::new(&bytes).unwrap().chosen().bootargs(),
        Some(accepted.as_str())
    );
    let rejected = "x".repeat(COMMAND_LINE_SIZE);
    CAPTURE.with(|capture| *capture.borrow_mut() = Capture::default());
    assert!(Machine::<CaptureBackend>::from_spec(BootSpec {
        command_line: &rejected,
        ..spec
    })
    .is_err());
    CAPTURE.with(|capture| assert_eq!(capture.borrow().created, 0));
}

#[test]
fn constructor_maps_selected_ram_and_copies_to_checked_offsets() {
    CAPTURE.with(|capture| *capture.borrow_mut() = Capture::default());
    let kernel = kernel(0x80000, 2 << 20);
    let initrd = vec![0x6a; 20 << 20];
    let spec =
        BootSpec::for_application(&kernel, &initrd, 7, 64 << 20, "rdinit=/init boot=test").unwrap();
    let layout = spec.validate().unwrap();
    let expected_dtb = fdt::build_for_layout(&layout).unwrap();
    let _machine = Machine::<CaptureBackend>::from_spec(spec).unwrap();
    CAPTURE.with(|capture| {
        let capture = capture.borrow();
        assert_eq!(capture.created, 1);
        assert!(capture.vcpu_created);
        assert_eq!(capture.mappings.len(), 2);
        let (gpa, ram) = &capture.mappings[&0];
        assert_eq!(*gpa, RAM_BASE);
        assert_eq!(ram.len(), 64 << 20);
        // No guest has ever run; the inspected mappings are stopped.
        let bytes = unsafe { ram.bytes() };
        for (addr, expected) in [
            (layout.kernel_range().start, kernel.as_slice()),
            (spec.initrd_addr, initrd.as_slice()),
            (spec.dtb_addr, expected_dtb.as_slice()),
        ] {
            let offset = layout.ram_offset(addr, expected.len()).unwrap();
            assert_eq!(&bytes[offset..offset + expected.len()], expected);
        }
        assert!(bytes[0x81000..0x280000].iter().all(|byte| *byte == 0));
        assert_eq!(capture.regs[0], spec.dtb_addr);
        assert!(capture.regs[1..32].iter().all(|reg| *reg == 0));
        assert_eq!(capture.regs[32], layout.kernel_range().start);
        assert_eq!(capture.regs[33], 0x3c5);
        assert_eq!(capture.sys[&SysReg::MPIDR.0], 0x8000_0000);
        assert_eq!(capture.sys[&SysReg::SCTLR.0], 0x30d0_0800);
        assert_eq!(capture.sys[&SysReg::CNTKCTL.0], 0);
        assert_eq!(capture.sys[&SysReg::CNTV_CTL.0], 0);
        let (gpa, page) = &capture.mappings[&1];
        assert_eq!(*gpa, TIME_ADDR);
        assert_eq!(page.len(), PAGE_SIZE);
        assert_eq!(unsafe { page.word(16).unwrap() }, u64::MAX);
        assert_eq!(unsafe { page.word(24).unwrap() }, ABI);
    });
    CAPTURE.with(|capture| *capture.borrow_mut() = Capture::default());
}

struct Capture {
    created: usize,
    vcpu_created: bool,
    mappings: BTreeMap<u32, (u64, Rc<Memory>)>,
    regs: [u64; 34],
    sys: BTreeMap<u16, u64>,
}
impl Default for Capture {
    fn default() -> Self {
        Self {
            created: 0,
            vcpu_created: false,
            mappings: BTreeMap::new(),
            regs: [0; 34],
            sys: BTreeMap::new(),
        }
    }
}
thread_local! {
    static CAPTURE: RefCell<Capture> = RefCell::new(Capture::default());
}
struct CaptureBackend;
impl Backend for CaptureBackend {
    fn create() -> Result<Self> {
        CAPTURE.with(|capture| capture.borrow_mut().created += 1);
        Ok(Self)
    }
    fn map_memory(&mut self, slot: u32, ipa: u64, memory: Rc<Memory>) -> Result<()> {
        CAPTURE.with(|capture| capture.borrow_mut().mappings.insert(slot, (ipa, memory)));
        Ok(())
    }
    fn unmap_memory(&mut self, slot: u32) -> Result<()> {
        CAPTURE.with(|capture| capture.borrow_mut().mappings.remove(&slot));
        Ok(())
    }
    fn create_vcpu(&mut self) -> Result<()> {
        CAPTURE.with(|capture| capture.borrow_mut().vcpu_created = true);
        Ok(())
    }
    fn get_reg(&self, reg: CoreReg) -> Result<u64> {
        Ok(CAPTURE.with(|capture| capture.borrow().regs[reg_index(reg)]))
    }
    fn set_reg(&mut self, reg: CoreReg, value: u64) -> Result<()> {
        CAPTURE.with(|capture| capture.borrow_mut().regs[reg_index(reg)] = value);
        Ok(())
    }
    fn get_sys_reg(&self, reg: SysReg) -> Result<u64> {
        Ok(CAPTURE.with(|capture| capture.borrow().sys.get(&reg.0).copied().unwrap_or(0)))
    }
    fn set_sys_reg(&mut self, reg: SysReg, value: u64) -> Result<()> {
        CAPTURE.with(|capture| capture.borrow_mut().sys.insert(reg.0, value));
        Ok(())
    }
    fn run(&mut self) -> Result<RunStop> {
        panic!("constructor test must not execute a guest")
    }
    fn complete(&mut self, _: Option<u64>) -> Result<()> {
        unreachable!()
    }
    fn inject_spi(&mut self, _: u32, _: bool) -> Result<()> {
        unreachable!()
    }
    fn dirty_pages(&self, _: u32) -> Result<DirtyPages> {
        unreachable!()
    }
}
fn reg_index(reg: CoreReg) -> usize {
    match reg {
        CoreReg::X(n) => n as usize,
        CoreReg::Sp => 31,
        CoreReg::Pc => 32,
        CoreReg::Pstate => 33,
    }
}
