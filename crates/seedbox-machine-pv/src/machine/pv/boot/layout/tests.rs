use super::*;
#[test]
fn arm_image_reservation_and_dynamic_dtb_are_checked_without_native_execution() {
    let ram = RamLayout::arm_test(2048 << 20).unwrap();
    let mut image = vec![0; 64];
    image[16..24].copy_from_slice(&4096u64.to_le_bytes());
    image[56..60].copy_from_slice(b"ARM\x64");
    let (entry, initrd) = super::super::arm_layout_for(&ram, &image, b"initrd").unwrap();
    assert_eq!(entry, 0x40000000);
    assert_eq!(initrd, 0x40004000);
    assert_eq!(ram.dtb().unwrap(), 0xbfff0000);
    #[cfg(target_arch = "x86_64")]
    {
        let tree =
            super::super::tree::build_for(&ram, initrd, 6, 7, "rdinit=/init", true, true).unwrap();
        let reg = [ram.base.to_be_bytes(), ram.capacity.to_be_bytes()].concat();
        assert!(tree.windows(reg.len()).any(|b| b == reg));
        assert!(tree.windows(8).any(|b| b == initrd.to_be_bytes()));
        assert!(tree.windows(8).any(|b| b == (initrd + 6).to_be_bytes()));
        assert!(tree.len() <= 65536);
    }
    image[16..24].copy_from_slice(&(3u64 << 30).to_le_bytes());
    assert!(super::super::arm_layout_for(&ram, &image, b"initrd").is_err());
    image[16..24].copy_from_slice(&4096u64.to_le_bytes());
    let small = RamLayout::arm_test(65536).unwrap();
    assert!(super::super::arm_layout_for(&small, &image, b"initrd").is_err());
}

#[test]
fn native_special_pages_are_outside_all_large_ram_segments() {
    let ram = RamLayout::build(65536 << 20, 0, &X86_HOLES, u64::MAX).unwrap();
    for (address, length) in [
        (0xfffbd000, 3 * 4096),
        (0xfffbc000, 4096),
        (0xfee00000, 4096),
        (0x0a000000, 16384),
    ] {
        assert!(ram.offset(address, length).is_err());
    }
    assert!(ram.offset(0x100000000, 16384).is_ok());
}

#[test]
fn default_geometry_and_capacity_are_preserved() {
    let ram = RamLayout::build(128 << 20, 0, &X86_HOLES, u64::MAX).unwrap();
    assert_eq!(ram.span, 128 << 20);
    assert_eq!(ram.segments, vec![0..128 << 20]);
    assert_eq!(ram.offset(0x100000, 4096).unwrap(), 0x100000);
}
#[test]
fn requested_ram_skips_apertures_without_subtracting_capacity() {
    for mib in [64, 128, 256, 512, 1024, 2048, 3072, 3328, 4096, 65536] {
        let ram = RamLayout::build(mib << 20, 0, &X86_HOLES, u64::MAX).unwrap();
        assert_eq!(
            ram.segments.iter().map(|r| r.end - r.start).sum::<u64>(),
            mib << 20
        );
        assert_eq!(ram.capacity, mib << 20);
        assert!(ram.segments.len() <= 3);
        for r in &ram.segments {
            assert!(r.start.is_multiple_of(ALIGN) && r.end.is_multiple_of(ALIGN));
            for h in &X86_HOLES {
                assert!(r.end <= h.start || r.start >= h.end);
            }
            assert!(ram.offset(r.start, (r.end - r.start) as usize).is_ok());
        }
        for h in &X86_HOLES {
            assert!(ram.offset(h.start, 1).is_err());
            assert!(ram.offset(h.start - 1, 2).is_err());
        }
    }
}
#[test]
fn arm_capacity_dtb_and_address_limit_are_checked() {
    for mib in [64, 128, 256, 512, 1024, 2048, 3072] {
        let ram = RamLayout::build(mib << 20, 0x40000000, &[], 1 << 32).unwrap();
        assert_eq!(ram.top(), 0x40000000 + (mib << 20));
        assert_eq!(ram.dtb().unwrap(), ram.top() - 65536);
    }
    assert!(RamLayout::build((3072 << 20) + ALIGN, 0x40000000, &[], 1 << 32).is_err());
}
#[test]
fn bad_resources_and_overflow_fail_without_allocation() {
    for bad in [0, 1, 16383, u64::MAX, u64::MAX - 16383] {
        assert!(RamLayout::build(bad, 0, &X86_HOLES, u64::MAX).is_err());
    }
    let ram = RamLayout::build(2048 << 20, 0, &X86_HOLES, u64::MAX).unwrap();
    let address = ram.place(4096, 0x80000000, 0x2000000, 4096).unwrap();
    assert_eq!(address, 0x7ffff000);
    assert!(ram.offset(u64::MAX, 2).is_err());
    assert!(ram.offset(0x09fff000, 8192).is_err());
    assert_eq!(ram.offset(0x0b034000, 4096).unwrap(), 0x0b034000);
}
