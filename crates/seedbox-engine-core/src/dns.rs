//! The simulated nameserver's ARP and DNS answers, as whole Ethernet frames.
//! It keeps no state and draws no randomness: the caller supplies the current
//! members' names and addresses and schedules the reply.

/// The nameserver's IPv4 address, written to every guest's `/etc/resolv.conf`.
pub const ADDRESS: [u8; 4] = [10, 0, 1, 1];
/// A locally administered unicast MAC no machine interface uses.
pub const MAC: [u8; 6] = [0x02, 0x53, 0x42, 0x58, 0x00, 0x01];
/// Answers always reflect current membership, so guests must not cache them.
pub const TTL_SECONDS: u32 = 0;

const ARP: [u8; 2] = [0x08, 0x06];
const IPV4: [u8; 2] = [0x08, 0x00];
const UDP: u8 = 17;
const PORT: [u8; 2] = [0, 53];
const TYPE_A: u16 = 1;
const TYPE_PTR: u16 = 12;
const CLASS_IN: u16 = 1;
const FORMERR: u16 = 1;
const NXDOMAIN: u16 = 3;
const NOTIMP: u16 = 4;
const REFUSED: u16 = 5;

/// The reply to one frame a machine sent, or None for a frame the nameserver
/// does not answer: an ARP request for [`ADDRESS`] or a UDP DNS query to it.
/// Members' names answer A queries and their `in-addr.arpa` names answer PTR
/// queries; names are compared lowercase.
pub fn answer<'a, M>(frame: &[u8], members: M) -> Option<Vec<u8>>
where
    M: IntoIterator<Item = (&'a str, [u8; 4])>,
{
    if frame.len() < 14 {
        return None;
    }
    match [frame[12], frame[13]] {
        ARP => arp(frame),
        IPV4 if frame[..6] == MAC => udp(frame, members),
        _ => None,
    }
}

fn arp(frame: &[u8]) -> Option<Vec<u8>> {
    let request = frame.get(14..42)?;
    if request[..8] != [0, 1, 8, 0, 6, 4, 0, 1] || request[24..28] != ADDRESS {
        return None;
    }
    let (sender_mac, sender_ip) = (&request[8..14], &request[14..18]);
    let mut out = Vec::with_capacity(42);
    out.extend_from_slice(sender_mac);
    out.extend_from_slice(&MAC);
    out.extend_from_slice(&ARP);
    out.extend_from_slice(&[0, 1, 8, 0, 6, 4, 0, 2]);
    out.extend_from_slice(&MAC);
    out.extend_from_slice(&ADDRESS);
    out.extend_from_slice(sender_mac);
    out.extend_from_slice(sender_ip);
    Some(out)
}

fn udp<'a>(frame: &[u8], members: impl IntoIterator<Item = (&'a str, [u8; 4])>) -> Option<Vec<u8>> {
    let ip = &frame[14..];
    let header = usize::from(*ip.first()? & 15) * 4;
    if ip.len() < 20
        || ip[0] >> 4 != 4
        || header < 20
        || ip[9] != UDP
        // Fragments, including a first fragment with more to follow, are ignored.
        || ip[6] & 0x3f != 0
        || ip[7] != 0
        || ip[16..20] != ADDRESS
    {
        return None;
    }
    let total = usize::from(u16::from_be_bytes([ip[2], ip[3]]));
    let datagram = ip.get(header..total)?;
    let length = usize::from(u16::from_be_bytes([*datagram.get(4)?, *datagram.get(5)?]));
    if datagram[2..4] != PORT || length < 8 || length > datagram.len() {
        return None;
    }
    let message = response(&datagram[8..length], members)?;
    let source: [u8; 4] = ip[12..16].try_into().ok()?;
    let mut reply = Vec::with_capacity(8 + message.len());
    reply.extend_from_slice(&PORT);
    reply.extend_from_slice(&datagram[0..2]);
    reply.extend_from_slice(&u16::try_from(8 + message.len()).ok()?.to_be_bytes());
    reply.extend_from_slice(&[0, 0]);
    reply.extend_from_slice(&message);
    let mut pseudo = Vec::with_capacity(12 + reply.len());
    pseudo.extend_from_slice(&ADDRESS);
    pseudo.extend_from_slice(&source);
    pseudo.extend_from_slice(&[0, UDP]);
    pseudo.extend_from_slice(&u16::try_from(reply.len()).ok()?.to_be_bytes());
    pseudo.extend_from_slice(&reply);
    let sum = match checksum(&pseudo) {
        0 => 0xffff,
        sum => sum,
    };
    reply[6..8].copy_from_slice(&sum.to_be_bytes());
    let mut header = [0u8; 20];
    header[0] = 0x45;
    header[2..4].copy_from_slice(&u16::try_from(20 + reply.len()).ok()?.to_be_bytes());
    header[8] = 64;
    header[9] = UDP;
    header[12..16].copy_from_slice(&ADDRESS);
    header[16..20].copy_from_slice(&source);
    let sum = checksum(&header);
    header[10..12].copy_from_slice(&sum.to_be_bytes());
    let mut out = Vec::with_capacity(14 + 20 + reply.len());
    out.extend_from_slice(&frame[6..12]);
    out.extend_from_slice(&MAC);
    out.extend_from_slice(&IPV4);
    out.extend_from_slice(&header);
    out.extend_from_slice(&reply);
    Some(out)
}

/// One DNS response. Responses are ignored; additional records such as EDNS
/// options are not echoed, which RFC 6891 permits.
fn response<'a>(
    query: &[u8],
    members: impl IntoIterator<Item = (&'a str, [u8; 4])>,
) -> Option<Vec<u8>> {
    let header = query.get(..12)?;
    let flags = u16::from_be_bytes([header[2], header[3]]);
    if flags & 0x8000 != 0 {
        return None;
    }
    let opcode = flags & 0x7800;
    // QR, the request's opcode and RD; AA for an answer from this server.
    let base = 0x8000 | opcode | (flags & 0x0100);
    let refuse = |code: u16| {
        let mut out = header[..2].to_vec();
        out.extend_from_slice(&(base | code).to_be_bytes());
        out.extend_from_slice(&[0; 8]);
        Some(out)
    };
    if opcode != 0 {
        return refuse(NOTIMP);
    }
    if header[4..6] != [0, 1] {
        return refuse(FORMERR);
    }
    let Some((name, end)) = question(&query[12..]) else {
        return refuse(FORMERR);
    };
    let question = &query[12..12 + end + 4];
    let kind = u16::from_be_bytes([question[end], question[end + 1]]);
    let class = u16::from_be_bytes([question[end + 2], question[end + 3]]);
    let (code, record) = if class != CLASS_IN {
        (REFUSED, None)
    } else {
        match record(&name, members) {
            // An existing name with no records of another type answers empty.
            Some((record_kind, data)) => (0, (kind == record_kind).then_some((kind, data))),
            None => (NXDOMAIN, None),
        }
    };
    let mut out = header[..2].to_vec();
    let authoritative = if class == CLASS_IN { 0x0400 } else { 0 };
    out.extend_from_slice(&(base | authoritative | code).to_be_bytes());
    out.extend_from_slice(&[0, 1, 0, u8::from(record.is_some()), 0, 0, 0, 0]);
    out.extend_from_slice(question);
    if let Some((kind, data)) = record {
        // A compression pointer to the question name at offset 12.
        out.extend_from_slice(&[0xc0, 0x0c]);
        out.extend_from_slice(&kind.to_be_bytes());
        out.extend_from_slice(&CLASS_IN.to_be_bytes());
        out.extend_from_slice(&TTL_SECONDS.to_be_bytes());
        out.extend_from_slice(&u16::try_from(data.len()).ok()?.to_be_bytes());
        out.extend_from_slice(&data);
    }
    Some(out)
}

/// The record type and data a current member's name owns: an A record for its
/// own name, or a PTR record for its address's `in-addr.arpa` name.
fn record<'a>(
    name: &str,
    members: impl IntoIterator<Item = (&'a str, [u8; 4])>,
) -> Option<(u16, Vec<u8>)> {
    let reverse = name.strip_suffix(".in-addr.arpa").and_then(|octets| {
        let mut address = [0u8; 4];
        let mut parts = octets.split('.');
        for octet in address.iter_mut().rev() {
            let part = parts.next()?;
            // Reverse names use canonical decimal octets only.
            if part.len() > 1 && part.starts_with('0') {
                return None;
            }
            *octet = part.parse().ok()?;
        }
        parts.next().is_none().then_some(address)
    });
    let mut members = members.into_iter();
    match reverse {
        Some(address) => {
            let (member, _) = members.find(|&(_, owned)| owned == address)?;
            let mut data = Vec::with_capacity(member.len() + 2);
            for label in member.split('.') {
                data.push(u8::try_from(label.len()).ok()?);
                data.extend_from_slice(label.as_bytes());
            }
            data.push(0);
            Some((TYPE_PTR, data))
        }
        None => members
            .find(|&(member, _)| member.eq_ignore_ascii_case(name))
            .map(|(_, address)| (TYPE_A, address.to_vec())),
    }
}

/// The question's lowercase dotted name and the offset of its type field.
/// Compression pointers and names over 255 bytes are malformed in a query.
fn question(bytes: &[u8]) -> Option<(String, usize)> {
    let mut labels = Vec::new();
    let mut at = 0;
    loop {
        let length = usize::from(*bytes.get(at)?);
        at += 1;
        if length == 0 {
            break;
        }
        if length > 63 {
            return None;
        }
        let label = std::str::from_utf8(bytes.get(at..at + length)?).ok()?;
        labels.push(label.to_ascii_lowercase());
        at += length;
        if at > 255 {
            return None;
        }
    }
    bytes.get(at..at + 4)?;
    Some((labels.join("."), at))
}

fn checksum(data: &[u8]) -> u16 {
    let mut sum: u32 = data
        .chunks(2)
        .map(|pair| u32::from(u16::from_be_bytes([pair[0], *pair.get(1).unwrap_or(&0)])))
        .sum();
    while sum >> 16 != 0 {
        sum = (sum & 0xffff) + (sum >> 16);
    }
    !(sum as u16)
}

#[cfg(test)]
mod tests {
    use super::*;

    const CLIENT_MAC: [u8; 6] = [2, 0, 0, 0, 0, 2];
    const CLIENT: [u8; 4] = [10, 0, 0, 2];

    fn query(name: &str, kind: u16) -> Vec<u8> {
        let mut dns = vec![0x12, 0x34, 0x01, 0x00, 0, 1, 0, 0, 0, 0, 0, 0];
        for label in name.split('.') {
            dns.push(label.len() as u8);
            dns.extend_from_slice(label.as_bytes());
        }
        dns.push(0);
        dns.extend_from_slice(&kind.to_be_bytes());
        dns.extend_from_slice(&CLASS_IN.to_be_bytes());
        dns
    }

    fn frame(dns: &[u8]) -> Vec<u8> {
        let mut out = MAC.to_vec();
        out.extend_from_slice(&CLIENT_MAC);
        out.extend_from_slice(&IPV4);
        let mut ip = [0u8; 20];
        ip[0] = 0x45;
        ip[2..4].copy_from_slice(&((20 + 8 + dns.len()) as u16).to_be_bytes());
        ip[8] = 64;
        ip[9] = UDP;
        ip[12..16].copy_from_slice(&CLIENT);
        ip[16..20].copy_from_slice(&ADDRESS);
        out.extend_from_slice(&ip);
        out.extend_from_slice(&[0xab, 0xcd, 0, 53]);
        out.extend_from_slice(&((8 + dns.len()) as u16).to_be_bytes());
        out.extend_from_slice(&[0, 0]);
        out.extend_from_slice(dns);
        out
    }

    const MEMBERS: [(&str, [u8; 4]); 1] = [("node", [10, 0, 0, 3])];
    fn members() -> impl IntoIterator<Item = (&'static str, [u8; 4])> {
        MEMBERS
    }

    /// The DNS message of a reply, after checking every lower layer.
    fn message(reply: &[u8]) -> &[u8] {
        assert_eq!(reply[..6], CLIENT_MAC);
        assert_eq!(reply[6..12], MAC);
        assert_eq!(reply[12..14], IPV4);
        let ip = &reply[14..];
        assert_eq!(checksum(&ip[..20]), 0);
        assert_eq!(usize::from(u16::from_be_bytes([ip[2], ip[3]])), ip.len());
        assert_eq!((&ip[12..16], &ip[16..20]), (&ADDRESS[..], &CLIENT[..]));
        let udp = &ip[20..];
        assert_eq!(udp[..4], [0, 53, 0xab, 0xcd]);
        assert_eq!(usize::from(u16::from_be_bytes([udp[4], udp[5]])), udp.len());
        let mut pseudo = ADDRESS.to_vec();
        pseudo.extend_from_slice(&CLIENT);
        pseudo.extend_from_slice(&[0, UDP]);
        pseudo.extend_from_slice(&udp[4..6]);
        pseudo.extend_from_slice(udp);
        assert_eq!(checksum(&pseudo), 0);
        &udp[8..]
    }

    #[test]
    fn known_name_answers_with_its_address_and_preserves_case() {
        let request = query("NoDe", TYPE_A);
        let reply = answer(&frame(&request), members()).unwrap();
        let dns = message(&reply);
        // ID, QR|AA|RD with NOERROR, one question and one answer.
        assert_eq!(dns[..12], [0x12, 0x34, 0x85, 0x00, 0, 1, 0, 1, 0, 0, 0, 0]);
        assert_eq!(dns[12..request.len()], request[12..]);
        assert_eq!(
            dns[request.len()..],
            [0xc0, 0x0c, 0, 1, 0, 1, 0, 0, 0, 0, 0, 4, 10, 0, 0, 3]
        );
    }

    #[test]
    fn member_addresses_answer_reverse_lookups() {
        let request = query("3.0.0.10.in-addr.arpa", TYPE_PTR);
        let reply = answer(&frame(&request), members()).unwrap();
        let dns = message(&reply);
        assert_eq!(dns[2..8], [0x85, 0x00, 0, 1, 0, 1]);
        assert_eq!(
            dns[request.len()..],
            [0xc0, 0x0c, 0, 12, 0, 1, 0, 0, 0, 0, 0, 6, 4, b'n', b'o', b'd', b'e', 0]
        );
        for (name, kind, flags) in [
            ("3.0.0.10.in-addr.arpa", TYPE_A, [0x85, 0x00]),
            ("4.0.0.10.in-addr.arpa", TYPE_PTR, [0x85, 0x03]),
            ("03.0.0.10.in-addr.arpa", TYPE_PTR, [0x85, 0x03]),
            ("0.0.10.in-addr.arpa", TYPE_PTR, [0x85, 0x03]),
        ] {
            let reply = answer(&frame(&query(name, kind)), members()).unwrap();
            assert_eq!(message(&reply)[2..4], flags, "{name}");
            assert_eq!(message(&reply)[6..8], [0, 0], "{name}");
        }
    }

    #[test]
    fn unknown_names_and_other_types_answer_without_records() {
        let missing = answer(&frame(&query("gone", TYPE_A)), members()).unwrap();
        assert_eq!(message(&missing)[2..8], [0x85, 0x03, 0, 1, 0, 0]);
        let aaaa = answer(&frame(&query("node", 28)), members()).unwrap();
        assert_eq!(message(&aaaa)[2..8], [0x85, 0x00, 0, 1, 0, 0]);
        let mut chaos = query("node", TYPE_A);
        let class = chaos.len() - 2;
        chaos[class..].copy_from_slice(&3u16.to_be_bytes());
        let refused = answer(&frame(&chaos), members()).unwrap();
        assert_eq!(message(&refused)[2..8], [0x81, 0x05, 0, 1, 0, 0]);
    }

    #[test]
    fn malformed_queries_answer_errors_and_responses_are_ignored() {
        let mut compressed = query("node", TYPE_A);
        compressed[12] = 0xc0;
        let reply = answer(&frame(&compressed), members()).unwrap();
        assert_eq!(message(&reply)[2..12], [0x81, 0x01, 0, 0, 0, 0, 0, 0, 0, 0]);
        let mut notify = query("node", TYPE_A);
        notify[2] = 0x20;
        let reply = answer(&frame(&notify), members()).unwrap();
        assert_eq!(message(&reply)[2..4], [0xa0, 0x04]);
        let mut response = query("node", TYPE_A);
        response[2] |= 0x80;
        assert!(answer(&frame(&response), members()).is_none());
    }

    #[test]
    fn only_traffic_for_the_nameserver_is_answered() {
        let request = frame(&query("node", TYPE_A));
        let mut other_port = request.clone();
        other_port[37] = 54;
        let mut other_mac = request.clone();
        other_mac[5] = 9;
        let mut fragment = request.clone();
        fragment[14 + 6] = 0x20;
        for frame in [other_port, other_mac, fragment, request[..40].to_vec()] {
            assert!(answer(&frame, members()).is_none());
        }
    }

    #[test]
    fn arp_requests_for_the_nameserver_get_its_mac() {
        let mut request = [0xff; 6].to_vec();
        request.extend_from_slice(&CLIENT_MAC);
        request.extend_from_slice(&ARP);
        request.extend_from_slice(&[0, 1, 8, 0, 6, 4, 0, 1]);
        request.extend_from_slice(&CLIENT_MAC);
        request.extend_from_slice(&CLIENT);
        request.extend_from_slice(&[0; 6]);
        request.extend_from_slice(&ADDRESS);
        let reply = answer(&request, members()).unwrap();
        assert_eq!(reply[..6], CLIENT_MAC);
        assert_eq!(reply[20..22], [0, 2]);
        assert_eq!(reply[22..32], [&MAC[..], &ADDRESS[..]].concat());
        assert_eq!(reply[32..42], [&CLIENT_MAC[..], &CLIENT[..]].concat());
        let mut other = request.clone();
        other[41] = 2;
        assert!(answer(&other, members()).is_none());
    }
}
