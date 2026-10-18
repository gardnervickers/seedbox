use super::*;
fn queue(ram: &mut [u8], writable: bool) -> Queue {
    ram[..8].copy_from_slice(&(BASE + 256).to_le_bytes());
    ram[8..12].copy_from_slice(&64u32.to_le_bytes());
    ram[12..14].copy_from_slice(&(if writable { 2u16 } else { 0 }).to_le_bytes());
    ram[66..68].copy_from_slice(&1u16.to_le_bytes());
    Queue {
        size: 8,
        ready: true,
        descriptor: BASE,
        available: BASE + 64,
        used: BASE + 128,
        cursor: 0,
    }
}
#[test]
fn advertises_one_port_in_standard_config_field() {
    let console = Console::default();
    assert_eq!(console.read(MAX_PORTS).unwrap(), 1);
    assert_eq!(console.read(EMERGENCY_WRITE).unwrap(), 0);
}
#[test]
fn pending_input_survives_snapshot_and_is_consumed_once() {
    let mut ram = vec![0; 512];
    let mut console = Console {
        status: STATUS_DRIVER_OK,
        open: true,
        ..Console::default()
    };
    console.shutdown();
    console.queues[0] = queue(&mut ram, true);
    let snapshot = serde_json::to_vec(&console).unwrap();
    let mut branch: Console = serde_json::from_slice(&snapshot).unwrap();
    branch.process(&mut ram, 0).unwrap();
    assert_eq!(&ram[256..265], b"shutdown\n");
    assert_eq!(u16_at(&ram, BASE + 130).unwrap(), 1);
    assert!(branch.data.is_empty());
    branch.process(&mut ram, 0).unwrap();
    assert_eq!(u16_at(&ram, BASE + 130).unwrap(), 1);
    assert!(!console.data.is_empty());
}
#[test]
fn tx_is_timestamped_and_survives_snapshot_without_repeating_consumed_ring() {
    let mut ram = vec![0; 512];
    let mut console = Console {
        status: STATUS_DRIVER_OK,
        ..Console::default()
    };
    console.queues[PORT_TX] = queue(&mut ram, false);
    ram[8..12].copy_from_slice(&3u32.to_le_bytes());
    ram[256..259].copy_from_slice(b"raw");
    console.process(&mut ram, 17).unwrap();
    let image = serde_json::to_vec(&console).unwrap();
    let mut restored: Console = serde_json::from_slice(&image).unwrap();
    restored.process(&mut ram, 20).unwrap();
    let writes = restored.output.drain();
    assert_eq!(writes.len(), 1);
    assert_eq!(writes[0].time, 17);
    assert_eq!(writes[0].bytes, b"raw");
    assert!(restored.output.drain().is_empty());
    let image = serde_json::to_vec(&restored).unwrap();
    let mut restored: Console = serde_json::from_slice(&image).unwrap();
    restored.process(&mut ram, 30).unwrap();
    assert!(restored.output.drain().is_empty());
    assert_eq!(console.output.drain(), writes);
}
#[test]
fn fragmented_input_preserves_backpressured_suffix_across_snapshot() {
    let mut ram = vec![0; 512];
    let mut console = Console {
        status: STATUS_DRIVER_OK,
        open: true,
        ..Console::default()
    };
    console.queues[PORT_RX] = queue(&mut ram, true);
    ram[8..12].copy_from_slice(&3u32.to_le_bytes());
    console.input(b"ab");
    console.input(b"cde");
    console.process(&mut ram, 11).unwrap();
    assert_eq!(&ram[256..259], b"abc");
    assert_eq!(console.data.iter().copied().collect::<Vec<_>>(), b"de");
    let image = serde_json::to_vec(&console).unwrap();
    let mut restored: Console = serde_json::from_slice(&image).unwrap();
    restored.process(&mut ram, 12).unwrap();
    assert_eq!(restored.data.len(), 2);
    restored.input(b"fg");
    ram[16..24].copy_from_slice(&(BASE + 272).to_le_bytes());
    ram[24..28].copy_from_slice(&4u32.to_le_bytes());
    ram[28..30].copy_from_slice(&DESCRIPTOR_WRITE.to_le_bytes());
    ram[66..68].copy_from_slice(&2u16.to_le_bytes());
    ram[70..72].copy_from_slice(&1u16.to_le_bytes());
    restored.process(&mut ram, 13).unwrap();
    assert_eq!(&ram[272..276], b"defg");
    assert!(restored.data.is_empty());
    let image = serde_json::to_vec(&restored).unwrap();
    let mut restored: Console = serde_json::from_slice(&image).unwrap();
    restored.process(&mut ram, 14).unwrap();
    assert!(restored.data.is_empty());
    assert_eq!(u16_at(&ram, BASE + 130).unwrap(), 2);
    assert_eq!(console.data.iter().copied().collect::<Vec<_>>(), b"de");
}
#[test]
fn malformed_rings_reject_before_dma() {
    let mut ram = vec![0; 512];
    let mut q = queue(&mut ram, true);
    ram[12..14].copy_from_slice(&3u16.to_le_bytes());
    ram[14..16].copy_from_slice(&0u16.to_le_bytes());
    assert!(q.take(&ram, true).is_err());
    q.available = u64::MAX;
    assert!(q.take(&ram, true).is_err());
}
