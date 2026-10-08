//! One named non-console virtio-console port over modern virtio-mmio.
//! Split-ring DMA is committed only while the guest is stopped on the world thread.
use std::collections::VecDeque;

const LIMIT: usize = 65536;
const DRIVER_OK: u32 = 4;
const MULTIPORT: u32 = 2;
const DEVICE_READY: u16 = 0;
const PORT_ADD: u16 = 1;
const PORT_READY: u16 = 3;
const PORT_OPEN: u16 = 6;
const PORT_NAME: u16 = 7;

use seedbox_engine_core::virtqueue::{Chain, Diagnostics, Direction, Policy, Queue};
use seedbox_engine_core::virtio_mmio::{Effect, Policy as MmioPolicy, Spec, Transport};
fn rd16(m: &[u8], a: usize) -> u16 { u16::from_le_bytes(m[a..a+2].try_into().unwrap()) }
fn rd32(m: &[u8], a: usize) -> u32 { u32::from_le_bytes(m[a..a+4].try_into().unwrap()) }
trait ConsoleQueue {
    fn chain(&self, memory: &[u8], writable: bool) -> Result<Option<Chain>, String>;
}
impl ConsoleQueue for Queue {
    fn chain(&self, memory: &[u8], writable: bool) -> Result<Option<Chain>, String> {
        self.validated_chain(memory, 0, Policy {
            limit: LIMIT, direction: Direction::Uniform(writable),
            reject_empty: true, diagnostics: Diagnostics::Console,
        })
    }
}
#[derive(Default)]
pub struct WriteOutcome { pub irq: bool, pub bytes: Vec<u8> }
#[derive(Default, Debug, PartialEq, Eq)]
pub struct ReceiveOutcome { pub accepted: bool, pub irq: bool }

#[derive(Clone)]
pub struct VirtioConsole {
    transport: Transport,
    device_ready: bool, port_ready: bool, guest_open: bool,
    rx: VecDeque<u8>, control_rx: VecDeque<Vec<u8>>,
    /// Every payload and used-ring write must be incorporated into undo's RAM journal.
    pub dirty: Vec<(usize,usize)>,
}
impl Default for VirtioConsole { fn default() -> Self { Self::new() } }
impl VirtioConsole {
    pub fn new() -> Self {
        Self { transport:Transport::new(Spec{device_id:3,features:[MULTIPORT,1]},4,MmioPolicy::Console),
            device_ready:false,port_ready:false,
            guest_open:false,rx:VecDeque::new(),control_rx:VecDeque::new(),dirty:Vec::new() }
    }
    pub fn pending_rx_bytes(&self) -> usize { self.rx.len() }
    pub fn read(&mut self, off: u64) -> u32 {
        if off==0x104 {1} else {self.transport.read(off)} // max_nr_ports
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
            if n>0 { m[segment.offset..segment.offset+n].copy_from_slice(&bytes[copied..copied+n]); self.dirty.push((segment.offset,n)); copied+=n; }
            if copied==bytes.len() { break; }
        }
    }
    fn drain(&mut self, m: &mut [u8]) -> Result<bool,String> {
        let mut irq=false;
        while let Some(bytes)=self.control_rx.front() {
            let Some(chain)=self.transport.queues[2].chain(m,true)? else {break;};
            if chain.len<bytes.len() { return Err("console control receive buffer too small".into()); }
            let bytes=self.control_rx.pop_front().unwrap();
            self.copy_in(m,&chain,&bytes); self.transport.queues[2].complete_chain(m,&chain,bytes.len(),|a,n|self.dirty.push((a,n))); irq=true;
        }
        while !self.rx.is_empty() && self.port_ready && self.guest_open {
            let Some(chain)=self.transport.queues[0].chain(m,true)? else {break;};
            let n=chain.len.min(self.rx.len()); let bytes:Vec<_>=self.rx.drain(..n).collect();
            self.copy_in(m,&chain,&bytes); self.transport.queues[0].complete_chain(m,&chain,n,|a,n|self.dirty.push((a,n))); irq=true;
        }
        if irq { self.transport.isr|=1; } Ok(irq)
    }
    pub fn receive(&mut self, m: &mut [u8], bytes: &[u8]) -> Result<ReceiveOutcome,String> {
        if self.transport.status & DRIVER_OK==0 { return Err("console driver not ready".into()); }
        if bytes.len()>LIMIT-self.rx.len() { return Err("console receive backlog full".into()); }
        self.rx.extend(bytes); let irq=self.drain(m)?; Ok(ReceiveOutcome{accepted:true,irq})
    }
    pub fn write(&mut self, m: &mut [u8], off: u64, v: u32) -> Result<WriteOutcome,String> {
        let mut out=WriteOutcome::default();
        match self.transport.write(off,v)? {
            Effect::Notify(v) => {
                if v==1||v==3 {
                    while let Some(chain)=self.transport.queues[v].chain(m,false)? {
                        let mut bytes=Vec::with_capacity(chain.len);
                        for s in &chain.segments {bytes.extend_from_slice(&m[s.offset..s.offset+s.len]);}
                        if v==3 {self.control(&bytes)?;} else {
                            if !self.port_ready||!self.guest_open{return Err("console transmit before port open".into());}
                            if bytes.len()>LIMIT-out.bytes.len(){return Err("console transmit batch too large".into());}
                            out.bytes.extend(bytes);
                        }
                        self.transport.queues[v].complete_chain(m,&chain,0,|a,n|self.dirty.push((a,n)));out.irq=true;
                    }
                }
                out.irq|=self.drain(m)?;
            }
            Effect::Reset => {let dirty=std::mem::take(&mut self.dirty);*self=Self::new();self.dirty=dirty;}
            Effect::None => {},
        }
        if out.irq{self.transport.isr|=1;} Ok(out)
    }
}
