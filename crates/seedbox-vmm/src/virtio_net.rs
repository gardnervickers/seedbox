//! virtio-net over virtio-mmio (version 2), split virtqueues, no offloads.
//! Queue 0 receives, queue 1 transmits. All DMA happens on the world thread
//! while the guest is stopped.

use std::collections::VecDeque;

const HDR: usize = 12; // virtio_net_hdr with num_buffers (VERSION_1)
const QSIZE: u16 = 256;
const BACKLOG: usize = 256;

const CHAIN_LIMIT: usize = 4 << 20;
use seedbox_engine_core::virtqueue::{Chain, Diagnostics, Direction, Policy, Queue};
trait NetworkQueue {
    fn chain(&self, memory: &[u8], writable: bool) -> Result<Option<Chain>,String>;
}
impl NetworkQueue for Queue {
    fn chain(&self, memory: &[u8], writable: bool) -> Result<Option<Chain>,String> {
        self.validated_chain(memory,0,Policy {limit:CHAIN_LIMIT,direction:Direction::Uniform(writable),reject_empty:false,diagnostics:Diagnostics::Virtio})
    }
}

/// Receiver acceptance and IRQ notification are independent. A frame queued
/// in the bounded device backlog is accepted without an immediate interrupt.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ReceiveOutcome {
    pub accepted: bool,
    pub irq: bool,
}

pub struct WriteOutcome {
    pub irq: bool,
    pub frames: Vec<Vec<u8>>,
}

#[derive(Clone)]
pub struct VirtioNet {
    mac: [u8; 6],
    dev_sel: u32,
    drv_sel: u32,
    drv_features: [u32; 2],
    queue_sel: u32,
    q: [Queue; 2],
    status: u32,
    isr: u32,
    backlog: VecDeque<Vec<u8>>,
    pub tx_frames: u64,
    pub rx_frames: u64,
    pub rx_dropped: u64,
    /// Guest memory ranges (start, len) written by the device since last taken.
    pub dirty: Vec<(usize, usize)>,
}

impl VirtioNet {
    pub fn new(mac: [u8; 6]) -> Self {
        Self {
            mac,
            dev_sel: 0,
            drv_sel: 0,
            drv_features: [0; 2],
            queue_sel: 0,
            q: Default::default(),
            status: 0,
            isr: 0,
            backlog: VecDeque::new(),
            tx_frames: 0,
            rx_frames: 0,
            rx_dropped: 0,
            dirty: Vec::new(),
        }
    }

    fn config(&self, off: u64) -> u8 {
        match off {
            0..=5 => self.mac[off as usize],
            _ => 0,
        }
    }

    pub fn read(&mut self, off: u64) -> u32 {
        let qs = self.queue_sel as usize;
        match off {
            0x000 => 0x7472_6976,
            0x004 => 2,
            0x008 => 1,
            0x00c => 0x5345_4544,
            0x010 => match self.dev_sel {
                0 => 1 << 5, // VIRTIO_NET_F_MAC
                1 => 1,      // VIRTIO_F_VERSION_1
                _ => 0,
            },
            0x034 => {
                if qs < 2 {
                    QSIZE as u32
                } else {
                    0
                }
            }
            0x044 => {
                if qs < 2 {
                    self.q[qs].ready as u32
                } else {
                    0
                }
            }
            0x060 => self.isr,
            0x070 => self.status,
            0x0fc => 0,
            o if o >= 0x100 => {
                let b = o - 0x100;
                u32::from_le_bytes([
                    self.config(b),
                    self.config(b + 1),
                    self.config(b + 2),
                    self.config(b + 3),
                ])
            }
            _ => 0,
        }
    }

    pub fn write(&mut self, m: &mut [u8], off: u64, v: u32) -> Result<WriteOutcome,String> {
        let mut out = WriteOutcome {
            irq: false,
            frames: Vec::new(),
        };
        let qs = (self.queue_sel as usize).min(1);
        match off {
            0x014 => self.dev_sel = v,
            0x020 => {
                if self.drv_sel < 2 {
                    self.drv_features[self.drv_sel as usize] = v;
                }
            }
            0x024 => self.drv_sel = v,
            0x030 => self.queue_sel = v,
            0x038 => self.q[qs].num = (v as u16).min(QSIZE).max(1),
            0x044 => {
                let q = &mut self.q[qs];
                q.ready = v == 1;
                q.last_avail = 0;
                q.used_idx = 0;
            }
            0x050 => match v {
                1 => {
                    while let Some(chain) = self.q[1].chain(m,false)? {
                        let bytes = chain.read(m)?;
                        self.q[1].complete_chain(m,&chain,0,|a,n|self.dirty.push((a,n)));
                        if bytes.len() > HDR {
                            out.frames.push(bytes[HDR..].to_vec());
                            self.tx_frames += 1;
                        }
                        out.irq = true;
                    }
                }
                0 => out.irq = self.drain(m)?,
                _ => {}
            },
            0x064 => self.isr &= !v,
            0x070 => {
                self.status = v;
                if v == 0 {
                    let dirty = std::mem::take(&mut self.dirty);
                    *self = Self::new(self.mac);
                    self.dirty = dirty;
                }
            }
            0x080 => self.q[qs].desc = (self.q[qs].desc & !0xffff_ffff) | v as u64,
            0x084 => self.q[qs].desc = (self.q[qs].desc & 0xffff_ffff) | (v as u64) << 32,
            0x090 => self.q[qs].avail = (self.q[qs].avail & !0xffff_ffff) | v as u64,
            0x094 => self.q[qs].avail = (self.q[qs].avail & 0xffff_ffff) | (v as u64) << 32,
            0x0a0 => self.q[qs].used = (self.q[qs].used & !0xffff_ffff) | v as u64,
            0x0a4 => self.q[qs].used = (self.q[qs].used & 0xffff_ffff) | (v as u64) << 32,
            _ => {}
        }
        if out.irq {
            self.isr |= 1;
        }
        Ok(out)
    }

    fn put_frame(&mut self, m: &mut [u8], frame: &[u8]) -> Result<bool,String> {
        let Some(chain)=self.q[0].chain(m,true)? else {return Ok(false);};
        if chain.len < HDR+frame.len() {return Err("network receive buffer too small".into());}
        let mut data=vec![0u8;HDR];data[10]=1;data.extend_from_slice(frame);
        let mut written=0;
        for segment in &chain.segments {
            let n=segment.len.min(data.len()-written);
            m[segment.offset..segment.offset+n].copy_from_slice(&data[written..written+n]);
            self.dirty.push((segment.offset,n));written+=n;
            if written==data.len() {break;}
        }
        self.q[0].complete_chain(m,&chain,written,|a,n|self.dirty.push((a,n)));
        self.rx_frames+=1;
        Ok(true)
    }

    fn drain(&mut self, m: &mut [u8]) -> Result<bool,String> {
        let mut any = false;
        while let Some(f) = self.backlog.front() {
            let f = f.clone();
            if !self.put_frame(m, &f)? {
                break;
            }
            self.backlog.pop_front();
            any = true;
        }
        if any {
            self.isr |= 1;
        }
        Ok(any)
    }

    /// Deliver an inbound frame now, reporting acceptance separately from IRQ.
    pub fn receive(&mut self, m: &mut [u8], frame: &[u8]) -> Result<ReceiveOutcome,String> {
        if !self.q[0].ready {
            self.rx_dropped += 1;
            return Ok(ReceiveOutcome::default());
        }
        if frame.len()>CHAIN_LIMIT-HDR {return Err("network frame exceeds DMA limit".into());}
        if !self.backlog.is_empty() || !self.put_frame(m, frame)? {
            if self.backlog.len() >= BACKLOG {
                self.rx_dropped += 1;
                return Ok(ReceiveOutcome::default());
            }
            self.backlog.push_back(frame.to_vec());
            return Ok(ReceiveOutcome {
                accepted: true,
                irq: false,
            });
        }
        self.isr |= 1;
        Ok(ReceiveOutcome {
            accepted: true,
            irq: true,
        })
    }
}
