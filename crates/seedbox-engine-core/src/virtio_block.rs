//! Shared block requests and disk effects over bounded split queues.
use crate::disk::{Disk, SECTOR};
use crate::virtio_mmio::Transport;
use crate::virtqueue::{Chain, Diagnostics, Direction, Policy as QueuePolicy};

type Result<T> = std::result::Result<T, String>;
const ID: &[u8; 20] = b"seedbox-disk\0\0\0\0\0\0\0\0";

/// Existing descriptor and effect policies, preserved during extraction.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Policy {
    Application,
    Block,
}

#[derive(Clone, Default, Debug, PartialEq, Eq)]
pub struct Stats {
    pub reads: u64,
    pub writes: u64,
    pub flushes: u64,
    pub sectors_read: u64,
    pub sectors_written: u64,
    pub sectors_flushed: u64,
    pub errors: u64,
}

#[derive(Default, Debug, PartialEq, Eq)]
pub struct Notified {
    pub completed: u64,
    pub flushed: u64,
    pub irq: bool,
    pub log: Vec<(u32, u64, u8)>,
}

#[derive(Clone)]
pub struct Block {
    disk: Disk,
    policy: Policy,
    // Application counters stay disabled and are not part of retained state.
    stats: Stats,
}

struct Request {
    kind: u32,
    sector: u64,
    input: Vec<u8>,
    output: Vec<(usize, usize)>,
    room: usize,
    status: usize,
}

impl Request {
    fn decode(memory: &[u8], chain: &Chain, policy: Policy) -> Result<Self> {
        let segments = &chain.segments;
        let (kind, sector, input, output, room, status) = match policy {
            Policy::Application => {
                if segments.len() < 2 {
                    return Err("short block descriptor chain".into());
                }
                let header = &segments[0];
                let status = segments.last().expect("checked descriptor count");
                if header.writable || header.len != 16 || !status.writable || status.len != 1 {
                    return Err("invalid block header/status descriptors".into());
                }
                let kind = u32::from_le_bytes(
                    memory[header.offset..header.offset + 4]
                        .try_into()
                        .expect("validated header"),
                );
                let sector = u64::from_le_bytes(
                    memory[header.offset + 8..header.offset + 16]
                        .try_into()
                        .expect("validated header"),
                );
                let data = &segments[1..segments.len() - 1];
                let len = data.iter().map(|s| s.len).sum::<usize>();
                match kind {
                    0 | 1
                        if !len.is_multiple_of(SECTOR)
                            || data.iter().any(|s| s.writable != (kind == 0)) =>
                    {
                        return Err("invalid block data descriptors".into())
                    }
                    4 if !data.is_empty() => {
                        return Err("block flush includes data descriptors".into())
                    }
                    8 if data.iter().any(|s| !s.writable) => {
                        return Err("block ID buffer is not writable".into())
                    }
                    _ => (),
                }
                let mut input = Vec::new();
                if kind == 1 {
                    for span in data {
                        input.extend_from_slice(&memory[span.offset..span.offset + span.len]);
                    }
                }
                (
                    kind,
                    sector,
                    input,
                    data.iter().map(|s| (s.offset, s.len)).collect(),
                    len,
                    status.offset,
                )
            }
            Policy::Block => {
                let readable = segments
                    .iter()
                    .filter(|s| !s.writable)
                    .map(|s| s.len)
                    .sum::<usize>();
                let writable = segments
                    .iter()
                    .filter(|s| s.writable)
                    .map(|s| s.len)
                    .sum::<usize>();
                if readable < 16 || writable < 1 {
                    return Err("disk request without a header or status byte".into());
                }
                // Copy all readable bytes before any disk or RAM write, including aliases.
                let mut bytes = Vec::with_capacity(readable);
                for span in segments.iter().filter(|s| !s.writable) {
                    bytes.extend_from_slice(&memory[span.offset..span.offset + span.len]);
                }
                let kind = u32::from_le_bytes(bytes[..4].try_into().expect("checked header"));
                let sector = u64::from_le_bytes(bytes[8..16].try_into().expect("checked header"));
                let room = writable - 1;
                let mut left = room;
                let mut output = Vec::new();
                let mut status = None;
                for span in segments.iter().filter(|s| s.writable) {
                    let n = span.len.min(left);
                    output.push((span.offset, n));
                    left -= n;
                    if left == 0 && n < span.len {
                        status = Some(span.offset + n);
                    }
                }
                (
                    kind,
                    sector,
                    bytes[16..].to_vec(),
                    output,
                    room,
                    status.expect("nonempty writable capacity"),
                )
            }
        };
        Ok(Self {
            kind,
            sector,
            input,
            output,
            room,
            status,
        })
    }
}

impl Block {
    pub fn new(disk: Disk, policy: Policy) -> Self {
        Self {
            disk,
            policy,
            stats: Stats::default(),
        }
    }
    pub fn into_disk(self) -> Disk {
        self.disk
    }
    pub fn disk(&self) -> &Disk {
        &self.disk
    }
    pub fn disk_mut(&mut self) -> &mut Disk {
        &mut self.disk
    }
    pub fn stats(&self) -> &Stats {
        &self.stats
    }

    /// The adapter supplies the one-queue transport and handles register resets.
    /// A reset retains this model: disk layers and KVM statistics survive it.
    pub fn notify(
        &mut self,
        transport: &mut Transport,
        memory: &mut [u8],
        base: u64,
        mut dirty: impl FnMut(usize, usize),
    ) -> Result<Notified> {
        let mut out = Notified::default();
        let policy = match self.policy {
            Policy::Application => QueuePolicy {
                limit: 4 << 20,
                direction: Direction::Any,
                reject_empty: true,
                diagnostics: Diagnostics::Virtio,
            },
            Policy::Block => QueuePolicy {
                limit: 4 << 20,
                direction: Direction::ReadThenWrite,
                reject_empty: false,
                diagnostics: Diagnostics::Disk,
            },
        };
        while let Some(chain) = transport.queues[0].validated_chain(memory, base, policy)? {
            let request = Request::decode(memory, &chain, self.policy)?;
            let mut data = Vec::new();
            let status = match request.kind {
                0 => {
                    data = vec![0; request.room];
                    if self.disk.read(request.sector, &mut data).is_ok() {
                        if self.policy == Policy::Block {
                            self.stats.reads += 1;
                            self.stats.sectors_read += (request.room / SECTOR) as u64;
                        }
                        0
                    } else {
                        data.clear();
                        1
                    }
                }
                1 => {
                    if self.disk.write(request.sector, &request.input).is_ok() {
                        if self.policy == Policy::Block {
                            self.stats.writes += 1;
                            self.stats.sectors_written += (request.input.len() / SECTOR) as u64;
                        }
                        0
                    } else {
                        1
                    }
                }
                4 => {
                    let n = self.disk.flush();
                    if self.policy == Policy::Block {
                        self.stats.flushes += 1;
                        self.stats.sectors_flushed += n;
                        out.flushed += n;
                    }
                    0
                }
                8 => {
                    data = ID[..request.room.min(ID.len())].to_vec();
                    0
                }
                _ => 2,
            };
            if self.policy == Policy::Block && status != 0 {
                self.stats.errors += 1;
            }
            let mut done = 0;
            for (offset, len) in request.output {
                let n = len.min(data.len().saturating_sub(done));
                if n > 0 {
                    memory[offset..offset + n].copy_from_slice(&data[done..done + n]);
                    dirty(offset, n);
                    done += n;
                }
            }
            memory[request.status] = status;
            dirty(request.status, 1);
            transport.queues[0].complete_chain(memory, &chain, done + 1, &mut dirty);
            out.irq = true;
            if self.policy == Policy::Application {
                transport.isr |= 1;
            } else {
                out.completed += 1;
                out.log.push((request.kind, request.sector, status));
            }
        }
        if out.irq {
            transport.isr |= 1;
        }
        Ok(out)
    }
}

#[cfg(test)]
#[path = "virtio_block_tests.rs"]
mod tests;
