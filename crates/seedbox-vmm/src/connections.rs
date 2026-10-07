//! Bounded Ethernet/IPv4/TCP classification; no application decoding or RNG.
use seedbox_control::Connection;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

pub const CAPACITY: usize = 1024;
type Endpoint = (usize, [u8;4], u16);
type Tuple = (Endpoint, Endpoint);
#[derive(Clone)]
struct Session {
    id: u64,
    syn: [Option<u32>;2],
    fin: [bool;2],
    closed: bool,
}
#[derive(Clone)]
pub struct Selection {
    pub fault_id: u32,
    pub connection: Connection,
    pub matched: u64,
    pub dropped: u64,
}
#[derive(Clone, Default)]
pub struct Connections {
    sessions: BTreeMap<Tuple, Session>,
    next: u64,
    evicted: u64,
    pub used: bool,
    pub selections: BTreeMap<String, Selection>,
}
struct Packet { src: [u8;4], dst: [u8;4], sport: u16, dport: u16, seq: u32, ack: u32, flags: u8 }
fn packet(frame: &[u8]) -> Option<Packet> {
    // Untagged unicast Ethernet only. Never reassemble or guess fragment ports.
    if frame.len()<54 || frame[0]&1!=0 || frame[12..14]!=[8,0] { return None; }
    let ip=&frame[14..];
    let ihl=(ip[0]&15) as usize*4;
    let total=u16::from_be_bytes([ip[2],ip[3]]) as usize;
    if ip[0]>>4!=4 || ihl<20 || total<ihl+20 || total>ip.len() || ip[9]!=6 || u16::from_be_bytes([ip[6],ip[7]])&0xbfff!=0 { return None; }
    let tcp=&ip[ihl..total];
    let offset=(tcp[12]>>4) as usize*4;
    if offset<20 || offset>tcp.len() || tcp[13]&2!=0 && tcp[13]&5!=0 { return None; }
    let sport=u16::from_be_bytes([tcp[0],tcp[1]]);
    let dport=u16::from_be_bytes([tcp[2],tcp[3]]);
    if sport==0 || dport==0 { return None; }
    Some(Packet {src:ip[12..16].try_into().unwrap(),dst:ip[16..20].try_into().unwrap(),sport,dport,seq:u32::from_be_bytes(tcp[4..8].try_into().unwrap()),ack:u32::from_be_bytes(tcp[8..12].try_into().unwrap()),flags:tcp[13]})
}
fn key(src: Endpoint, dst: Endpoint) -> (Tuple, usize) {
    if src<dst { ((src,dst),0) } else { ((dst,src),1) }
}
fn connection(tuple: Tuple, dir: usize, id: u64, names: &[String]) -> Connection {
    let (a,b)=if dir==0 {tuple} else {(tuple.1,tuple.0)};
    Connection {from:names[a.0].clone(),to:names[b.0].clone(),src_addr:a.1.into(),dst_addr:b.1.into(),src_port:a.2,dst_port:b.2,incarnation:id}
}
impl Connections {
    pub fn observe(&mut self, from: usize, to: usize, frame: &[u8], names: &[String]) -> Option<Connection> {
        let p=packet(frame)?;
        let (tuple,dir)=key((from,p.src,p.sport),(to,p.dst,p.dport));
        let opening=p.flags&0x12==2;
        if opening && self.sessions.get(&tuple).is_none_or(|s|s.closed || s.syn[dir].is_some_and(|seq|seq!=p.seq)) {
            let id=self.next.checked_add(1)?;
            self.next=id;
            if !self.sessions.contains_key(&tuple) && self.sessions.len()==CAPACITY {
                // Oldest admitted incarnation, independent of observation and host time.
                let oldest=*self.sessions.iter().min_by_key(|(_,s)|s.id)?.0;
                self.sessions.remove(&oldest);self.evicted+=1;
            }
            self.sessions.insert(tuple,Session {id,syn:[None,None],fin:[false,false],closed:false});
        }
        let s=self.sessions.get_mut(&tuple)?;
        if p.flags&2!=0 {
            if !opening && s.syn[1-dir].is_none_or(|seq|p.ack!=seq.wrapping_add(1)) { return None; }
            if s.syn[dir].is_some_and(|seq|seq!=p.seq) { return None; }
            s.syn[dir]=Some(p.seq);
        }
        if p.flags&1!=0 { s.fin[dir]=true; }
        if p.flags&4!=0 || s.fin==[true,true] { s.closed=true; }
        Some(connection(tuple,dir,s.id,names))
    }
    pub fn available(&self, c: &Connection, names: &[String]) -> bool {
        self.sessions.iter().any(|(&tuple,s)|!s.closed && s.id==c.incarnation && (0..2).any(|d|connection(tuple,d,s.id,names)==*c))
    }
    pub fn list(&self, from: &str, to: &str, after: u64, names: &[String]) -> Vec<Connection> {
        let mut result=Vec::new();
        for (&tuple,s) in &self.sessions {
            if s.closed || s.id<=after { continue; }
            for d in 0..2 {
                let c=connection(tuple,d,s.id,names);
                if c.from==from && c.to==to { result.push(c); }
            }
        }
        result.sort_by_key(|c|c.incarnation);result.truncate(32);result
    }
    pub fn select(&mut self, fault_id: u32, handle: String, c: Connection, names: &[String]) -> Result<(),String> {
        if self.selections.len()==CAPACITY { return Err("connection selector capacity exhausted".into()); }
        if !self.available(&c,names) { return Err("connection incarnation unavailable".into()); }
        if self.selections.contains_key(&handle) { return Err("duplicate connection handle".into()); }
        self.used=true;
        self.selections.insert(handle,Selection {fault_id,connection:c,matched:0,dropped:0});Ok(())
    }
    /// Called at send time, after classification, before link RNG/FIFO decisions.
    /// A partition masks selected loss; overlaps each retain their own evidence.
    pub fn drops(&mut self, c: Option<&Connection>, cut: bool) -> bool {
        let mut drop=false;
        for s in self.selections.values_mut() {
            if c==Some(&s.connection) { s.matched+=1; if !cut {s.dropped+=1;drop=true;} }
        }
        drop
    }
    pub fn witness(&self, h: &mut Sha256) {
        if !self.used { return; }
        h.update(b"seedbox-connections-v1");h.update(self.next.to_le_bytes());h.update(self.evicted.to_le_bytes());
        h.update((self.sessions.len() as u64).to_le_bytes());
        for (&(a,b),s) in &self.sessions {
            for (m,addr,port) in [a,b] {h.update((m as u64).to_le_bytes());h.update(addr);h.update(port.to_le_bytes());}
            h.update(s.id.to_le_bytes());
            for syn in s.syn {h.update([syn.is_some() as u8]);h.update(syn.unwrap_or(0).to_le_bytes());}
            h.update([s.fin[0] as u8,s.fin[1] as u8,s.closed as u8]);
        }
        h.update((self.selections.len() as u64).to_le_bytes());
        for (handle,s) in &self.selections {
            h.update(s.fault_id.to_le_bytes());crate::links::string_hash(h,handle);
            crate::links::string_hash(h,&serde_json::to_string(&s.connection).unwrap());
            h.update(s.matched.to_le_bytes());h.update(s.dropped.to_le_bytes());
        }
    }
}
