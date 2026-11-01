//! Random packet streams through the connection classifier, checked against
//! an independent reference written from docs/internals.md
//! ("Connection incarnations") and the classifier's documented rules.
//!
//! The reference parses frames itself and keeps one record per unordered
//! endpoint pair in a plain list:
//!
//! * only untagged unicast Ethernet carrying unfragmented IPv4 TCP with
//!   nonzero ports and a header that fits is classified; IPv6, VLAN tags,
//!   other protocols, fragments, truncated or inconsistent headers and SYN
//!   combined with FIN or RST bypass selection and change nothing;
//! * an opening SYN (SYN without ACK) starts a new incarnation when the pair
//!   is unknown or closed, or when its sender already opened with another
//!   sequence number (tuple reuse); a retransmitted SYN keeps the incarnation,
//!   and the peer's own opening SYN joins it (simultaneous open);
//! * a SYN-ACK must acknowledge the peer's SYN and must not change its
//!   sender's sequence number, or it is not classified;
//! * RST, or FIN from both sides, closes the incarnation; later segments
//!   still belong to it until an opening SYN replaces it;
//! * at capacity a new pair evicts the oldest closed record, then the oldest
//!   open one, never one a selection refers to; with every record selected
//!   the new pair stays untracked;
//! * discovery lists open incarnations after a cursor, in incarnation order,
//!   32 at a time, and a selection drops only its own direction and
//!   incarnation.
use seedbox_engine_core::connections::{ConnectionState, Connections, Identity, CAPACITY};
use seedbox_engine_protocol::control::Connection;
use std::net::Ipv4Addr;

const FIN: u8 = 0x01;
const SYN: u8 = 0x02;
const RST: u8 = 0x04;
const PSH: u8 = 0x08;
const ACK: u8 = 0x10;

type Endpoint = (usize, [u8; 4], u16);

/// SplitMix64, local to this test.
struct Gen(u64);
impl Gen {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
    fn pick<T: Copy>(&mut self, items: &[T]) -> T {
        items[self.below(items.len() as u64) as usize]
    }
}

fn names() -> Vec<String> {
    ["a", "b", "c"].map(String::from).to_vec()
}

fn address(machine: usize, alias: bool) -> [u8; 4] {
    [10, 0, u8::from(alias), machine as u8 + 1]
}

/// Everything needed to build one frame; defaults are a valid IPv4 TCP segment.
#[derive(Clone, Debug)]
struct Frame {
    multicast: bool,
    vlan: bool,
    ethertype: u16,
    version: u8,
    ihl: u8,
    total_delta: i32,
    fragment: u16,
    protocol: u8,
    src: [u8; 4],
    dst: [u8; 4],
    sport: u16,
    dport: u16,
    seq: u32,
    ack: u32,
    offset: u8,
    flags: u8,
    payload: usize,
    padding: usize,
    truncate: Option<usize>,
}

impl Frame {
    fn tcp(
        src: [u8; 4],
        dst: [u8; 4],
        sport: u16,
        dport: u16,
        seq: u32,
        ack: u32,
        flags: u8,
    ) -> Self {
        Self {
            multicast: false,
            vlan: false,
            ethertype: 0x0800,
            version: 4,
            ihl: 5,
            total_delta: 0,
            fragment: 0x4000, // Don't fragment.
            protocol: 6,
            src,
            dst,
            sport,
            dport,
            seq,
            ack,
            offset: 5,
            flags,
            payload: 0,
            padding: 0,
            truncate: None,
        }
    }

    fn bytes(&self) -> Vec<u8> {
        let mut f = vec![0u8; 12];
        f[0] = if self.multicast { 0x01 } else { 0x02 };
        f[6] = 0x02;
        if self.vlan {
            f.extend_from_slice(&[0x81, 0x00, 0x00, 0x05]);
        }
        f.extend_from_slice(&self.ethertype.to_be_bytes());
        let ip_header = self.ihl as usize * 4;
        let tcp_header = self.offset as usize * 4;
        let total =
            (ip_header.max(20) + tcp_header.max(20) + self.payload) as i32 + self.total_delta;
        let mut ip = vec![0u8; ip_header.max(20)];
        ip[0] = self.version << 4 | (self.ihl & 15);
        ip[2..4].copy_from_slice(&(total.clamp(0, 0xffff) as u16).to_be_bytes());
        ip[6..8].copy_from_slice(&self.fragment.to_be_bytes());
        ip[8] = 64;
        ip[9] = self.protocol;
        ip[12..16].copy_from_slice(&self.src);
        ip[16..20].copy_from_slice(&self.dst);
        f.extend_from_slice(&ip);
        let mut tcp = vec![0u8; tcp_header.max(20)];
        tcp[0..2].copy_from_slice(&self.sport.to_be_bytes());
        tcp[2..4].copy_from_slice(&self.dport.to_be_bytes());
        tcp[4..8].copy_from_slice(&self.seq.to_be_bytes());
        tcp[8..12].copy_from_slice(&self.ack.to_be_bytes());
        tcp[12] = self.offset << 4;
        tcp[13] = self.flags;
        f.extend_from_slice(&tcp);
        f.extend((0..self.payload).map(|i| i as u8));
        f.extend(std::iter::repeat_n(0, self.padding));
        if let Some(n) = self.truncate {
            f.truncate(n);
        }
        f
    }
}

/// One classified segment as the reference parser reads it.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Segment {
    src: [u8; 4],
    dst: [u8; 4],
    sport: u16,
    dport: u16,
    seq: u32,
    ack: u32,
    flags: u8,
}

fn be16(b: &[u8]) -> u16 {
    u16::from_be_bytes([b[0], b[1]])
}

/// The reference parser: what the classifier documents it accepts.
fn parse(frame: &[u8]) -> Option<Segment> {
    if frame.len() < 14 || frame[0] & 1 != 0 || be16(&frame[12..]) != 0x0800 {
        return None;
    }
    let ip = &frame[14..];
    if ip.len() < 20 || ip[0] >> 4 != 4 {
        return None;
    }
    let header = (ip[0] & 15) as usize * 4;
    let total = be16(&ip[2..]) as usize;
    // Fragments (MF or an offset) and the reserved flag are never classified.
    if header < 20
        || total < header + 20
        || total > ip.len()
        || ip[9] != 6
        || be16(&ip[6..]) & 0xbfff != 0
    {
        return None;
    }
    let tcp = &ip[header..total];
    let offset = (tcp[12] >> 4) as usize * 4;
    let flags = tcp[13];
    if offset < 20 || offset > tcp.len() || (flags & SYN != 0 && flags & (FIN | RST) != 0) {
        return None;
    }
    let (sport, dport) = (be16(tcp), be16(&tcp[2..]));
    if sport == 0 || dport == 0 {
        return None;
    }
    Some(Segment {
        src: ip[12..16].try_into().unwrap(),
        dst: ip[16..20].try_into().unwrap(),
        sport,
        dport,
        seq: u32::from_be_bytes(tcp[4..8].try_into().unwrap()),
        ack: u32::from_be_bytes(tcp[8..12].try_into().unwrap()),
        flags,
    })
}

#[derive(Clone, Debug)]
struct Record {
    ends: [Endpoint; 2],
    id: u64,
    syn: [Option<u32>; 2],
    fin: [bool; 2],
    closed: bool,
}

impl Record {
    fn side(&self, end: Endpoint) -> usize {
        usize::from(self.ends[1] == end)
    }
    fn holds(&self, a: Endpoint, b: Endpoint) -> bool {
        self.ends == [a, b] || self.ends == [b, a]
    }
}

#[derive(Clone, Default)]
struct Reference {
    records: Vec<Record>,
    last: u64,
    /// Selected (from, to, incarnation) directions.
    selected: Vec<(Endpoint, Endpoint, u64)>,
    /// Records evicted at capacity.
    evicted: u64,
}

impl Reference {
    fn observe(
        &mut self,
        from: usize,
        to: usize,
        frame: &[u8],
    ) -> Option<(Endpoint, Endpoint, u64)> {
        let p = parse(frame)?;
        let (src, dst) = ((from, p.src, p.sport), (to, p.dst, p.dport));
        let opening = p.flags & SYN != 0 && p.flags & ACK == 0;
        let found = self.records.iter().position(|r| r.holds(src, dst));
        let starts = opening
            && match found {
                None => true,
                Some(i) => {
                    let r = &self.records[i];
                    r.closed || r.syn[r.side(src)].is_some_and(|seq| seq != p.seq)
                }
            };
        if starts {
            if found.is_none() && self.records.len() == CAPACITY {
                let victim = self
                    .records
                    .iter()
                    .enumerate()
                    .filter(|(_, r)| !self.selected.iter().any(|s| s.2 == r.id))
                    .min_by_key(|(_, r)| (!r.closed, r.id))
                    .map(|(i, _)| i)?;
                self.records.remove(victim);
                self.evicted += 1;
            }
            self.last += 1;
            let record = Record {
                ends: [src, dst],
                id: self.last,
                syn: [None, None],
                fin: [false, false],
                closed: false,
            };
            match self.records.iter().position(|r| r.holds(src, dst)) {
                Some(i) => self.records[i] = record,
                None => self.records.push(record),
            }
        }
        let r = self.records.iter_mut().find(|r| r.holds(src, dst))?;
        let (me, peer) = (r.side(src), 1 - r.side(src));
        if p.flags & SYN != 0 {
            if !opening && r.syn[peer].is_none_or(|seq| p.ack != seq.wrapping_add(1)) {
                return None;
            }
            if r.syn[me].is_some_and(|seq| seq != p.seq) {
                return None;
            }
            r.syn[me] = Some(p.seq);
        }
        if p.flags & FIN != 0 {
            r.fin[me] = true;
        }
        if p.flags & RST != 0 || r.fin == [true, true] {
            r.closed = true;
        }
        Some((src, dst, r.id))
    }

    fn list(&self, from: usize, to: usize, after: u64) -> Vec<(Endpoint, Endpoint, u64)> {
        let mut out: Vec<_> = self
            .records
            .iter()
            .filter(|r| !r.closed && r.id > after)
            .flat_map(|r| [(r.ends[0], r.ends[1], r.id), (r.ends[1], r.ends[0], r.id)])
            .filter(|(a, b, _)| a.0 == from && b.0 == to)
            .collect();
        out.sort_by_key(|c| (c.2, c.0, c.1));
        out.truncate(32);
        out
    }
}

fn flat(identity: Identity) -> (Endpoint, Endpoint, u64) {
    ((identity.0).0, (identity.0).1, identity.1)
}

fn connection(c: (Endpoint, Endpoint, u64)) -> Connection {
    let names = names();
    Connection {
        from: names[c.0 .0].clone(),
        to: names[c.1 .0].clone(),
        src_addr: Ipv4Addr::from(c.0 .1),
        dst_addr: Ipv4Addr::from(c.1 .1),
        src_port: c.0 .2,
        dst_port: c.1 .2,
        incarnation: c.2,
    }
}

fn unflat(c: &Connection) -> (Endpoint, Endpoint, u64) {
    let names = names();
    let index = |n: &str| names.iter().position(|m| m == n).unwrap();
    (
        (index(&c.from), c.src_addr.octets(), c.src_port),
        (index(&c.to), c.dst_addr.octets(), c.dst_port),
        c.incarnation,
    )
}

/// A small flow space so tuples are reused and collide often.
struct Flows {
    /// Last opening sequence per (client, server) direction.
    isn: std::collections::BTreeMap<(Endpoint, Endpoint), u32>,
}

/// A generated packet: who sends it, its frame, and whether the generator
/// built it as a classifiable segment.
fn generate(g: &mut Gen, flows: &mut Flows) -> (usize, usize, Frame, bool) {
    let client = g.below(3) as usize;
    let server = (client + 1 + g.below(2) as usize) % 3;
    let alias = g.below(8) == 0;
    let c = (
        client,
        address(client, alias),
        g.pick(&[4000u16, 4001, 4002]),
    );
    let s = (server, address(server, false), g.pick(&[80u16, 443]));
    let reverse = g.below(2) == 0;
    let (from, to) = if reverse { (s, c) } else { (c, s) };
    let isn = |g: &mut Gen| g.pick(&[7u32, 1000, u32::MAX, 0x8000_0000]);
    let mut frame = Frame::tcp(
        from.1,
        to.1,
        from.2,
        to.2,
        g.next() as u32,
        g.next() as u32,
        ACK,
    );
    let mut valid = true;
    match g.below(26) {
        // Opening SYNs: fresh, retransmitted, or reused with a new sequence.
        0..=3 => {
            let seq = match flows.isn.get(&(from, to)) {
                Some(&old) if g.below(2) == 0 => old,
                _ => isn(g),
            };
            flows.isn.insert((from, to), seq);
            frame.seq = seq;
            frame.ack = 0;
            frame.flags = SYN;
        }
        // SYN-ACK acknowledging the peer's last SYN, or a wrong one.
        4..=6 => {
            let peer = flows.isn.get(&(to, from)).copied().unwrap_or(5);
            frame.flags = SYN | ACK;
            frame.ack = if g.below(5) == 0 {
                peer.wrapping_add(2)
            } else {
                peer.wrapping_add(1)
            };
            frame.seq = match flows.isn.get(&(from, to)) {
                Some(&own) if g.below(3) != 0 => own,
                _ => isn(g),
            };
            flows.isn.insert((from, to), frame.seq);
        }
        7..=11 => frame.flags = ACK | PSH,
        12..=13 => frame.flags = FIN | ACK,
        14 => frame.flags = RST | g.pick(&[0, ACK]),
        15 => {
            frame.flags = SYN | g.pick(&[FIN, RST, FIN | ACK]);
            valid = false;
        }
        // Valid variants: IP and TCP options, payload, Ethernet padding.
        16 => {
            frame.ihl = 5 + g.below(11) as u8;
            frame.offset = 5 + g.below(11) as u8;
            frame.payload = g.below(64) as usize;
            frame.padding = g.below(10) as usize;
        }
        17 => {
            frame.fragment = g.pick(&[0x2000, 0x0001, 0x2010, 0x8000]);
            valid = false;
        }
        18 => {
            frame.ethertype = 0x86dd;
            frame.version = 6;
            valid = false;
        }
        19 => {
            frame.vlan = true;
            valid = false;
        }
        20 => {
            frame.protocol = 17;
            valid = false;
        }
        21 => {
            frame.multicast = true;
            valid = false;
        }
        22 => {
            // Cut anywhere inside the headers.
            frame.truncate = Some(g.below(54) as usize);
            valid = false;
        }
        23 => {
            // IP total length past the frame, or too short for TCP.
            frame.total_delta = g.pick(&[1, 40, -1, -20]);
            valid = false;
        }
        24 => {
            frame.ihl = g.below(5) as u8;
            frame.offset = g.pick(&[0u8, 4, 15]);
            valid = frame.ihl >= 5 && frame.offset >= 5;
        }
        _ => {
            // An opening SYN would otherwise start an incarnation.
            frame.flags = g.pick(&[SYN, ACK]);
            if g.below(2) == 0 {
                frame.sport = 0;
            } else {
                frame.dport = 0;
            }
            valid = false;
        }
    }
    (from.0, to.0, frame, valid)
}

fn check_lists(model: &Reference, actual: &Connections, step: usize) {
    for from in 0..3 {
        for to in 0..3 {
            for after in [0, model.last / 2, model.last.saturating_sub(1)] {
                let expected = model.list(from, to, after);
                let got: Vec<_> = actual
                    .list(&names()[from], &names()[to], after, &names())
                    .iter()
                    .map(unflat)
                    .collect();
                let key = |v: &Vec<(Endpoint, Endpoint, u64)>| {
                    let mut v = v.clone();
                    v.sort();
                    v
                };
                // Incarnation order is required; ties inside one incarnation
                // cannot occur in one direction.
                assert_eq!(
                    got.iter().map(|c| c.2).collect::<Vec<_>>(),
                    expected.iter().map(|c| c.2).collect::<Vec<_>>(),
                    "step {step}: list {from}->{to} after {after}"
                );
                assert_eq!(key(&got), key(&expected), "step {step}: list {from}->{to}");
                for c in &expected {
                    assert!(
                        actual.available(&connection(*c), &names()),
                        "step {step}: {c:?}"
                    );
                }
            }
        }
    }
}

fn stream(seed: u64, packets: usize) {
    let mut g = Gen(seed);
    let mut flows = Flows {
        isn: Default::default(),
    };
    let mut actual = Connections::default();
    let mut model = Reference::default();
    let mut drops = [0u64; 2];
    let mut classified = 0;
    let mut handles = 0;
    let mut snapshot: Option<(ConnectionState, Reference)> = None;
    for step in 0..packets {
        let (from, to, spec, intended) = generate(&mut g, &mut flows);
        let mut frame = spec.bytes();
        let mutated = g.below(12) == 0;
        if mutated && !frame.is_empty() {
            let at = g.below(frame.len() as u64) as usize;
            frame[at] ^= 1 << g.below(8);
        }
        let parsed = parse(&frame);
        if !mutated {
            assert_eq!(
                parsed.is_some(),
                intended,
                "step {step}: reference parser disagrees with {spec:?}"
            );
        }
        let expected = model.observe(from, to, &frame);
        let got = actual.observe(from, to, &frame).map(flat);
        assert_eq!(
            got, expected,
            "seed {seed} step {step}: {spec:?} mutated={mutated}"
        );
        classified += usize::from(got.is_some());
        // A selection drops only its own direction and incarnation.
        let want_drop = got.is_some_and(|c| model.selected.contains(&c));
        let dropped = actual.drops(got.map(|c| ((c.0, c.1), c.2)), false, |_| {});
        assert_eq!(dropped, want_drop, "step {step}: drop decision for {got:?}");
        drops[usize::from(dropped)] += 1;
        if step % 50 == 49 {
            check_lists(&model, &actual, step);
            // Select one listed direction now and then.
            let open = model.list(g.below(3) as usize, g.below(3) as usize, 0);
            if let Some(&c) = open.first() {
                handles += 1;
                actual
                    .select(handles, format!("h{handles}"), connection(c), &names())
                    .unwrap();
                model.selected.push(c);
            }
            // A closed or replaced incarnation can never be selected.
            let stale = (c_endpoint(0), c_endpoint(1), model.last + 1);
            assert!(actual
                .select(0, "stale".into(), connection(stale), &names())
                .is_err());
        }
        if step % 333 == 100 {
            snapshot = Some((actual.export_state(), model.clone()));
        }
        if step % 333 == 200 {
            // Restoring an earlier capture, through the plain state the
            // Driver persists, continues exactly from there.
            if let Some((state, captured)) = snapshot.take() {
                actual = Connections::from_state(state, &names()).unwrap();
                model = captured;
            }
        }
    }
    assert!(
        classified > packets / 4,
        "seed {seed}: only {classified} classified"
    );
    assert!(drops[1] > 0, "seed {seed}: no selected drop was exercised");
}

fn c_endpoint(machine: usize) -> Endpoint {
    (machine, address(machine, false), 4000)
}

#[test]
fn random_packet_streams_match_the_reference_incarnation_model() {
    for seed in [1, 2, 3, 85, 0xc0ffee, u64::MAX] {
        stream(seed, 4000);
    }
}

#[test]
fn capacity_eviction_matches_the_reference() {
    // Many short-lived and long-lived pairs past capacity, with selections
    // pinning some records, and finally a table where every record is selected.
    let mut g = Gen(1024);
    let mut actual = Connections::default();
    let mut model = Reference::default();
    let names = names();
    let mut handles = 0;
    let syn = |sport: u16, seq: u32, flags: u8, reverse: bool| {
        let (a, b) = (address(0, false), address(1, false));
        if reverse {
            Frame::tcp(b, a, 80, sport, seq, 0, flags)
        } else {
            Frame::tcp(a, b, sport, 80, seq, 0, flags)
        }
        .bytes()
    };
    for round in 0..2 * CAPACITY {
        let port = 1 + g.below(2 * CAPACITY as u64) as u16;
        let seq = g.next() as u32;
        let frames = match g.below(4) {
            0 => vec![
                (0, 1, syn(port, seq, SYN, false)),
                (0, 1, syn(port, seq, RST, false)),
            ],
            _ => vec![(0, 1, syn(port, seq, SYN, false))],
        };
        for (from, to, frame) in frames {
            assert_eq!(
                actual.observe(from, to, &frame).map(flat),
                model.observe(from, to, &frame),
                "round {round}"
            );
        }
        if g.below(32) == 0 {
            if let Some(&c) = model.list(0, 1, g.below(model.last + 1)).first() {
                handles += 1;
                actual
                    .select(handles, format!("h{handles}"), connection(c), &names)
                    .unwrap();
                model.selected.push(c);
            }
        }
    }
    assert!(model.evicted > 0, "capacity was never reached");
    check_lists(&model, &actual, 0);
    // A table of open, all-selected records can evict nothing: a new pair
    // stays untracked, while selected pairs keep classifying.
    let mut actual = Connections::default();
    let mut model = Reference::default();
    for port in 1..=CAPACITY as u16 {
        let frame = syn(port, port as u32, SYN, false);
        let c = model.observe(0, 1, &frame).unwrap();
        assert_eq!(actual.observe(0, 1, &frame).map(flat), Some(c));
        actual
            .select(port as u32, format!("p{port}"), connection(c), &names)
            .unwrap();
        model.selected.push(c);
    }
    let fresh = syn(9999, 1, SYN, true);
    assert_eq!(model.observe(1, 0, &fresh), None);
    assert_eq!(actual.observe(1, 0, &fresh).map(flat), None);
    let old = syn(7, 7, ACK, false);
    assert_eq!(
        actual.observe(0, 1, &old).map(flat),
        model.observe(0, 1, &old)
    );
    check_lists(&model, &actual, 1);
}
