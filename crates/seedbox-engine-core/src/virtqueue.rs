//! Bounded split-ring DMA shared by the platform device adapters.
//! Addresses are guest physical addresses; segments and dirty ranges are RAM offsets.
use serde::{Deserialize, Serialize};

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Queue {
    pub num: u16,
    pub ready: bool,
    pub desc: u64,
    pub avail: u64,
    pub used: u64,
    pub last_avail: u16,
    pub used_idx: u16,
}

pub struct Segment {
    pub offset: usize,
    pub len: usize,
    pub writable: bool,
}

pub struct Chain {
    pub head: u16,
    pub segments: Vec<Segment>,
    pub len: usize,
    used: usize,
}

#[derive(Clone, Copy)]
pub enum Direction {
    Any,
    Uniform(bool),
    ReadThenWrite,
}

/// Preserve device-specific validation order, limits and diagnostics.
#[derive(Clone, Copy)]
pub struct Policy {
    pub limit: usize,
    pub direction: Direction,
    pub reject_empty: bool,
    pub diagnostics: Diagnostics,
}

#[derive(Clone, Copy)]
pub enum Diagnostics {
    Virtio,
    Console,
    Disk,
}

enum Invalid {
    Address,
    Span,
    Size,
    Alignment,
    Overrun,
    Cycle,
    Flags,
    ReadAfterWrite,
    Limit,
    Empty,
}

impl Diagnostics {
    fn error(self, invalid: Invalid) -> String {
        use Diagnostics::*;
        use Invalid::*;
        match (self, invalid) {
            (Virtio, Address) => "virtio DMA address is outside RAM",
            (Virtio, Span) => "virtio DMA span is outside RAM",
            (Virtio, Size) => "invalid virtio queue size",
            (Virtio, Alignment) => "unaligned virtio queue",
            (Virtio, Overrun) => "virtio available ring overrun",
            (Virtio, Cycle) => "invalid/cyclic virtio descriptor chain",
            (Virtio, Flags) => "unsupported virtio descriptor flags",
            (Virtio, Limit) => "virtio chain too large",
            (Virtio, Empty) => "empty virtio descriptor chain",
            (Console, Address) => "console DMA address overflow",
            (Console, Span) => "console DMA outside guest RAM",
            (Console, Size) => "invalid console queue size",
            (Console, Alignment) => "unaligned console queue",
            (Console, Overrun) => "console available ring overrun",
            (Console, Cycle) => "invalid/cyclic console descriptor chain",
            (Console, Flags) => "unsupported console descriptor flags/direction",
            (Console, Limit) => "console descriptor chain too large",
            (Console, Empty) => "empty console descriptor chain",
            (Disk, Address) => "disk DMA address overflow",
            (Disk, Span) => "disk DMA outside guest RAM",
            (Disk, Size) => "invalid disk queue size",
            (Disk, Alignment) => "unaligned disk queue",
            (Disk, Overrun) => "disk available ring overrun",
            (Disk, Cycle) => "invalid/cyclic disk descriptor chain",
            (Disk, Flags) => "unsupported disk descriptor flags",
            (Disk, Limit) => "disk request too large",
            (_, ReadAfterWrite) => "disk descriptor chain reads after it writes",
            (Disk, Empty) => "empty disk descriptor chain",
        }
        .into()
    }
}

fn span(
    memory: &[u8],
    base: u64,
    address: u64,
    len: usize,
    d: Diagnostics,
) -> Result<usize, String> {
    let offset = address
        .checked_sub(base)
        .and_then(|n| usize::try_from(n).ok())
        .ok_or_else(|| d.error(Invalid::Address))?;
    if offset.checked_add(len).is_none_or(|end| end > memory.len()) {
        return Err(d.error(Invalid::Span));
    }
    Ok(offset)
}

fn rd16(m: &[u8], at: usize) -> u16 {
    u16::from_le_bytes(m[at..at + 2].try_into().expect("validated DMA"))
}
fn rd32(m: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(m[at..at + 4].try_into().expect("validated DMA"))
}
fn rd64(m: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(m[at..at + 8].try_into().expect("validated DMA"))
}

/// Virtio split-ring event threshold, including u16 index wrap.
pub fn need_event(event: u16, new: u16, old: u16) -> bool {
    new.wrapping_sub(event).wrapping_sub(1) < new.wrapping_sub(old)
}

impl Queue {
    // Includes both event-index words, even when the feature is not negotiated.
    fn rings(
        &self,
        memory: &[u8],
        base: u64,
        d: Diagnostics,
    ) -> Result<Option<(usize, usize, usize)>, String> {
        if !self.ready {
            return Ok(None);
        }
        if self.num == 0 || self.num > 256 || !self.num.is_power_of_two() {
            return Err(d.error(Invalid::Size));
        }
        if self.desc & 15 != 0 || self.avail & 1 != 0 || self.used & 3 != 0 {
            return Err(d.error(Invalid::Alignment));
        }
        let desc = span(memory, base, self.desc, self.num as usize * 16, d)?;
        let avail = span(memory, base, self.avail, 6 + self.num as usize * 2, d)?;
        let used = span(memory, base, self.used, 6 + self.num as usize * 8, d)?;
        if rd16(memory, avail + 2).wrapping_sub(self.last_avail) > self.num {
            return Err(d.error(Invalid::Overrun));
        }
        Ok(Some((desc, avail, used)))
    }
    pub fn pending(&self, memory: &[u8], base: u64) -> Result<bool, String> {
        Ok(self
            .rings(memory, base, Diagnostics::Virtio)?
            .is_some_and(|(_, avail, _)| rd16(memory, avail + 2) != self.last_avail))
    }
    pub fn interrupt(
        &self,
        memory: &[u8],
        base: u64,
        event_idx: bool,
        old_used: u16,
    ) -> Result<bool, String> {
        let Some((_, avail, _)) = self.rings(memory, base, Diagnostics::Virtio)? else {
            return Ok(false);
        };
        Ok(if event_idx {
            need_event(
                rd16(memory, avail + 4 + 2 * self.num as usize),
                self.used_idx,
                old_used,
            )
        } else {
            rd16(memory, avail) & 1 == 0
        })
    }
    pub fn suppress_notify(
        &self,
        memory: &mut [u8],
        base: u64,
        suppress: bool,
        mut dirty: impl FnMut(usize, usize),
    ) -> Result<(), String> {
        let Some((_, _, used)) = self.rings(memory, base, Diagnostics::Virtio)? else {
            return Ok(());
        };
        let flags = rd16(memory, used);
        let next = if suppress { flags | 1 } else { flags & !1 };
        if flags != next {
            memory[used..used + 2].copy_from_slice(&next.to_le_bytes());
            dirty(used, 2);
        }
        Ok(())
    }
    pub fn arm_notify(
        &self,
        memory: &mut [u8],
        base: u64,
        event_idx: bool,
        deferred: bool,
        mut dirty: impl FnMut(usize, usize),
    ) -> Result<bool, String> {
        let Some((_, _, used)) = self.rings(memory, base, Diagnostics::Virtio)? else {
            return Ok(false);
        };
        if event_idx {
            let at = used + 4 + 8 * self.num as usize;
            let event = self
                .last_avail
                .wrapping_add(if deferred { self.num } else { 0 });
            if rd16(memory, at) != event {
                memory[at..at + 2].copy_from_slice(&event.to_le_bytes());
                dirty(at, 2);
            }
        } else {
            self.suppress_notify(memory, base, deferred, &mut dirty)?;
        }
        self.pending(memory, base)
    }
    /// Validate the complete rings and chain before any guest or device mutation.
    pub fn validated_chain(
        &self,
        memory: &[u8],
        base: u64,
        policy: Policy,
    ) -> Result<Option<Chain>, String> {
        let Some((desc, avail, used)) = self.rings(memory, base, policy.diagnostics)? else {
            return Ok(None);
        };
        let d = policy.diagnostics;
        let pending = rd16(memory, avail + 2).wrapping_sub(self.last_avail);
        if pending == 0 {
            return Ok(None);
        }
        let head = rd16(
            memory,
            avail + 4 + 2 * (self.last_avail % self.num) as usize,
        );
        let mut index = head;
        let mut seen = vec![false; self.num as usize];
        let mut segments: Vec<Segment> = Vec::new();
        let mut len = 0usize;
        loop {
            if index >= self.num || seen[index as usize] {
                return Err(d.error(Invalid::Cycle));
            }
            seen[index as usize] = true;
            let offset = desc + 16 * index as usize;
            let flags = rd16(memory, offset + 12);
            if flags & !3 != 0
                || matches!(policy.direction, Direction::Uniform(w) if (flags & 2 != 0) != w)
            {
                return Err(d.error(Invalid::Flags));
            }
            let writable = flags & 2 != 0;
            if matches!(policy.direction, Direction::ReadThenWrite)
                && !writable
                && segments.last().is_some_and(|s| s.writable)
            {
                return Err(d.error(Invalid::ReadAfterWrite));
            }
            let size = rd32(memory, offset + 8) as usize;
            len = len
                .checked_add(size)
                .filter(|n| *n <= policy.limit)
                .ok_or_else(|| d.error(Invalid::Limit))?;
            segments.push(Segment {
                offset: span(memory, base, rd64(memory, offset), size, d)?,
                len: size,
                writable,
            });
            if flags & 1 == 0 {
                break;
            }
            index = rd16(memory, offset + 14);
        }
        if policy.reject_empty && len == 0 {
            return Err(d.error(Invalid::Empty));
        }
        Ok(Some(Chain {
            head,
            segments,
            len,
            used,
        }))
    }

    /// Complete a validated chain, preserving the used-entry/index journal order.
    /// The adapter keeps RAM and queue configuration unchanged between validation and completion.
    pub fn complete_chain(
        &mut self,
        memory: &mut [u8],
        chain: &Chain,
        len: usize,
        mut dirty: impl FnMut(usize, usize),
    ) {
        let entry = chain.used + 4 + 8 * (self.used_idx % self.num) as usize;
        memory[entry..entry + 4].copy_from_slice(&(chain.head as u32).to_le_bytes());
        memory[entry + 4..entry + 8].copy_from_slice(&(len as u32).to_le_bytes());
        self.used_idx = self.used_idx.wrapping_add(1);
        let index = chain.used + 2;
        memory[index..index + 2].copy_from_slice(&self.used_idx.to_le_bytes());
        dirty(entry, 8);
        dirty(index, 2);
        self.last_avail = self.last_avail.wrapping_add(1);
    }
}

impl Chain {
    pub fn read(&self, memory: &[u8]) -> Result<Vec<u8>, String> {
        if self.segments.iter().any(|s| s.writable) {
            return Err("virtio transmit chain is writable".into());
        }
        let mut bytes = Vec::with_capacity(self.len);
        for s in &self.segments {
            bytes.extend_from_slice(&memory[s.offset..s.offset + s.len]);
        }
        Ok(bytes)
    }
    pub fn write(&self, memory: &mut [u8], bytes: &[u8]) -> Result<(), String> {
        if self.segments.iter().any(|s| !s.writable) || self.len < bytes.len() {
            return Err("invalid/short virtio receive chain".into());
        }
        let mut written = 0usize;
        for s in &self.segments {
            let n = s.len.min(bytes.len() - written);
            memory[s.offset..s.offset + n].copy_from_slice(&bytes[written..written + n]);
            written += n;
            if written == bytes.len() {
                break;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
#[path = "virtqueue_tests.rs"]
mod tests;
