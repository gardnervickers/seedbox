//! PC-device output retention and virtio-net receive checks, without KVM.
use crate::{devices, virtio_net};

#[test]
fn uart_output_is_persisted_before_completion_and_limited_without_truncation() {
    let path = std::env::temp_dir().join(format!("seedbox-uart-retention-{}", std::process::id()));
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
    std::fs::remove_file(&path).unwrap();
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

#[test]
fn live_uart_discards_bytes_without_changing_registers_or_transmission_observation() {
    let mut retained = devices::Uart::default();
    let mut live = devices::Uart::default();
    live.discard_output = true;
    live.output_limit = Some(0);
    for (register, value) in [(3, 0x80), (0, 12), (3, 0), (1, 2), (0, 0), (0, 255)] {
        retained.write(register, value).unwrap();
        live.write(register, value).unwrap();
        assert_eq!(retained.transmitted, live.transmitted);
        assert_eq!(retained.edge(), live.edge());
        for register in 0..8 {
            assert_eq!(retained.read(register), live.read(register));
        }
    }
    assert_eq!(live.transmitted, 2);
    assert!(live.out.is_empty());
    assert_eq!(retained.out, [0, 255]);
    let snapshot = live.snapshot();
    live.transmit(b'x').unwrap();
    live.restore(&snapshot);
    assert_eq!(live.transmitted, 2);
    assert!(live.discard_output && live.out.is_empty());
}

fn rx_device(available: bool) -> (virtio_net::VirtioNet, Vec<u8>) {
    let mut net = virtio_net::VirtioNet::new([2, 0, 0, 0, 0, 1]);
    let mut memory = vec![0; 4096];
    // One writable receive descriptor and its split avail/used rings.
    memory[0x100..0x108].copy_from_slice(&0x400u64.to_le_bytes());
    memory[0x108..0x10c].copy_from_slice(&128u32.to_le_bytes());
    memory[0x10c..0x10e].copy_from_slice(&2u16.to_le_bytes());
    memory[0x202..0x204].copy_from_slice(&u16::from(available).to_le_bytes());
    for (offset, value) in [
        (0x024, 1),
        (0x020, 1),
        (0x070, 15),
        (0x030, 0),
        (0x038, 1),
        (0x080, 0x100),
        (0x090, 0x200),
        (0x0a0, 0x300),
        (0x044, 1),
    ] {
        net.write(&mut memory, offset, value).unwrap();
    }
    (net, memory)
}

#[test]
fn receiver_not_ready_drops_and_ready_dma_reports_acceptance_with_irq() {
    use virtio_net::{ReceiveOutcome, VirtioNet};
    let mut net = VirtioNet::new([2, 0, 0, 0, 0, 1]);
    assert_eq!(
        net.receive(&mut [], b"frame").unwrap(),
        ReceiveOutcome {
            accepted: false,
            irq: false
        }
    );
    assert_eq!(net.stats().rx_dropped, 1);
    let (mut net, mut memory) = rx_device(true);
    assert_eq!(
        net.receive(&mut memory, b"frame").unwrap(),
        ReceiveOutcome {
            accepted: true,
            irq: true
        }
    );
    assert_eq!(net.stats().rx_frames, 1);
    assert_eq!(net.stats().rx_dropped, 0);
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
            net.receive(&mut memory, b"queued").unwrap(),
            ReceiveOutcome {
                accepted: true,
                irq: false
            }
        );
    }
    assert_eq!(net.stats().rx_frames, 0);
    assert_eq!(net.stats().rx_dropped, 0);
    assert_eq!(
        net.receive(&mut memory, b"overflow").unwrap(),
        ReceiveOutcome {
            accepted: false,
            irq: false
        }
    );
    assert_eq!(net.stats().rx_dropped, 1);
    // Publishing one real guest descriptor drains an accepted queued frame.
    memory[0x202..0x204].copy_from_slice(&1u16.to_le_bytes());
    assert!(net.write(&mut memory, 0x050, 0).unwrap().irq);
    assert_eq!(net.stats().rx_frames, 1);
    assert_eq!(&memory[0x40c..0x412], b"queued");
    assert_eq!(net.stats().rx_dropped, 1);
    assert_eq!(
        net.receive(&mut memory, b"replacement").unwrap(),
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
    let mut packet = vec![0; 12]; // No offload features were negotiated.
    packet.extend_from_slice(frame);
    // Follow descriptor links 2 -> 0 -> 3, not descriptor or guest-address order.
    // The header itself crosses a descriptor boundary.
    tx_descriptor(&mut memory, 2, 0x700, &packet[..8], Some(0));
    tx_descriptor(&mut memory, 0, 0x500, &packet[8..19], Some(3));
    tx_descriptor(&mut memory, 3, 0x600, &packet[19..], None);
    let mut packet_two = vec![0; 12];
    packet_two.extend_from_slice(second);
    tx_descriptor(&mut memory, 1, 0x800, &packet_two, None);
    memory[0x204..0x206].copy_from_slice(&2u16.to_le_bytes());
    memory[0x206..0x208].copy_from_slice(&1u16.to_le_bytes());
    for (offset, value) in [
        (0x024, 1),
        (0x020, 1),
        (0x070, 15),
        (0x030, 1),
        (0x038, 4),
        (0x080, 0x100),
        (0x090, 0x200),
        (0x0a0, 0x300),
        (0x044, 1),
    ] {
        net.write(&mut memory, offset, value).unwrap();
    }
    let empty = net.write(&mut memory, 0x050, 1).unwrap();
    assert!(empty.frames.is_empty());
    assert!(!empty.irq);
    assert!(net.dirty.is_empty());
    memory[0x202..0x204].copy_from_slice(&2u16.to_le_bytes());
    let output = net.write(&mut memory, 0x050, 1).unwrap();
    assert_eq!(output.frames, vec![frame.to_vec(), second.to_vec()]);
    assert!(output.irq);
    assert_eq!(net.read(0x060), 1);
    assert_eq!(net.stats().tx_frames, 2);
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
        vec![
            (0x300, 2),
            (0x304, 8),
            (0x302, 2),
            (0x30c, 8),
            (0x302, 2),
            (0x300, 2)
        ]
    );
    // The returned frames own their bytes; subsequent guest writes cannot change them.
    memory[0x500..0x900].fill(0);
    assert_eq!(output.frames, vec![frame.to_vec(), second.to_vec()]);
    net.dirty.clear();
    net.write(&mut memory, 0x064, 1).unwrap();
    let duplicate = net.write(&mut memory, 0x050, 1).unwrap();
    assert!(duplicate.frames.is_empty());
    assert!(!duplicate.irq);
    assert_eq!(net.read(0x060), 0);
    assert_eq!(net.stats().tx_frames, 2);
    assert!(net.dirty.is_empty());
}

#[test]
fn malformed_receive_chains_fail_without_consuming_or_writing() {
    if std::env::var_os("SEEDBOX_NET_HOSTILE_CHILD").is_none() {
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "device_tests::malformed_receive_chains_fail_without_consuming_or_writing",
                "--nocapture",
            ])
            .env("SEEDBOX_NET_HOSTILE_CHILD", "1")
            .spawn()
            .unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                assert!(status.success());
                return;
            }
            if std::time::Instant::now() >= deadline {
                child.kill().unwrap();
                child.wait().unwrap();
                panic!("malformed network chain did not terminate within ten seconds");
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }
    for case in 0..9 {
        let (mut net, mut memory) = rx_device(true);
        match case {
            0 => memory[0x108..0x10c].copy_from_slice(&4096u32.to_le_bytes()), // span outside RAM
            1 => memory[0x10c..0x10e].copy_from_slice(&3u16.to_le_bytes()),    // cycle
            2 => memory[0x204..0x206].copy_from_slice(&1u16.to_le_bytes()),    // head
            3 => memory[0x202..0x204].copy_from_slice(&2u16.to_le_bytes()),    // overrun
            4 => memory[0x100..0x108].copy_from_slice(&u64::MAX.to_le_bytes()),
            5 => memory[0x10c..0x10e].copy_from_slice(&6u16.to_le_bytes()),
            6 => memory[0x10c..0x10e].copy_from_slice(&0u16.to_le_bytes()),
            7 => {
                assert!(net.write(&mut memory, 0x038, 3).is_err());
                continue;
            }
            8 => {
                net.write(&mut memory, 0x044, 0).unwrap();
                net.write(&mut memory, 0x0a0, 4092).unwrap();
                net.write(&mut memory, 0x044, 1).unwrap();
            }
            _ => unreachable!(),
        }
        let before = memory.clone();
        assert!(net.receive(&mut memory, b"frame").is_err(), "case {case}");
        assert_eq!(memory, before, "case {case}");
        assert!(net.dirty.is_empty());
        assert_eq!(
            (
                net.stats().rx_frames,
                net.stats().rx_dropped,
                net.read(0x060)
            ),
            (0, 0, 0)
        );
        // Repair the malformed request in place. Exactly one successful completion
        // proves that the rejected head did not advance the device cursor.
        memory[0x100..0x108].copy_from_slice(&0x400u64.to_le_bytes());
        memory[0x108..0x10c].copy_from_slice(&128u32.to_le_bytes());
        memory[0x10c..0x10e].copy_from_slice(&2u16.to_le_bytes());
        memory[0x202..0x204].copy_from_slice(&1u16.to_le_bytes());
        memory[0x204..0x206].fill(0);
        net.write(&mut memory, 0x044, 0).unwrap();
        net.write(&mut memory, 0x038, 1).unwrap();
        net.write(&mut memory, 0x0a0, 0x300).unwrap();
        net.write(&mut memory, 0x044, 1).unwrap();
        assert!(net.write(&mut memory, 0x050, 0).unwrap().irq);
        assert_eq!(net.stats().rx_frames, 1);
        assert_eq!(&memory[0x302..0x304], &[1, 0]);
    }
}

#[test]
fn receive_drain_keeps_completed_prefix_and_failed_backlog_head() {
    let (mut net, mut memory) = rx_device(false);
    net.write(&mut memory, 0x044, 0).unwrap();
    net.write(&mut memory, 0x038, 2).unwrap();
    net.write(&mut memory, 0x044, 1).unwrap();
    assert!(net.receive(&mut memory, b"first").unwrap().accepted);
    assert!(net.receive(&mut memory, b"second").unwrap().accepted);
    memory[0x110..0x118].copy_from_slice(&u64::MAX.to_le_bytes());
    memory[0x118..0x11c].copy_from_slice(&12u32.to_le_bytes());
    memory[0x11c..0x11e].copy_from_slice(&2u16.to_le_bytes());
    memory[0x202..0x204].copy_from_slice(&2u16.to_le_bytes());
    memory[0x206..0x208].copy_from_slice(&1u16.to_le_bytes());
    assert!(net.write(&mut memory, 0x050, 0).is_err());
    assert_eq!(net.stats().rx_frames, 1);
    assert_eq!(net.read(0x060), 1);
    assert_eq!(&memory[0x40c..0x411], b"first");
    assert_eq!(&memory[0x500..0x520], &[0; 32]);
    assert_eq!(&memory[0x302..0x304], &[1, 0]);
    assert_eq!(
        net.dirty,
        vec![(0x300, 2), (0x400, 17), (0x304, 8), (0x302, 2)]
    );
    memory[0x110..0x118].copy_from_slice(&0x500u64.to_le_bytes());
    memory[0x118..0x11c].copy_from_slice(&128u32.to_le_bytes());
    assert!(net.write(&mut memory, 0x050, 0).unwrap().irq);
    assert_eq!(net.stats().rx_frames, 2);
    assert_eq!(&memory[0x50c..0x512], b"second");
    assert_eq!(&memory[0x302..0x304], &[2, 0]);
}

#[test]
fn transmit_late_error_preserves_completed_dma_and_retries_failed_head() {
    let (mut net, mut memory) = rx_device(false);
    for (offset, value) in [
        (0x030, 1),
        (0x038, 2),
        (0x080, 0x100),
        (0x090, 0x200),
        (0x0a0, 0x300),
        (0x044, 1),
    ] {
        net.write(&mut memory, offset, value).unwrap();
    }
    tx_descriptor(&mut memory, 0, 0x400, &[0; 13], None);
    tx_descriptor(&mut memory, 1, 0x500, &[0; 14], Some(1));
    memory[0x202..0x204].copy_from_slice(&2u16.to_le_bytes());
    memory[0x206..0x208].copy_from_slice(&1u16.to_le_bytes());
    assert!(net.write(&mut memory, 0x050, 1).is_err());
    assert_eq!(net.stats().tx_frames, 1);
    assert_eq!(net.read(0x060), 1);
    assert_eq!(net.dirty, vec![(0x300, 2), (0x304, 8), (0x302, 2)]);
    memory[0x11c..0x11e].fill(0);
    let output = net.write(&mut memory, 0x050, 1).unwrap();
    assert_eq!(output.frames, vec![vec![0; 2]]);
    assert_eq!(net.stats().tx_frames, 2);
    assert_eq!(&memory[0x302..0x304], &[2, 0]);
}

#[test]
fn receive_frame_limit_is_checked_before_backlog_admission() {
    let (mut net, mut memory) = rx_device(false);
    let frame = vec![0x5a; 65536];
    assert!(net.receive(&mut memory, &frame).unwrap().accepted);
    let oversized = vec![0; frame.len() + 1];
    assert!(net.receive(&mut memory, &oversized).is_err());
    assert_eq!(net.stats().rx_dropped, 0);
    // The maximum accepted frame fits exactly in a maximum-size chain.
    memory.resize(0x400 + 12 + frame.len(), 0);
    memory[0x108..0x10c].copy_from_slice(&((12 + frame.len()) as u32).to_le_bytes());
    memory[0x202..0x204].copy_from_slice(&1u16.to_le_bytes());
    assert!(net.write(&mut memory, 0x050, 0).unwrap().irq);
    assert_eq!(&memory[0x40c..], frame);
    assert_eq!(net.stats().rx_frames, 1);
}

#[test]
fn transmit_empty_short_and_limit_boundaries_are_explicit() {
    for size in [0, 11, 12, 13, 4 << 20, (4 << 20) + 1] {
        let (mut net, mut memory) = rx_device(false);
        memory.resize(0x400 + size, 0);
        for (offset, value) in [
            (0x030, 1),
            (0x038, 1),
            (0x080, 0x100),
            (0x090, 0x200),
            (0x0a0, 0x300),
            (0x044, 1),
        ] {
            net.write(&mut memory, offset, value).unwrap();
        }
        let mut packet = vec![0x5a; size];
        packet[..size.min(12)].fill(0);
        tx_descriptor(&mut memory, 0, 0x400, &packet, None);
        memory[0x202..0x204].copy_from_slice(&1u16.to_le_bytes());
        let output = net.write(&mut memory, 0x050, 1);
        if !(12..=4 << 20).contains(&size) {
            assert!(output.is_err());
            // The legacy drain suppresses guest kicks before validating the
            // request; that flag write remains journaled when DMA validation fails.
            assert_eq!(net.dirty, [(0x300, 2)]);
            assert_eq!(&memory[0x300..0x302], &[1, 0]);
            assert_eq!(net.stats().tx_frames, 0);
            assert_eq!(&memory[0x302..0x304], &[0, 0]);
        } else {
            let output = output.unwrap();
            assert!(output.irq);
            assert_eq!(output.frames.len(), usize::from(size > 12));
            if size > 12 {
                assert_eq!(output.frames[0], vec![0x5a; size - 12]);
            }
            assert_eq!(&memory[0x302..0x304], &[1, 0]);
        }
    }
}

#[test]
fn network_reset_preserves_dirty_journal_but_clears_backlog_and_counters() {
    let (mut net, mut memory) = rx_device(true);
    assert!(net.receive(&mut memory, b"completed").unwrap().irq);
    assert!(net.receive(&mut memory, b"queued").unwrap().accepted);
    let dirty = net.dirty.clone();
    let mut snapshot = net.clone();
    net.write(&mut memory, 0x070, 0).unwrap();
    assert_eq!(net.dirty, dirty);
    assert_eq!(
        (
            net.stats().rx_frames,
            net.stats().rx_dropped,
            net.stats().tx_frames,
            net.read(0x060)
        ),
        (0, 0, 0, 0)
    );
    // The snapshot owns its pending backlog; reset did not mutate it.
    memory[0x202..0x204].copy_from_slice(&2u16.to_le_bytes());
    assert!(snapshot.write(&mut memory, 0x050, 0).unwrap().irq);
    assert_eq!(&memory[0x40c..0x412], b"queued");
    assert_eq!(snapshot.stats().rx_frames, 2);
    assert!(!net.receive(&mut memory, b"not ready").unwrap().accepted);
    assert_eq!(net.stats().rx_dropped, 1);
}
