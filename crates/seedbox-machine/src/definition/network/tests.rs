use super::*;

#[test]
fn mac_addresses_must_be_unicast_and_nonzero() {
    assert!(MacAddress::new([0; 6]).is_err());
    assert!(MacAddress::new([0xff; 6]).is_err());
    assert!(MacAddress::new([0x01, 0, 0x5e, 0, 0, 1]).is_err());
    let mac = MacAddress::new([0x52, 0x54, 0, 0, 0, 1]).unwrap();
    assert_eq!(mac.octets(), [0x52, 0x54, 0, 0, 0, 1]);
    assert_eq!(mac.to_string(), "52:54:00:00:00:01");
    // Locally administered unicast is allowed.
    assert!(MacAddress::new([0x02, 0, 0, 0, 0, 0]).is_ok());
}

#[test]
fn prefix_lengths_cover_zero_through_thirty_two() {
    let mac = MacAddress::new([2, 0, 0, 0, 0, 1]).unwrap();
    let address = Ipv4Addr::new(10, 0, 0, 1);
    for prefix in [0, 24, 32] {
        let interface = NetworkInterface::new(mac, address, prefix).unwrap();
        assert_eq!(
            (interface.mac(), interface.ipv4(), interface.prefix_len()),
            (mac, address, prefix)
        );
    }
    assert!(NetworkInterface::new(mac, address, 33).is_err());
}
