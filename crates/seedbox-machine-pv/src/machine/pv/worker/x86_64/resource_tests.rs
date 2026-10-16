use super::*;
#[test]
fn loader_e820_excludes_apertures_and_retains_default_boot_geometry() {
    for mib in [128u64, 256] {
        let mut image = vec![0; 3072];
        image[0x1f1] = 4;
        image[0x202..0x206].copy_from_slice(b"HdrS");
        image[0x206..0x208].copy_from_slice(&0x020fu16.to_le_bytes());
        image[0x211] = 1;
        image[0x236] = 1;
        image[0x22c..0x230].copy_from_slice(&0x7fffffffu32.to_le_bytes());
        image[0x258..0x260].copy_from_slice(&0x1000000u64.to_le_bytes());
        let layout = boot::RamLayout::native(mib << 20).unwrap();
        let mut ram = Memory::ram(layout.clone()).unwrap();
        let entry = load(
            &mut ram,
            &image,
            b"initrd",
            7,
            "console=ttyS0",
            false,
            false,
        )
        .unwrap();
        assert_eq!(entry.rip, boot::KERNEL + 0x200);
        let bytes = unsafe { ram.bytes() };
        let zero = &bytes[ZERO_PAGE as usize..ZERO_PAGE as usize + 4096];
        let mut expected = vec![(0, 0x9fc00), (boot::KERNEL, (mib << 20) - boot::KERNEL)];
        if mib == 256 {
            expected = vec![
                (0, 0x9fc00),
                (boot::KERNEL, 0x0a000000 - boot::KERNEL),
                (0x0b034000, 96 << 20),
            ];
        }
        assert_eq!(zero[0x1e8] as usize, expected.len());
        for (index, &(address, length)) in expected.iter().enumerate() {
            let at = 0x2d0 + 20 * index;
            assert_eq!(
                u64::from_le_bytes(zero[at..at + 8].try_into().unwrap()),
                address
            );
            assert_eq!(
                u64::from_le_bytes(zero[at + 8..at + 16].try_into().unwrap()),
                length
            );
            assert_eq!(
                u32::from_le_bytes(zero[at + 16..at + 20].try_into().unwrap()),
                1
            );
            assert!(layout.offset(address, length as usize).is_ok());
        }
        // The complete initial 4 GiB identity map still addresses MMIO and
        // all bootstrap structures; KVM backing, not guest PTE absence, traps IO.
        assert_eq!(
            u64::from_le_bytes(bytes[PML4 as usize..PML4 as usize + 8].try_into().unwrap()),
            (PML4 + 4096) | 3
        );
        let last = PML4 as usize + 6 * 4096 - 8;
        assert_eq!(
            u64::from_le_bytes(bytes[last..last + 8].try_into().unwrap()),
            0xffe00083
        );
    }
}
