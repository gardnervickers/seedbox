// The PC-device model is portable. Exercise the actual VMM source here so these
// output-retention checks also run on Darwin without pretending to run KVM.
#[allow(dead_code)]
#[allow(clippy::manual_div_ceil)] // Existing PIT arithmetic is outside these UART tests.
#[rustfmt::skip]
#[path = "../../seedbox-vmm/src/devices.rs"]
mod devices;

#[test]
fn uart_output_is_persisted_before_completion_and_limited_without_truncation() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("console");
    let mut uart = devices::Uart::default();
    uart.output_file = Some(std::fs::File::create(&path).unwrap());
    uart.output_limit = Some(2);
    uart.write(0, b'O').unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), b"O");
    uart.transmit(b'K').unwrap();
    assert!(uart.write(0, b'!').is_err());
    assert_eq!(std::fs::read(&path).unwrap(), b"OK");
    assert_eq!(uart.out, b"OK");
    drop(uart);
    assert_eq!(std::fs::read(&path).unwrap(), b"OK");
}

#[test]
fn uart_baud_register_writes_do_not_enter_console_output() {
    let mut uart = devices::Uart::default();
    uart.write(3, 0x80).unwrap();
    uart.write(0, 12).unwrap();
    assert!(uart.out.is_empty());
    uart.write(3, 0).unwrap();
    uart.write(0, b'A').unwrap();
    assert_eq!(uart.out, b"A");
}

#[allow(dead_code)]
#[allow(clippy::manual_clamp)] // Existing queue-size arithmetic is outside receive coverage.
#[rustfmt::skip]
#[path = "../../seedbox-vmm/src/virtio_net.rs"]
mod virtio_net;

fn rx_device(available: bool) -> (virtio_net::VirtioNet, Vec<u8>) {
    let mut net = virtio_net::VirtioNet::new([2, 0, 0, 0, 0, 1]);
    let mut memory = vec![0; 4096];
    // One writable receive descriptor and its split avail/used rings.
    memory[0x100..0x108].copy_from_slice(&0x400u64.to_le_bytes());
    memory[0x108..0x10c].copy_from_slice(&128u32.to_le_bytes());
    memory[0x10c..0x10e].copy_from_slice(&2u16.to_le_bytes());
    memory[0x202..0x204].copy_from_slice(&u16::from(available).to_le_bytes());
    for (offset, value) in [
        (0x030, 0),
        (0x038, 1),
        (0x080, 0x100),
        (0x090, 0x200),
        (0x0a0, 0x300),
        (0x044, 1),
    ] {
        net.write(&mut memory, offset, value);
    }
    (net, memory)
}

#[test]
fn receiver_not_ready_drops_and_ready_dma_reports_acceptance_with_irq() {
    use virtio_net::{ReceiveOutcome, VirtioNet};
    let mut net = VirtioNet::new([2, 0, 0, 0, 0, 1]);
    assert_eq!(
        net.receive(&mut [], b"frame"),
        ReceiveOutcome {
            accepted: false,
            irq: false
        }
    );
    assert_eq!(net.rx_dropped, 1);
    let (mut net, mut memory) = rx_device(true);
    assert_eq!(
        net.receive(&mut memory, b"frame"),
        ReceiveOutcome {
            accepted: true,
            irq: true
        }
    );
    assert_eq!(net.rx_frames, 1);
    assert_eq!(net.rx_dropped, 0);
    assert_eq!(&memory[0x40c..0x411], b"frame");
    assert_eq!(
        u16::from_le_bytes(memory[0x302..0x304].try_into().unwrap()),
        1
    );
}

#[test]
fn accepted_backlog_has_no_irq_until_dma_and_full_backlog_drops() {
    use virtio_net::ReceiveOutcome;
    let (mut net, mut memory) = rx_device(false);
    for _ in 0..256 {
        assert_eq!(
            net.receive(&mut memory, b"queued"),
            ReceiveOutcome {
                accepted: true,
                irq: false
            }
        );
    }
    assert_eq!(net.rx_frames, 0);
    assert_eq!(net.rx_dropped, 0);
    assert_eq!(
        net.receive(&mut memory, b"overflow"),
        ReceiveOutcome {
            accepted: false,
            irq: false
        }
    );
    assert_eq!(net.rx_dropped, 1);
    // Publishing one real guest descriptor drains an accepted queued frame.
    memory[0x202..0x204].copy_from_slice(&1u16.to_le_bytes());
    assert!(net.write(&mut memory, 0x050, 0).irq);
    assert_eq!(net.rx_frames, 1);
    assert_eq!(&memory[0x40c..0x412], b"queued");
    assert_eq!(net.rx_dropped, 1);
    assert_eq!(
        net.receive(&mut memory, b"replacement"),
        ReceiveOutcome {
            accepted: true,
            irq: false
        }
    );
}

fn tx_descriptor(memory: &mut [u8], index: usize, address: u64, bytes: &[u8], next: Option<u16>) {
    let descriptor = 0x100 + 16 * index;
    memory[descriptor..descriptor + 8].copy_from_slice(&address.to_le_bytes());
    memory[descriptor + 8..descriptor + 12].copy_from_slice(&(bytes.len() as u32).to_le_bytes());
    memory[descriptor + 12..descriptor + 14]
        .copy_from_slice(&u16::from(next.is_some()).to_le_bytes());
    memory[descriptor + 14..descriptor + 16].copy_from_slice(&next.unwrap_or(0).to_le_bytes());
    let address = address as usize;
    memory[address..address + bytes.len()].copy_from_slice(bytes);
}

#[test]
fn tx_chains_preserve_opaque_bytes_and_complete_each_available_head_once() {
    let mut net = virtio_net::VirtioNet::new([2, 0, 0, 0, 0, 1]);
    let mut memory = vec![0; 4096];
    let frame = b"\xff\x00\x80\xfe\x01\x02\x00\x03\x04\x05\x06\x07\x88\xb5\x00\xff\n\r\x80payload";
    let second = b"\x00\xff\x80\x00second\n";
    let mut packet = vec![0x5a; 12]; // Nonzero virtio header must be stripped, not interpreted as payload.
    packet.extend_from_slice(frame);
    // Follow descriptor links 2 -> 0 -> 3, not descriptor or guest-address order.
    // The header itself crosses a descriptor boundary.
    tx_descriptor(&mut memory, 2, 0x700, &packet[..8], Some(0));
    tx_descriptor(&mut memory, 0, 0x500, &packet[8..19], Some(3));
    tx_descriptor(&mut memory, 3, 0x600, &packet[19..], None);
    let mut packet_two = vec![0xa5; 12];
    packet_two.extend_from_slice(second);
    tx_descriptor(&mut memory, 1, 0x800, &packet_two, None);
    memory[0x204..0x206].copy_from_slice(&2u16.to_le_bytes());
    memory[0x206..0x208].copy_from_slice(&1u16.to_le_bytes());
    for (offset, value) in [
        (0x030, 1),
        (0x038, 4),
        (0x080, 0x100),
        (0x090, 0x200),
        (0x0a0, 0x300),
        (0x044, 1),
    ] {
        net.write(&mut memory, offset, value);
    }
    let empty = net.write(&mut memory, 0x050, 1);
    assert!(empty.frames.is_empty());
    assert!(!empty.irq);
    assert!(net.dirty.is_empty());
    memory[0x202..0x204].copy_from_slice(&2u16.to_le_bytes());
    let output = net.write(&mut memory, 0x050, 1);
    assert_eq!(output.frames, vec![frame.to_vec(), second.to_vec()]);
    assert!(output.irq);
    assert_eq!(net.read(0x060), 1);
    assert_eq!(net.tx_frames, 2);
    assert_eq!(
        &memory[0x302..0x314],
        &[
            2, 0, // used index
            2, 0, 0, 0, 0, 0, 0, 0, // first head, length zero
            1, 0, 0, 0, 0, 0, 0, 0, // second head, length zero
        ]
    );
    assert_eq!(
        net.dirty,
        vec![(0x304, 8), (0x302, 2), (0x30c, 8), (0x302, 2)]
    );
    // The returned frames own their bytes; subsequent guest writes cannot change them.
    memory[0x500..0x900].fill(0);
    assert_eq!(output.frames, vec![frame.to_vec(), second.to_vec()]);
    net.dirty.clear();
    net.write(&mut memory, 0x064, 1);
    let duplicate = net.write(&mut memory, 0x050, 1);
    assert!(duplicate.frames.is_empty());
    assert!(!duplicate.irq);
    assert_eq!(net.read(0x060), 0);
    assert_eq!(net.tx_frames, 2);
    assert!(net.dirty.is_empty());
}
