//! KVM network adapter: guest address base, DMA journal and shared device state.
use seedbox_engine_core::virtio_mmio::{Transport,Spec,Policy as MmioPolicy,Effect};
use seedbox_engine_core::virtio_network::{Network,Policy,Stats};
pub use seedbox_engine_core::virtio_network::{Received as ReceiveOutcome,Notified as WriteOutcome};
#[derive(Clone)]
pub struct VirtioNet { transport:Transport, model:Network, pub dirty:Vec<(usize,usize)> }
impl VirtioNet {
 pub fn new(mac:[u8;6])->Self {Self{transport:Transport::new(Spec{device_id:1,features:[1<<5,1]},2,MmioPolicy::Network),model:Network::new(mac,Policy::Network),dirty:Vec::new()}}
 pub fn stats(&self)->&Stats {self.model.stats()}
 pub fn read(&mut self,off:u64)->u32 {
  if off>=0x100 {let mac=self.model.mac();let byte=|o:u64|mac.get(o as usize).copied().unwrap_or(0);let b=off-0x100;u32::from_le_bytes([byte(b),byte(b+1),byte(b+2),byte(b+3)])}else{self.transport.read(off)}
 }
 pub fn write(&mut self,m:&mut[u8],off:u64,v:u32)->Result<WriteOutcome,String> {
  match self.transport.write(off,v)? {
   Effect::None=>(),
   Effect::Reset=>self.model=Network::new(self.model.mac(),Policy::Network),
   Effect::Notify(index)=>return self.model.notify(&mut self.transport,m,0,index,|a,n|self.dirty.push((a,n))),
  }
  Ok(WriteOutcome::default())
 }
 pub fn receive(&mut self,m:&mut[u8],frame:&[u8])->Result<ReceiveOutcome,String> {self.model.receive(&mut self.transport,m,0,frame,|a,n|self.dirty.push((a,n)))}
}
