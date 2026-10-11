//! Deterministic Ethernet/IPv4/UDP DNS answers for the virtual nameserver.

pub const IP: [u8; 4] = [10, 0, 1, 1];
pub const MAC: [u8; 6] = [0x02, 0x53, 0x42, 0x58, 0x00, 0x01];
pub const TTL_SECONDS: u32 = 5;

/// Respond to Ethernet/IPv4 ARP who-has requests for the virtual nameserver.
pub fn arp_reply(frame: &[u8]) -> Option<Vec<u8>> {
    if frame.len() < 42
        || frame[12..14] != [0x08, 0x06]
        || frame[14..22] != [0, 1, 8, 0, 6, 4, 0, 1]
        || frame[38..42] != IP
    {
        return None;
    }
    let mut out = Vec::with_capacity(42);
    out.extend_from_slice(&frame[22..28]);
    out.extend_from_slice(&MAC);
    out.extend_from_slice(&[8, 6, 0, 1, 8, 0, 6, 4, 0, 2]);
    out.extend_from_slice(&MAC);
    out.extend_from_slice(&IP);
    out.extend_from_slice(&frame[22..28]);
    out.extend_from_slice(&frame[28..32]);
    Some(out)
}

/// Reply to a single-question A/IN DNS query sent to the virtual nameserver.
/// Unknown names receive NXDOMAIN; unrelated or malformed packets are ignored.
/// The caller owns name-to-address policy and the response's delivery time.
pub fn reply(frame: &[u8], records: impl Fn(&str) -> Option<[u8; 4]>) -> Option<Vec<u8>> {
    if frame.len() < 14 + 20 + 8 + 12 || frame[..6] != MAC || frame[12..14] != [0x08, 0x00] {
        return None;
    }
    let ip = &frame[14..];
    let header_len = usize::from(ip[0] & 15) * 4;
    if ip[0] >> 4 != 4
        || header_len < 20
        || ip.len() < header_len
        || ip[9] != 17
        || ip[6] & 0x3f != 0
        || ip[7] != 0
        || ip[16..20] != IP
    {
        return None;
    }
    let ip_len = usize::from(u16::from_be_bytes([ip[2], ip[3]]));
    if ip_len < header_len + 8 + 12 || ip_len > ip.len() {
        return None;
    }
    let udp = &ip[header_len..ip_len];
    let udp_len = usize::from(u16::from_be_bytes([udp[4], udp[5]]));
    if udp[2..4] != [0, 53] || udp_len < 20 || udp_len > udp.len() {
        return None;
    }
    let query = &udp[8..udp_len];
    let flags = u16::from_be_bytes([query[2], query[3]]);
    if flags & 0xf800 != 0 || query[4..6] != [0, 1] || query[6..12] != [0; 6] {
        return None;
    }
    let mut pos = 12;
    let mut name = String::new();
    loop {
        let len = usize::from(*query.get(pos)?);
        pos += 1;
        if len == 0 {
            break;
        }
        // Queries need not use compression; rejecting pointers also avoids loops.
        if len > 63 || pos + len > query.len() || pos + len + 5 > query.len() {
            return None;
        }
        let label = &query[pos..pos + len];
        if !label.iter().all(u8::is_ascii) || name.len() + len + 1 > 253 {
            return None;
        }
        if !name.is_empty() {
            name.push('.');
        }
        name.push_str(&String::from_utf8_lossy(label).to_ascii_lowercase());
        pos += len;
    }
    if pos + 4 != query.len() || query[pos + 2..pos + 4] != [0, 1] {
        return None;
    }
    let kind = u16::from_be_bytes([query[pos], query[pos + 1]]);
    if kind != 1 && kind != 28 {
        return None;
    }
    let found = (kind == 1).then(|| records(&name)).flatten();
    let unknown = kind == 1 && found.is_none();
    let mut answer = Vec::with_capacity(query.len() + 16);
    answer.extend_from_slice(&query[..2]);
    answer.extend_from_slice(
        &(0x8000u16 | (flags & 0x0100) | 0x0400 | (u16::from(unknown) * 3)).to_be_bytes(),
    );
    answer.extend_from_slice(&1u16.to_be_bytes());
    answer.extend_from_slice(&u16::from(found.is_some()).to_be_bytes());
    answer.extend_from_slice(&[0; 4]);
    answer.extend_from_slice(&query[12..]);
    if let Some(address) = found {
        answer.extend_from_slice(&[0xc0, 0x0c, 0, 1, 0, 1]);
        answer.extend_from_slice(&TTL_SECONDS.to_be_bytes());
        answer.extend_from_slice(&4u16.to_be_bytes());
        answer.extend_from_slice(&address);
    }
    let wire_len = 20 + 8 + answer.len();
    let mut out = Vec::with_capacity(14 + wire_len);
    out.extend_from_slice(&frame[6..12]);
    out.extend_from_slice(&MAC);
    out.extend_from_slice(&[0x08, 0x00]);
    let mut header = [0u8; 20];
    header[0] = 0x45;
    header[2..4].copy_from_slice(&u16::try_from(wire_len).ok()?.to_be_bytes());
    header[8] = 64;
    header[9] = 17;
    header[12..16].copy_from_slice(&IP);
    header[16..20].copy_from_slice(&ip[12..16]);
    let sum = checksum(&header);
    header[10..12].copy_from_slice(&sum.to_be_bytes());
    out.extend_from_slice(&header);
    let mut reply_udp = Vec::with_capacity(8 + answer.len());
    reply_udp.extend_from_slice(&53u16.to_be_bytes());
    reply_udp.extend_from_slice(&udp[0..2]);
    reply_udp.extend_from_slice(&u16::try_from(8 + answer.len()).ok()?.to_be_bytes());
    reply_udp.extend_from_slice(&[0; 2]);
    reply_udp.extend_from_slice(&answer);
    let mut pseudo = Vec::with_capacity(12 + reply_udp.len() + 1);
    pseudo.extend_from_slice(&IP);
    pseudo.extend_from_slice(&ip[12..16]);
    pseudo.extend_from_slice(&[0, 17]);
    pseudo.extend_from_slice(&u16::try_from(reply_udp.len()).ok()?.to_be_bytes());
    pseudo.extend_from_slice(&reply_udp);
    let sum = checksum(&pseudo);
    reply_udp[6..8].copy_from_slice(&(if sum == 0 { 0xffff } else { sum }).to_be_bytes());
    out.extend_from_slice(&reply_udp);
    Some(out)
}

fn checksum(data: &[u8]) -> u16 {
    let (pairs, remainder) = data.as_chunks::<2>();
    let mut sum: u32 = pairs
        .iter()
        .map(|pair| u32::from(u16::from_be_bytes(*pair)))
        .sum();
    if let Some(&last) = remainder.first() {
        sum += u32::from(last) << 8;
    }
    while sum >> 16 != 0 {
        sum = (sum & 0xffff) + (sum >> 16);
    }
    !(sum as u16)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(name: &[u8]) -> Vec<u8> {
        let mut dns = vec![0x12, 0x34, 1, 0, 0, 1, 0, 0, 0, 0, 0, 0];
        for label in name.split(|&b| b == b'.') {
            dns.push(label.len() as u8);
            dns.extend_from_slice(label);
        }
        dns.extend_from_slice(&[0, 0, 1, 0, 1]);
        let ip_len = 20 + 8 + dns.len();
        let mut out = Vec::from(MAC);
        out.extend_from_slice(&[2, 0, 0, 0, 0, 2, 8, 0]);
        let mut ip = [0u8; 20];
        ip[0] = 0x45;
        ip[2..4].copy_from_slice(&(ip_len as u16).to_be_bytes());
        ip[8] = 64;
        ip[9] = 17;
        ip[12..16].copy_from_slice(&[10, 0, 0, 2]);
        ip[16..20].copy_from_slice(&IP);
        out.extend_from_slice(&ip);
        out.extend_from_slice(&[0xab, 0xcd, 0, 53]);
        out.extend_from_slice(&((dns.len() + 8) as u16).to_be_bytes());
        out.extend_from_slice(&[0, 0]);
        out.extend_from_slice(&dns);
        out
    }

    #[test]
    fn answer_and_negative_answer_have_valid_checksums() {
        let query = request(b"Node");
        let reply = reply(&query, |name| (name == "node").then_some([10, 0, 0, 3])).unwrap();
        assert_eq!(&reply[..6], &[2, 0, 0, 0, 0, 2]);
        assert_eq!(&reply[6..12], &MAC);
        assert_eq!(&reply[26..30], &IP);
        assert_eq!(&reply[30..34], &[10, 0, 0, 2]);
        assert_eq!(checksum(&reply[14..34]), 0);
        let mut pseudo = Vec::from(&reply[26..34]);
        pseudo.extend_from_slice(&[0, 17]);
        pseudo.extend_from_slice(&reply[38..40]);
        pseudo.extend_from_slice(&reply[34..]);
        assert_eq!(checksum(&pseudo), 0);
        assert_eq!(&reply[42..44], &[0x12, 0x34]);
        assert_eq!(&reply[48..50], &[0, 1]);
        assert_eq!(&reply[reply.len() - 4..], &[10, 0, 0, 3]);
        let negative = super::reply(&query, |_| None).unwrap();
        assert_eq!(&negative[44..46], &[0x85, 0x03]);
        assert_eq!(&negative[48..50], &[0, 0]);
    }

    #[test]
    fn arp_for_virtual_address_only() {
        let mut request = Vec::from([0xff; 6]);
        request.extend_from_slice(&[2, 0, 0, 0, 0, 2]);
        request.extend_from_slice(&[8, 6, 0, 1, 8, 0, 6, 4, 0, 1]);
        request.extend_from_slice(&[2, 0, 0, 0, 0, 2]);
        request.extend_from_slice(&[10, 0, 0, 2]);
        request.extend_from_slice(&[0; 6]);
        request.extend_from_slice(&IP);
        let answer = arp_reply(&request).unwrap();
        assert_eq!(&answer[..6], &[2, 0, 0, 0, 0, 2]);
        assert_eq!(&answer[6..12], &MAC);
        assert_eq!(&answer[20..22], &[0, 2]);
        assert_eq!(&answer[22..28], &MAC);
        assert_eq!(&answer[28..32], &IP);
        request[41] = 2;
        assert!(arp_reply(&request).is_none());
    }

    #[test]
    fn aaaa_known_and_unknown_names_have_empty_success_answers() {
        let mut query = request(b"node");
        let kind = query.len() - 4;
        query[kind + 1] = 28;
        for address in [Some([10, 0, 0, 3]), None] {
            let answer = reply(&query, |_| address).unwrap();
            assert_eq!(&answer[44..46], &[0x85, 0]);
            assert_eq!(&answer[48..50], &[0, 0]);
        }
    }

    #[test]
    fn ignores_unrelated_and_malformed_packets() {
        let mut query = request(b"node");
        query[30] = 2;
        assert!(reply(&query, |_| Some([10, 0, 0, 3])).is_none());
        let mut query = request(b"node");
        query[42 + 12] = 0xc0;
        assert!(reply(&query, |_| Some([10, 0, 0, 3])).is_none());
        let query = request(b"node");
        assert!(reply(&query[..query.len() - 2], |_| None).is_none());
    }
}
