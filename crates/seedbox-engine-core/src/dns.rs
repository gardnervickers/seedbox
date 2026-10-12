//! Deterministic Ethernet/IPv4/UDP DNS answers for the virtual nameserver.

use hickory_proto::{
    op::{Edns, Message, MessageType, OpCode, ResponseCode},
    rr::{rdata::A, DNSClass, RData, Record, RecordType},
};

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

/// Reply to a DNS query sent to the virtual nameserver.
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
    let answer = dns_response(&udp[8..udp_len], records)?;
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

fn dns_response(query: &[u8], records: impl Fn(&str) -> Option<[u8; 4]>) -> Option<Vec<u8>> {
    let query = Message::from_vec(query).ok()?;
    if query.metadata.message_type != MessageType::Query {
        return None;
    }
    // Explicit IDs avoid the library's random query constructor. Hickory owns
    // DNS parsing, name compression and encoding; the simulation owns records.
    let mut answer = Message::response(query.metadata.id, query.metadata.op_code);
    answer.metadata.recursion_desired = query.metadata.recursion_desired;
    if query.edns.is_some() {
        let mut edns = Edns::new();
        edns.set_max_payload(1232);
        answer.edns = Some(edns);
    }
    if query.version() != 0 {
        answer.metadata.response_code = ResponseCode::BADVERS;
    } else if query.metadata.op_code != OpCode::Query {
        answer.metadata.response_code = ResponseCode::NotImp;
    } else if query.queries.len() != 1 {
        answer.metadata.response_code = ResponseCode::FormErr;
    } else {
        let question = &query.queries[0];
        answer.add_query(question.clone());
        if question.query_class() != DNSClass::IN {
            answer.metadata.response_code = ResponseCode::Refused;
        } else {
            answer.metadata.authoritative = true;
            let name = question.name().to_lowercase().to_ascii();
            match records(name.trim_end_matches('.')) {
                Some(address) if question.query_type() == RecordType::A => {
                    answer.add_answer(Record::from_rdata(
                        question.name().clone(),
                        TTL_SECONDS,
                        RData::A(A(address.into())),
                    ));
                }
                Some(_) => {} // Existing name with no records of the requested type.
                None => answer.metadata.response_code = ResponseCode::NXDomain,
            }
        }
    }
    answer.to_vec().ok()
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
        packet(&dns)
    }

    fn packet(dns: &[u8]) -> Vec<u8> {
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
        out.extend_from_slice(dns);
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
        let message = Message::from_vec(&reply[42..]).unwrap();
        assert_eq!(message.answers[0].ttl, TTL_SECONDS);
        assert!(message.metadata.authoritative);
        assert!(!message.metadata.recursion_available);
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
    fn known_names_without_the_requested_type_differ_from_unknown_names() {
        let mut query = request(b"node");
        let kind = query.len() - 4;
        query[kind + 1] = 28;
        for (address, code) in [
            (Some([10, 0, 0, 3]), ResponseCode::NoError),
            (None, ResponseCode::NXDomain),
        ] {
            let answer = reply(&query, |_| address).unwrap();
            let message = Message::from_vec(&answer[42..]).unwrap();
            assert_eq!(message.metadata.response_code, code);
            assert!(message.answers.is_empty());
        }
    }

    #[test]
    fn edns_and_compressed_names_are_decoded_by_hickory() {
        let frame = request(b"Node");
        let mut dns = frame[42..].to_vec();
        // Compress the root suffix against the earlier zero byte in the header.
        dns.splice(17..18, [0xc0, 0x04]);
        dns[11] = 1;
        // OPT: 1232-byte payload, version zero, no options.
        dns.extend_from_slice(&[0, 0, 41, 4, 208, 0, 0, 0, 0, 0, 0]);
        let response = reply(&packet(&dns), |name| {
            assert_eq!(name, "node");
            Some([10, 0, 0, 3])
        })
        .unwrap();
        let message = Message::from_vec(&response[42..]).unwrap();
        assert_eq!(message.metadata.id, 0x1234);
        assert_eq!(message.metadata.response_code, ResponseCode::NoError);
        assert_eq!(message.answers.len(), 1);
        assert_eq!(message.edns.unwrap().version(), 0);
        // Repeating the identical request cannot sample host randomness.
        assert_eq!(
            response,
            reply(&packet(&dns), |_| Some([10, 0, 0, 3])).unwrap()
        );
    }

    #[test]
    fn each_query_uses_current_membership_and_address_without_a_dns_cache() {
        use std::{cell::RefCell, collections::BTreeMap};
        let records = RefCell::new(BTreeMap::new());
        let lookup = |name: &str| records.borrow().get(name).copied();
        let query = request(b"node");
        for address in [None, Some([10, 0, 0, 3]), None, Some([10, 0, 0, 7])] {
            records.borrow_mut().clear();
            if let Some(address) = address {
                records.borrow_mut().insert("node".to_owned(), address);
            }
            let response = reply(&query, lookup).unwrap();
            let message = Message::from_vec(&response[42..]).unwrap();
            assert_eq!(
                message.metadata.response_code,
                if address.is_some() {
                    ResponseCode::NoError
                } else {
                    ResponseCode::NXDomain
                }
            );
            if let Some(address) = address {
                assert_eq!(&response[response.len() - 4..], &address);
            } else {
                assert!(message.answers.is_empty());
            }
        }
    }

    #[test]
    fn unsupported_requests_receive_dns_errors_instead_of_timing_out() {
        let query = Message::from_vec(&request(b"node")[42..]).unwrap();
        let mut multiple = query.clone();
        multiple.queries.push(query.queries[0].clone());
        let mut opcode = query.clone();
        opcode.metadata.op_code = OpCode::Status;
        let mut class = query.clone();
        class.queries[0].set_query_class(DNSClass::CH);
        let mut version = query;
        let mut edns = Edns::new();
        edns.set_version(1);
        version.edns = Some(edns);
        for (query, code) in [
            (multiple, ResponseCode::FormErr),
            (opcode, ResponseCode::NotImp),
            (class, ResponseCode::Refused),
            (version, ResponseCode::BADVERS),
        ] {
            let response = reply(&packet(&query.to_vec().unwrap()), |_| {
                panic!("invalid query must not consult the registry")
            })
            .unwrap();
            let message = Message::from_vec(&response[42..]).unwrap();
            // BADVERS and BADSIG share wire code 16; Hickory decodes the alias.
            assert_eq!(u16::from(message.metadata.response_code), u16::from(code));
            assert!(message.answers.is_empty());
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
        for length in 0..query.len() {
            assert!(reply(&query[..length], |_| None).is_none());
        }
        let mut fragment = query.clone();
        fragment[20] = 0x20;
        assert!(reply(&fragment, |_| None).is_none());
        let mut response = query;
        response[44] = 0x80;
        assert!(reply(&response, |_| None).is_none());
    }
}
