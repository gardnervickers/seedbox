//! One named non-console virtio-console port over modern virtio-mmio.
//! Split-ring DMA is committed only while the guest is stopped on the world thread.
use std::collections::VecDeque;

const QSIZE: u16 = 256;
const LIMIT: usize = 65536;
const DRIVER_OK: u32 = 4;
const FEATURES_OK: u32 = 8;
const MULTIPORT: u32 = 2;
const DEVICE_READY: u16 = 0;
const PORT_ADD: u16 = 1;
const PORT_READY: u16 = 3;
const PORT_OPEN: u16 = 6;
const PORT_NAME: u16 = 7;

#[derive(Default, Clone)]
struct Queue {
    num: u16, ready: bool, desc: u64, avail: u64, used: u64,
    last_avail: u16, used_idx: u16,
}
struct Segment { addr: usize, len: usize }
struct Chain { head: u16, segments: Vec<Segment>, len: usize }

fn range(memory: &[u8], address: u64, len: usize) -> Result<usize, String> {
    let address = usize::try_from(address).map_err(|_| "console DMA address overflow")?;
    if address.checked_add(len).is_none_or(|end| end > memory.len()) {
        return Err("console DMA outside guest RAM".into());
    }
    Ok(address)
}
fn rd16(m: &[u8], a: usize) -> u16 { u16::from_le_bytes(m[a..a+2].try_into().unwrap()) }
fn rd32(m: &[u8], a: usize) -> u32 { u32::from_le_bytes(m[a..a+4].try_into().unwrap()) }
fn rd64(m: &[u8], a: usize) -> u64 { u64::from_le_bytes(m[a..a+8].try_into().unwrap()) }
impl Queue {
    fn chain(&self, memory: &[u8], writable: bool) -> Result<Option<Chain>, String> {
        if !self.ready { return Ok(None); }
        if self.num == 0 || self.num > QSIZE || !self.num.is_power_of_two() {
            return Err("invalid console queue size".into());
        }
        if self.desc & 15 != 0 || self.avail & 1 != 0 || self.used & 3 != 0 {
            return Err("unaligned console queue".into());
        }
        let desc = range(memory, self.desc, self.num as usize * 16)?;
        let avail = range(memory, self.avail, 6 + self.num as usize * 2)?;
        range(memory, self.used, 6 + self.num as usize * 8)?;
        let pending = rd16(memory, avail+2).wrapping_sub(self.last_avail);
        if pending > self.num { return Err("console available ring overrun".into()); }
        if pending == 0 { return Ok(None); }
        let head = rd16(memory, avail+4+2*(self.last_avail % self.num) as usize);
        let mut i = head;
        let mut seen = vec![false; self.num as usize];
        let mut segments = Vec::new();
        let mut len = 0usize;
        loop {
            if i >= self.num || seen[i as usize] { return Err("invalid/cyclic console descriptor chain".into()); }
            seen[i as usize] = true;
            let d = desc + i as usize*16;
            let flags = rd16(memory,d+12);
            if flags & !3 != 0 || (flags & 2 != 0) != writable {
                return Err("unsupported console descriptor flags/direction".into());
            }
            let size = rd32(memory,d+8) as usize;
            len = len.checked_add(size).filter(|n| *n <= LIMIT).ok_or("console descriptor chain too large")?;
            segments.push(Segment { addr:range(memory,rd64(memory,d),size)?,len:size });
            if flags & 1 == 0 { break; }
            i = rd16(memory,d+14);
        }
        if len == 0 { return Err("empty console descriptor chain".into()); }
        Ok(Some(Chain { head,segments,len }))
    }
    fn complete(&mut self, m: &mut [u8], dirty: &mut Vec<(usize,usize)>, head: u16, len: usize) {
        // chain() validated the complete used ring before any DMA.
        let entry = self.used as usize+4+8*(self.used_idx % self.num) as usize;
        m[entry..entry+4].copy_from_slice(&(head as u32).to_le_bytes());
        m[entry+4..entry+8].copy_from_slice(&(len as u32).to_le_bytes());
        self.used_idx = self.used_idx.wrapping_add(1);
        let index = self.used as usize+2;
        m[index..index+2].copy_from_slice(&self.used_idx.to_le_bytes());
        dirty.push((entry,8)); dirty.push((index,2));
        self.last_avail = self.last_avail.wrapping_add(1);
    }
}
#[derive(Default)]
pub struct WriteOutcome { pub irq: bool, pub bytes: Vec<u8> }
#[derive(Default, Debug, PartialEq, Eq)]
pub struct ReceiveOutcome { pub accepted: bool, pub irq: bool }

#[derive(Clone)]
pub struct VirtioConsole {
    dev_sel: u32, drv_sel: u32, drv_features: [u32;2], queue_sel: u32,
    q: [Queue;4], status: u32, isr: u32,
    device_ready: bool, port_ready: bool, guest_open: bool,
    rx: VecDeque<u8>, control_rx: VecDeque<Vec<u8>>,
    /// Every payload and used-ring write must be incorporated into undo's RAM journal.
    pub dirty: Vec<(usize,usize)>,
}
impl Default for VirtioConsole { fn default() -> Self { Self::new() } }
impl VirtioConsole {
    pub fn new() -> Self {
        Self { dev_sel:0,drv_sel:0,drv_features:[0;2],queue_sel:0,
            q:Default::default(),status:0,isr:0,device_ready:false,port_ready:false,
            guest_open:false,rx:VecDeque::new(),control_rx:VecDeque::new(),dirty:Vec::new() }
    }
    pub fn pending_rx_bytes(&self) -> usize { self.rx.len() }
    pub fn read(&mut self, off: u64) -> u32 {
        match off {
            0x000 => 0x7472_6976, 0x004 => 2, 0x008 => 3, 0x00c => 0x5345_4544,
            0x010 => match self.dev_sel { 0=>MULTIPORT,1=>1,_=>0 },
            0x034 => if self.queue_sel < 4 { QSIZE as u32 } else {0},
            0x044 => self.q.get(self.queue_sel as usize).map_or(0,|q|q.ready as u32),
            0x060 => self.isr, 0x070 => self.status, 0x0fc => 0,
            0x104 => 1, // max_nr_ports, MULTIPORT config field
            _ => 0,
        }
    }
    fn packet(&mut self, event: u16, value: u16, data: &[u8]) -> Result<(),String> {
        if self.control_rx.len() >= 16 { return Err("console control backlog full".into()); }
        let mut bytes=Vec::with_capacity(8+data.len());
        bytes.extend_from_slice(&0u32.to_le_bytes());
        bytes.extend_from_slice(&event.to_le_bytes()); bytes.extend_from_slice(&value.to_le_bytes());
        bytes.extend_from_slice(data); self.control_rx.push_back(bytes); Ok(())
    }
    fn control(&mut self, bytes: &[u8]) -> Result<(),String> {
        if bytes.len()!=8 { return Err("invalid console control packet length".into()); }
        let id=rd32(bytes,0); let event=rd16(bytes,4); let value=rd16(bytes,6);
        match event {
            DEVICE_READY if id==u32::MAX && value==1 && !self.device_ready => {
                self.device_ready=true; self.packet(PORT_ADD,1,&[])?;
            }
            PORT_READY if id==0 && value==1 && self.device_ready && !self.port_ready => {
                self.port_ready=true; self.packet(PORT_NAME,1,b"seedbox")?; self.packet(PORT_OPEN,1,&[])?;
            }
            PORT_OPEN if id==0 && value<=1 && self.port_ready => self.guest_open=value==1,
            _ => return Err("invalid console control handshake".into()),
        }
        Ok(())
    }
    fn copy_in(&mut self, m: &mut [u8], chain: &Chain, bytes: &[u8]) {
        let mut copied=0;
        for segment in &chain.segments {
            let n=segment.len.min(bytes.len()-copied);
            if n>0 { m[segment.addr..segment.addr+n].copy_from_slice(&bytes[copied..copied+n]); self.dirty.push((segment.addr,n)); copied+=n; }
            if copied==bytes.len() { break; }
        }
    }
    fn drain(&mut self, m: &mut [u8]) -> Result<bool,String> {
        let mut irq=false;
        while let Some(bytes)=self.control_rx.front() {
            let Some(chain)=self.q[2].chain(m,true)? else {break;};
            if chain.len<bytes.len() { return Err("console control receive buffer too small".into()); }
            let bytes=self.control_rx.pop_front().unwrap();
            self.copy_in(m,&chain,&bytes); self.q[2].complete(m,&mut self.dirty,chain.head,bytes.len()); irq=true;
        }
        while !self.rx.is_empty() && self.port_ready && self.guest_open {
            let Some(chain)=self.q[0].chain(m,true)? else {break;};
            let n=chain.len.min(self.rx.len()); let bytes:Vec<_>=self.rx.drain(..n).collect();
            self.copy_in(m,&chain,&bytes); self.q[0].complete(m,&mut self.dirty,chain.head,n); irq=true;
        }
        if irq { self.isr|=1; } Ok(irq)
    }
    pub fn receive(&mut self, m: &mut [u8], bytes: &[u8]) -> Result<ReceiveOutcome,String> {
        if self.status & DRIVER_OK==0 { return Err("console driver not ready".into()); }
        if bytes.len()>LIMIT-self.rx.len() { return Err("console receive backlog full".into()); }
        self.rx.extend(bytes); let irq=self.drain(m)?; Ok(ReceiveOutcome{accepted:true,irq})
    }
    pub fn write(&mut self, m: &mut [u8], off: u64, v: u32) -> Result<WriteOutcome,String> {
        let mut out=WriteOutcome::default();
        match off {
            0x014 => self.dev_sel=v,
            0x020 => if self.drv_sel<2 {self.drv_features[self.drv_sel as usize]=v;},
            0x024 => self.drv_sel=v,
            0x030 => self.queue_sel=v,
            0x038|0x044|0x080|0x084|0x090|0x094|0x0a0|0x0a4 => {
                let q=self.q.get_mut(self.queue_sel as usize).ok_or("invalid console queue selector")?;
                if q.ready && off!=0x044 { return Err("console queue changed while ready".into()); }
                match off {
                    0x038 => { if v==0||v>QSIZE as u32||!v.is_power_of_two(){return Err("invalid console queue size".into());} q.num=v as u16; }
                    0x044 => { if v>1{return Err("invalid console queue ready value".into());} q.ready=v==1; if v==0{q.last_avail=0;q.used_idx=0;} }
                    0x080 => q.desc=(q.desc & !0xffff_ffff)|v as u64,
                    0x084 => q.desc=(q.desc & 0xffff_ffff)|((v as u64)<<32),
                    0x090 => q.avail=(q.avail & !0xffff_ffff)|v as u64,
                    0x094 => q.avail=(q.avail & 0xffff_ffff)|((v as u64)<<32),
                    0x0a0 => q.used=(q.used & !0xffff_ffff)|v as u64,
                    0x0a4 => q.used=(q.used & 0xffff_ffff)|((v as u64)<<32),
                    _=>unreachable!(),
                }
            }
            0x050 => {
                if v>=4 { return Err("invalid console notify queue".into()); }
                if self.status & DRIVER_OK==0 { return Err("console notify before DRIVER_OK".into()); }
                if v==1||v==3 {
                    while let Some(chain)=self.q[v as usize].chain(m,false)? {
                        let mut bytes=Vec::with_capacity(chain.len);
                        for s in &chain.segments {bytes.extend_from_slice(&m[s.addr..s.addr+s.len]);}
                        if v==3 {self.control(&bytes)?;} else {
                            if !self.port_ready||!self.guest_open{return Err("console transmit before port open".into());}
                            if bytes.len()>LIMIT-out.bytes.len(){return Err("console transmit batch too large".into());}
                            out.bytes.extend(bytes);
                        }
                        self.q[v as usize].complete(m,&mut self.dirty,chain.head,0);out.irq=true;
                    }
                }
                out.irq|=self.drain(m)?;
            }
            0x064 => self.isr &= !v,
            0x070 => {
                if v==0 {let dirty=std::mem::take(&mut self.dirty);*self=Self::new();self.dirty=dirty;}
                else {
                    self.status=v;
                    if v & FEATURES_OK!=0 && self.drv_features!=[MULTIPORT,1] {self.status &= !FEATURES_OK;}
                    if v & DRIVER_OK!=0 && self.status & FEATURES_OK==0{return Err("console DRIVER_OK without negotiated features".into());}
                }
            }
            _=>{},
        }
        if out.irq{self.isr|=1;} Ok(out)
    }
}
