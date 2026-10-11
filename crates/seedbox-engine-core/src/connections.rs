//! Bounded Ethernet/IPv4/TCP classification; no application decoding or RNG.
use seedbox_control::Connection;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

pub const CAPACITY: usize = 1024;
/// TCP header flag bits.
const FIN: u8 = 0x01;
const SYN: u8 = 0x02;
const RST: u8 = 0x04;
const ACK: u8 = 0x10;
/// The flags byte of a minimal Ethernet/IPv4/TCP frame, as [`reset_frame`] builds.
const MINIMAL_TCP_FLAGS: usize = 14 + 20 + 13;
type Endpoint = (usize, [u8; 4], u16);
type Tuple = (Endpoint, Endpoint);
pub type Identity = (Tuple, u64);
#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct Session {
    id: u64,
    syn: [Option<u32>; 2],
    fin: [bool; 2],
    closed: bool,
}
#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct Selection {
    pub fault_id: u32,
    pub connection: Connection,
    identity: Identity,
    pub matched: u64,
    pub dropped: u64,
}
#[derive(Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct Connections {
    #[serde(with = "crate::checkpoint::ordered_map")]
    sessions: BTreeMap<Tuple, Session>,
    next: u64,
    evicted: u64,
    pub used: bool,
    pub selections: BTreeMap<String, Selection>,
}
struct Packet {
    src: [u8; 4],
    dst: [u8; 4],
    sport: u16,
    dport: u16,
    seq: u32,
    ack: u32,
    flags: u8,
}
fn packet(frame: &[u8]) -> Option<Packet> {
    // Untagged unicast Ethernet only. Never reassemble or guess fragment ports.
    if frame.len() < 54 || frame[0] & 1 != 0 || frame[12..14] != [8, 0] {
        return None;
    }
    let ip = &frame[14..];
    let ihl = (ip[0] & 15) as usize * 4;
    let total = u16::from_be_bytes([ip[2], ip[3]]) as usize;
    if ip[0] >> 4 != 4
        || ihl < 20
        || total < ihl + 20
        || total > ip.len()
        || ip[9] != 6
        || u16::from_be_bytes([ip[6], ip[7]]) & 0xbfff != 0
    {
        return None;
    }
    let tcp = &ip[ihl..total];
    let offset = (tcp[12] >> 4) as usize * 4;
    if offset < 20 || offset > tcp.len() || tcp[13] & SYN != 0 && tcp[13] & (FIN | RST) != 0 {
        return None;
    }
    let sport = u16::from_be_bytes([tcp[0], tcp[1]]);
    let dport = u16::from_be_bytes([tcp[2], tcp[3]]);
    if sport == 0 || dport == 0 {
        return None;
    }
    Some(Packet {
        src: ip[12..16].try_into().unwrap(),
        dst: ip[16..20].try_into().unwrap(),
        sport,
        dport,
        seq: u32::from_be_bytes(tcp[4..8].try_into().unwrap()),
        ack: u32::from_be_bytes(tcp[8..12].try_into().unwrap()),
        flags: tcp[13],
    })
}
/// Replace an ACK-bearing, non-SYN/non-RST IPv4 TCP segment with a reset in
/// that direction, preserving the observed sequence for the receiver's normal
/// TCP window validation. No host TCP stack, application decoding, reassembly
/// or guessed ports.
/// Construct fresh minimal headers, including IP and TCP pseudoheader checksums.
pub fn reset_frame(frame: &[u8]) -> Option<Vec<u8>> {
    let p = packet(frame)?;
    if p.flags & (SYN | RST | ACK) != ACK {
        return None;
    }
    let mut reset = vec![0; 54];
    reset[..14].copy_from_slice(&frame[..14]);
    let ip = &mut reset[14..34];
    ip[0] = 0x45;
    ip[2..4].copy_from_slice(&40u16.to_be_bytes());
    ip[4..6].copy_from_slice(&frame[18..20]);
    ip[8] = frame[22];
    ip[9] = 6;
    ip[12..16].copy_from_slice(&p.src);
    ip[16..20].copy_from_slice(&p.dst);
    let sum = checksum(ip);
    ip[10..12].copy_from_slice(&sum.to_be_bytes());
    let tcp = &mut reset[34..];
    tcp[..2].copy_from_slice(&p.sport.to_be_bytes());
    tcp[2..4].copy_from_slice(&p.dport.to_be_bytes());
    tcp[4..8].copy_from_slice(&p.seq.to_be_bytes());
    tcp[8..12].copy_from_slice(&p.ack.to_be_bytes());
    tcp[12] = 0x50;
    tcp[13] = RST | ACK;
    let mut pseudo = Vec::from(p.src);
    pseudo.extend_from_slice(&p.dst);
    pseudo.extend_from_slice(&[0, 6, 0, 20]);
    pseudo.extend_from_slice(tcp);
    tcp[16..18].copy_from_slice(&checksum(&pseudo).to_be_bytes());
    Some(reset)
}
/// Whether `frame` is a reset made by [`reset_frame`]: its fixed minimal
/// headers put RST|ACK at a known offset.
pub fn is_reset_frame(frame: &[u8]) -> bool {
    frame.get(MINIMAL_TCP_FLAGS) == Some(&(RST | ACK))
}
fn checksum(bytes: &[u8]) -> u16 {
    let mut sum: u32 = bytes
        .chunks(2)
        .map(|c| u16::from_be_bytes([c[0], *c.get(1).unwrap_or(&0)]) as u32)
        .sum();
    while sum >> 16 != 0 {
        sum = (sum & 0xffff) + (sum >> 16);
    }
    !(sum as u16)
}

fn key(src: Endpoint, dst: Endpoint) -> (Tuple, usize) {
    if src < dst {
        ((src, dst), 0)
    } else {
        ((dst, src), 1)
    }
}
fn connection(tuple: Tuple, dir: usize, id: u64, names: &[String]) -> Connection {
    let (a, b) = if dir == 0 { tuple } else { (tuple.1, tuple.0) };
    Connection {
        from: names[a.0].clone(),
        to: names[b.0].clone(),
        src_addr: a.1.into(),
        dst_addr: b.1.into(),
        src_port: a.2,
        dst_port: b.2,
        incarnation: id,
    }
}
impl Connections {
    pub fn observe(&mut self, from: usize, to: usize, frame: &[u8]) -> Option<Identity> {
        let p = packet(frame)?;
        let (tuple, dir) = key((from, p.src, p.sport), (to, p.dst, p.dport));
        let opening = p.flags & (SYN | ACK) == SYN;
        if opening
            && self
                .sessions
                .get(&tuple)
                .is_none_or(|s| s.closed || s.syn[dir].is_some_and(|seq| seq != p.seq))
        {
            let id = self.next.checked_add(1)?;
            if !self.sessions.contains_key(&tuple) && self.sessions.len() == CAPACITY {
                // Protect every live selection, including closed incarnations.
                // Prefer the oldest closed session, then the oldest unselected open one.
                // If all sessions are selected, leave the new tuple untracked.
                let oldest = *self
                    .sessions
                    .iter()
                    .filter(|(_, s)| {
                        !self
                            .selections
                            .values()
                            .any(|selection| selection.connection.incarnation == s.id)
                    })
                    .min_by_key(|(_, s)| (!s.closed, s.id))?
                    .0;
                self.sessions.remove(&oldest);
                self.evicted += 1;
            }
            self.next = id;
            self.sessions.insert(
                tuple,
                Session {
                    id,
                    syn: [None, None],
                    fin: [false, false],
                    closed: false,
                },
            );
        }
        let s = self.sessions.get_mut(&tuple)?;
        if p.flags & SYN != 0 {
            if !opening && s.syn[1 - dir].is_none_or(|seq| p.ack != seq.wrapping_add(1)) {
                return None;
            }
            if s.syn[dir].is_some_and(|seq| seq != p.seq) {
                return None;
            }
            s.syn[dir] = Some(p.seq);
        }
        if p.flags & FIN != 0 {
            s.fin[dir] = true;
        }
        if p.flags & RST != 0 || s.fin == [true, true] {
            s.closed = true;
        }
        Some((if dir == 0 { tuple } else { (tuple.1, tuple.0) }, s.id))
    }
    pub fn available(&self, c: &Connection, names: &[String]) -> bool {
        self.sessions.iter().any(|(&tuple, s)| {
            !s.closed
                && s.id == c.incarnation
                && (0..2).any(|d| connection(tuple, d, s.id, names) == *c)
        })
    }
    pub fn list(&self, from: &str, to: &str, after: u64, names: &[String]) -> Vec<Connection> {
        let mut result = Vec::new();
        for (&tuple, s) in &self.sessions {
            if s.closed || s.id <= after {
                continue;
            }
            for d in 0..2 {
                let c = connection(tuple, d, s.id, names);
                if c.from == from && c.to == to {
                    result.push(c);
                }
            }
        }
        result.sort_by_key(|c| c.incarnation);
        result.truncate(32);
        result
    }
    pub fn select(
        &mut self,
        fault_id: u32,
        handle: String,
        c: Connection,
        names: &[String],
    ) -> Result<(), String> {
        if self.selections.len() == CAPACITY {
            return Err("connection selector capacity exhausted".into());
        }
        if !self.available(&c, names) {
            return Err("connection incarnation unavailable".into());
        }
        if self.selections.contains_key(&handle) {
            return Err("duplicate connection handle".into());
        }
        // An available connection was built from these names.
        let index = |name: &str| {
            names
                .iter()
                .position(|n| n == name)
                .ok_or("unknown connection machine")
        };
        let (from, to) = (index(&c.from)?, index(&c.to)?);
        let identity = (
            (
                (from, c.src_addr.octets(), c.src_port),
                (to, c.dst_addr.octets(), c.dst_port),
            ),
            c.incarnation,
        );
        self.used = true;
        self.selections.insert(
            handle,
            Selection {
                fault_id,
                connection: c,
                identity,
                matched: 0,
                dropped: 0,
            },
        );
        Ok(())
    }
    /// Called at send time, after classification, before link RNG/FIFO decisions.
    /// A partition masks selected loss; overlaps each retain their own evidence.
    pub fn drops(
        &mut self,
        c: Option<Identity>,
        cut: bool,
        mut matched: impl FnMut(&Selection),
    ) -> bool {
        let mut drop = false;
        for s in self.selections.values_mut() {
            if c == Some(s.identity) {
                s.matched += 1;
                if !cut {
                    s.dropped += 1;
                    drop = true;
                }
                matched(s);
            }
        }
        drop
    }
    pub fn witness(&self, h: &mut Sha256) {
        if !self.used {
            return;
        }
        h.update(b"seedbox-connections-v1");
        h.update(self.next.to_le_bytes());
        h.update(self.evicted.to_le_bytes());
        h.update((self.sessions.len() as u64).to_le_bytes());
        for (&(a, b), s) in &self.sessions {
            for (m, addr, port) in [a, b] {
                h.update((m as u64).to_le_bytes());
                h.update(addr);
                h.update(port.to_le_bytes());
            }
            h.update(s.id.to_le_bytes());
            for syn in s.syn {
                h.update([syn.is_some() as u8]);
                h.update(syn.unwrap_or(0).to_le_bytes());
            }
            h.update([s.fin[0] as u8, s.fin[1] as u8, s.closed as u8]);
        }
        h.update((self.selections.len() as u64).to_le_bytes());
        for (handle, s) in &self.selections {
            h.update(s.fault_id.to_le_bytes());
            crate::links::string_hash(h, handle);
            crate::links::string_hash(h, &serde_json::to_string(&s.connection).unwrap());
            h.update(s.matched.to_le_bytes());
            h.update(s.dropped.to_le_bytes());
        }
    }
}

#[cfg(test)]
mod reset_tests {
    use super::*;
    #[test]
    fn reset_keeps_direction_and_sequence_removes_options_and_payload_and_has_valid_checksums() {
        let mut frame = vec![0; 75];
        frame[12..14].copy_from_slice(&[8, 0]);
        frame[14] = 0x46; // IPv4 options.
        frame[16..18].copy_from_slice(&61u16.to_be_bytes());
        frame[22] = 64;
        frame[23] = 6;
        frame[26..34].copy_from_slice(&[10, 0, 0, 1, 10, 0, 0, 2]);
        frame[38..42].copy_from_slice(&[0x0f, 0xa0, 0, 80]);
        frame[42..46].copy_from_slice(&7u32.to_be_bytes());
        frame[46..50].copy_from_slice(&51u32.to_be_bytes());
        frame[50] = 0x60;
        frame[51] = 0x18; // TCP options and data.
        let reset = reset_frame(&frame).unwrap();
        assert_eq!(reset.len(), 54);
        assert_eq!(&reset[..14], &frame[..14]);
        assert_eq!(&reset[26..34], &frame[26..34]);
        assert_eq!(&reset[34..46], &frame[38..50]);
        assert_eq!(&reset[46..48], &[0x50, 0x14]);
        assert_eq!(checksum(&reset[14..34]), 0);
        let mut pseudo = reset[26..34].to_vec();
        pseudo.extend_from_slice(&[0, 6, 0, 20]);
        pseudo.extend_from_slice(&reset[34..]);
        assert_eq!(checksum(&pseudo), 0);
        for flag in [2, 0x12, 4, 0x14, 0] {
            let mut bad = frame.clone();
            bad[51] = flag;
            assert!(reset_frame(&bad).is_none());
        }
        for (offset, value) in [
            (0, 1),
            (12, 0x86),
            (14, 0x66),
            (20, 0x20),
            (21, 1),
            (23, 17),
            (50, 0x10),
        ] {
            let mut bad = frame.clone();
            bad[offset] = value;
            assert!(reset_frame(&bad).is_none());
        }
        for n in 0..frame.len() {
            assert!(reset_frame(&frame[..n]).is_none());
        }
    }
}
