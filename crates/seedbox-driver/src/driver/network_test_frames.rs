//! Ethernet/IPv4/TCP data for host-only policy mocks; never guest code.
pub(crate) fn mac(index: usize) -> [u8; 6] {
    [2, 0, 0, 0, 0, (index + 1) as u8]
}
pub(crate) fn plain(from: usize, to: usize, tag: u8) -> Vec<u8> {
    let mut b = vec![tag; 60];
    b[..6].copy_from_slice(&mac(to));
    b[6..12].copy_from_slice(&mac(from));
    b[12..14].copy_from_slice(&[0x88, 0xb5]);
    b
}
pub(crate) fn tcp(
    from: usize,
    to: usize,
    ports: (u16, u16),
    sequence: u32,
    ack: u32,
    flags: u8,
) -> Vec<u8> {
    let mut b = vec![0; 60];
    b[..6].copy_from_slice(&mac(to));
    b[6..12].copy_from_slice(&mac(from));
    b[12..14].copy_from_slice(&[8, 0]);
    b[14] = 0x45;
    b[16..18].copy_from_slice(&40u16.to_be_bytes());
    b[22] = 64;
    b[23] = 6;
    b[26..30].copy_from_slice(&[10, 0, 0, (from + 1) as u8]);
    b[30..34].copy_from_slice(&[10, 0, 0, (to + 1) as u8]);
    b[34..36].copy_from_slice(&ports.0.to_be_bytes());
    b[36..38].copy_from_slice(&ports.1.to_be_bytes());
    b[38..42].copy_from_slice(&sequence.to_be_bytes());
    b[42..46].copy_from_slice(&ack.to_be_bytes());
    b[46] = 0x50;
    b[47] = flags;
    b
}
