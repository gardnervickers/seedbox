use super::*;
#[test]
fn kernel_arguments_are_preserved_and_capacity_is_checked() {
    let arguments = "rdinit=/init custom=value console=other console=last";
    let bytes = build(0x42000000, 512, 7, arguments, false, false).unwrap();
    let mut expected = arguments.as_bytes().to_vec();
    expected.push(0);
    assert!(bytes
        .windows(expected.len())
        .any(|window| window == expected));
    assert!(!bytes
        .windows(b"earlycon=pl011".len())
        .any(|window| window == b"earlycon=pl011"));
    assert!(build(0x42000000, 512, 7, &"x".repeat(65536), false, false).is_err());
}
#[test]
fn only_a_boot_with_a_disk_describes_its_block_device() {
    let node = b"virtio@b020000\0";
    let has = |bytes: &[u8]| bytes.windows(node.len()).any(|window| window == node);
    let diskless = build(0x42000000, 512, 7, "", false, false).unwrap();
    let disk = build(0x42000000, 512, 7, "", true, false).unwrap();
    assert!(!has(&diskless));
    assert!(has(&disk));
    assert!(disk.len() > diskless.len());
}
#[test]
fn only_a_boot_with_a_nic_describes_its_network_device() {
    let node = b"virtio@b030000\0";
    let has = |bytes: &[u8]| bytes.windows(node.len()).any(|window| window == node);
    assert!(!has(&build(0x42000000, 512, 7, "", true, false).unwrap()));
    let both = build(0x42000000, 512, 7, "", true, true).unwrap();
    assert!(has(&both));
    assert!(has(&build(0x42000000, 512, 7, "", false, true).unwrap()));
    // SPI 12 (GIC interrupt 44) follows the console's 10 and the disk's 11.
    let at = both.windows(node.len()).position(|w| w == node).unwrap();
    let spi = [0u32, 12, 4]
        .iter()
        .flat_map(|cell| cell.to_be_bytes())
        .collect::<Vec<_>>();
    assert!(both[at..].windows(spi.len()).any(|w| w == spi));
}
