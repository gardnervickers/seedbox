use super::super::{boot::RamLayout, memory::Memory};
use super::*;

fn device() -> Net {
    let mut net = Net::new([2, 0, 0, 0, 0, 1]);
    net.status = STATUS_DRIVER_OK;
    net.queues[TX] = Queue {
        size: 2,
        ready: true,
        descriptor: 0,
        available: 0x1000,
        used: 0x2000,
        cursor: 0,
    };
    net
}
fn describe(ram: &mut [u8], data: u64, length: u32) {
    ram[..8].copy_from_slice(&data.to_le_bytes());
    ram[8..12].copy_from_slice(&length.to_le_bytes());
    ram[0x1002..0x1004].copy_from_slice(&1u16.to_le_bytes());
}

#[test]
fn sparse_high_bank_dma_preserves_completion_and_irq_order() {
    // Anonymous address space only; no giant initialized Vec or native VM.
    let backing = Memory::ram(RamLayout::native(256 << 20).unwrap()).unwrap();
    let mut net = device();
    unsafe {
        backing.modify(|ram| {
            let address = 0x0b03c000usize;
            describe(ram, address as u64, (HEADER + 64) as u32);
            ram[address + HEADER..address + HEADER + 64].fill(7);
            assert!(!net.irq());
            net.write(ram, QUEUE_NOTIFY, 4, TX as u32, 9).unwrap();
            assert_eq!(
                net.take_transmitted(),
                vec![Frame {
                    time: 9,
                    bytes: vec![7; 64]
                }]
            );
            assert_eq!(net.queues[TX].cursor, 1);
            assert_eq!(
                u16::from_le_bytes(ram[0x2002..0x2004].try_into().unwrap()),
                1
            );
            assert!(net.irq());
        })
    };
}

#[test]
fn hole_descriptors_and_rings_fail_before_device_or_ram_mutation() {
    let backing = Memory::ram(RamLayout::native(256 << 20).unwrap()).unwrap();
    for kind in 0..5 {
        let mut net = device();
        unsafe {
            backing.modify(|ram| {
                ram[..0x4000].fill(0);
                describe(
                    ram,
                    if kind == 0 {
                        0x0a000000
                    } else if kind == 1 {
                        0x09fffff0
                    } else {
                        0x8000
                    },
                    64,
                );
                match kind {
                    2 => net.queues[TX].descriptor = 0x0a000000,
                    3 => net.queues[TX].available = 0x0a010000,
                    4 => net.queues[TX].used = 0x0b020000,
                    _ => (),
                }
                let before = serde_json::to_vec(&net).unwrap();
                let bytes = ram[..0x4000].to_vec();
                assert!(
                    net.write(ram, QUEUE_NOTIFY, 4, TX as u32, 9).is_err(),
                    "kind={kind}"
                );
                assert_eq!(serde_json::to_vec(&net).unwrap(), before, "kind={kind}");
                assert_eq!(&ram[..0x4000], bytes);
                assert!(!net.irq());
            })
        };
    }
}
