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

#[test]
#[ignore = "requires native KVM and SEEDBOX_PV_CLANG/SEEDBOX_PV_LD"]
fn native_shared_pic_handler_snapshot_preserves_mask_ack_and_exact_redelivery() {
    use super::super::{read_snapshot, restore_machine, BootInputs};
    let kernel = super::super::super::test_image::assemble(include_str!(
        "../../../../../../seedbox-machine/src/pv/guest_irq_shared.S"
    ));
    let inputs = BootInputs {
        witness: super::super::protocol::BootWitness {
            mark: None,
            boot: 0,
            initial: seedbox_machine::machine::execution_evidence::chain::initial(),
        },
        memory_bytes: boot::RAM_BYTES as u64,
        kernel,
        initrd: b"initrd".to_vec(),
        seed: 7,
        command_line: "console=ttyS0".into(),
        disk: None,
        network: None,
        policy: None,
    };
    let mut vm = Machine::<Native>::boot(
        &inputs.kernel,
        &inputs.initrd,
        7,
        &inputs.command_line,
        None,
        None,
    )
    .unwrap();
    assert_eq!(
        vm.run(0, 5, 1000, false, &[], vec![]).unwrap(),
        Stop::Blocked(Some(5))
    );
    assert_eq!(
        vm.run(5, 10, 1000, false, &[], vec![]).unwrap(),
        Stop::Horizon
    );
    assert!(vm.state.timer, "handler has not acknowledged the device");
    assert_eq!(vm.state.devices.pic.imr, 0xff);
    assert_eq!(vm.state.devices.pic.isr, 0);
    assert!(
        !vm.native.interruptible(),
        "snapshot inside handler retains IF=0"
    );
    assert_eq!(vm.page.word(PIC_UPDATE), 0);
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("handler");
    vm.capture(&path).unwrap();
    let mut restored = restore_machine(&inputs, read_snapshot(&path, &inputs).unwrap()).unwrap();
    // Adopt beyond the new deadline before the stopped handler arms it.
    // Its old ACK clears only the old timer. Arming the overdue timer then
    // unmasking must deliver another IRQ at IRET, without another clock read.
    // The first return instruction sets a sentinel; the second handler traps
    // if it ran, so a later HLT rendezvous cannot satisfy this assertion.
    for machine in [&mut vm, &mut restored] {
        assert_eq!(
            machine.run(20, 21, 1000, false, &[], vec![]).unwrap(),
            Stop::PoweredOff
        );
        assert_eq!(machine.state.now, 20);
        assert!(!machine.state.timer);
        assert_eq!(machine.state.devices.pic.isr, 0);
    }
    assert_eq!(vm.state.witness, restored.state.witness);
    assert_eq!(
        vm.native.capture().unwrap(),
        restored.native.capture().unwrap()
    );
    assert_eq!(unsafe { vm.page.bytes() }, unsafe { restored.page.bytes() });
}

#[test]
#[ignore = "requires native KVM and SEEDBOX_PV_CLANG/SEEDBOX_PV_LD"]
fn native_shared_virtio_handler_snapshot_preserves_ack_and_exact_redelivery() {
    use super::super::{read_snapshot, restore_machine, BootInputs};
    let kernel = super::super::super::test_image::assemble(include_str!(
        "../../../../../../seedbox-machine/src/pv/guest_virtio_irq_shared.S"
    ));
    let inputs = BootInputs {
        witness: super::super::protocol::BootWitness {
            mark: None,
            boot: 0,
            initial: seedbox_machine::machine::execution_evidence::chain::initial(),
        },
        memory_bytes: boot::RAM_BYTES as u64,
        kernel,
        initrd: b"initrd".to_vec(),
        seed: 7,
        command_line: "console=ttyS0".into(),
        disk: None,
        network: Some([2, 0, 0, 0, 0, 7]),
        policy: None,
    };
    let mut vm = Machine::<Native>::boot(
        &inputs.kernel,
        &inputs.initrd,
        7,
        &inputs.command_line,
        None,
        inputs.network,
    )
    .unwrap();
    // Fixture setup: negotiate one RX ring and offer three distinct buffers.
    // Actual frames, DMA, device assertions, guest ACKs and PIC delivery below
    // all use the production paths. No synthesized device interrupt status.
    unsafe {
        vm.ram.modify(|ram| {
            let net = vm.state.net.as_mut().unwrap();
            for (offset, value) in [
                (0x70, 3),
                (0x24, 1),
                (0x20, 1),
                (0x70, 11),
                (0x30, 0),
                (0x38, 16),
                (0x80, 0x4000000),
                (0x90, 0x4001000),
                (0xa0, 0x4002000),
                (0x44, 1),
                (0x70, 15),
            ] {
                net.write(ram, offset, 4, value, 0).unwrap();
            }
            for i in 0..3usize {
                let at = 0x4000000 + i * 16;
                ram[at..at + 8].copy_from_slice(&(0x4008000u64 + i as u64 * 2048).to_le_bytes());
                ram[at + 8..at + 12].copy_from_slice(&1526u32.to_le_bytes());
                ram[at + 12..at + 14].copy_from_slice(&2u16.to_le_bytes());
                ram[0x4001004 + i * 2..0x4001006 + i * 2]
                    .copy_from_slice(&(i as u16).to_le_bytes());
            }
            ram[0x4001002..0x4001004].copy_from_slice(&3u16.to_le_bytes());
        })
    };
    assert_eq!(
        vm.run(0, 5, 1000, false, &[], vec![]).unwrap(),
        Stop::Blocked(None)
    );
    assert_eq!(
        vm.run(5, 10, 1000, false, &[], vec![vec![41; 60]]).unwrap(),
        Stop::Horizon
    );
    assert!(!vm.state.net.as_ref().unwrap().irq());
    assert_eq!(vm.state.devices.pic.imr, 0xff);
    assert_eq!(vm.state.devices.pic.isr, 0);
    assert!(!vm.native.interruptible());
    assert_eq!(vm.page.word(VIRTIO_IRQ + 16), 0);
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("virtio-handler");
    vm.capture(&path).unwrap();
    let mut restored = restore_machine(&inputs, read_snapshot(&path, &inputs).unwrap()).unwrap();
    for machine in [&mut vm, &mut restored] {
        // New arrivals cross the stopped masked handler, first with no budget.
        assert_eq!(
            machine
                .run(20, 20, 1000, false, &[], vec![vec![42; 60], vec![43; 60]])
                .unwrap(),
            Stop::Horizon
        );
        assert_eq!(machine.page.word(VIRTIO_IRQ + 16), 1);
        assert_eq!(machine.state.devices.lines & 0x80, 0x80);
        assert_eq!(
            machine.run(20, 21, 1000, false, &[], vec![]).unwrap(),
            Stop::PoweredOff
        );
        assert_eq!(machine.state.now, 20);
        assert!(!machine.state.net.as_ref().unwrap().irq());
        assert_eq!(machine.state.devices.pic.isr, 0);
        let ram = unsafe { machine.ram.bytes() };
        assert_eq!(
            u16::from_le_bytes(ram[0x4002002..0x4002004].try_into().unwrap()),
            3
        );
        for i in 0..3 {
            assert_eq!(
                &ram[0x400800c + i * 2048..0x4008048 + i * 2048],
                &[41 + i as u8; 60]
            );
        }
    }
    assert_eq!(vm.state.witness, restored.state.witness);
    assert_eq!(
        vm.native.capture().unwrap(),
        restored.native.capture().unwrap()
    );
    assert_eq!(unsafe { vm.page.bytes() }, unsafe { restored.page.bytes() });
}
