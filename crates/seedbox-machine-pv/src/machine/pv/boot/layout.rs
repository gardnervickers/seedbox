//! Private board RAM authority. Capacity excludes fixed device apertures;
//! offsets into sparse backing remain GPA minus base, never packed offsets.
use std::ops::Range;
pub(super) const ALIGN: u64 = 16384;
pub(super) const VERSION: u32 = 1;
const X86_HOLES: [Range<u64>; 2] = [0x0a00_0000..0x0b03_4000, 0xfec0_0000..0x1_0000_0000];
#[derive(Clone, Debug)]
pub(crate) struct RamLayout {
    pub capacity: u64,
    pub base: u64,
    pub span: usize,
    pub segments: Vec<Range<u64>>,
}
impl RamLayout {
    /// Validate resource arithmetic before allocation. Mapped segments contain
    /// exactly capacity bytes; device holes never count as RAM or allow DMA.
    // This one layout governs boot, native slots, DMA and capture; a backing
    // slice length alone would include MMIO holes.
    pub fn native(capacity: u64) -> Result<Self, String> {
        #[cfg(target_arch = "x86_64")]
        {
            Self::build(capacity, super::RAM_BASE, &X86_HOLES, u64::MAX)
        }
        #[cfg(target_arch = "aarch64")]
        {
            Self::build(capacity, super::RAM_BASE, &[], 1 << 32)
        }
    }
    #[cfg(test)]
    pub fn arm_test(capacity: u64) -> Result<Self, String> {
        Self::build(capacity, 0x4000_0000, &[], 1 << 32)
    }
    fn build(capacity: u64, base: u64, holes: &[Range<u64>], limit: u64) -> Result<Self, String> {
        if capacity == 0 || !capacity.is_multiple_of(ALIGN) {
            return Err("PV memory must be a positive multiple of 16 KiB".into());
        }
        let mut remaining = capacity;
        let mut at = base;
        let mut segments = Vec::new();
        for hole in holes {
            let length = remaining.min(hole.start.checked_sub(at).ok_or("PV RAM aperture order")?);
            if length != 0 {
                segments.push(at..at.checked_add(length).ok_or("PV RAM overflow")?);
            }
            at = at.checked_add(length).ok_or("PV RAM overflow")?;
            remaining -= length;
            if remaining == 0 {
                break;
            }
            at = hole.end;
        }
        if remaining != 0 {
            let end = at.checked_add(remaining).ok_or("PV RAM overflow")?;
            segments.push(at..end);
            at = end;
        }
        if at > limit {
            return Err("PV RAM exceeds the ARM 32-bit GPA layout".into());
        }
        let span = usize::try_from(at.checked_sub(base).ok_or("PV RAM span")?)
            .map_err(|_| "PV RAM exceeds host usize")?;
        Ok(Self {
            capacity,
            base,
            span,
            segments,
        })
    }
    pub fn offset(&self, address: u64, length: usize) -> Result<usize, String> {
        let offset = Self::offset_in_span(self.base, self.span, address, length)?;
        if length != 0
            && !self.segments.iter().any(|r| {
                address >= r.start
                    && address
                        .checked_add(length as u64)
                        .is_some_and(|end| end <= r.end)
            })
        {
            return Err("virtio range outside RAM".into());
        }
        Ok(offset)
    }
    /// Validate a complete guest RAM span, not just its endpoints. The host
    /// backing includes holes and must never make them DMA-addressable.
    pub fn offset_in_span(
        base: u64,
        span: usize,
        address: u64,
        length: usize,
    ) -> Result<usize, String> {
        let offset = address
            .checked_sub(base)
            .and_then(|v| usize::try_from(v).ok())
            .ok_or("virtio address outside RAM")?;
        let end = address
            .checked_add(length as u64)
            .ok_or("virtio range outside RAM")?;
        if offset.checked_add(length).is_none_or(|v| v > span) {
            return Err("virtio range outside RAM".into());
        }
        if length == 0 && offset == span {
            return Ok(offset);
        }
        // This board uses GPA base 0 on x86; ARM starts at 1 GiB without holes.
        if base == 0
            && X86_HOLES.iter().any(|h| {
                if length == 0 {
                    address >= h.start && address < h.end
                } else {
                    address < h.end && end > h.start
                }
            })
        {
            return Err("virtio range outside RAM".into());
        }
        Ok(offset)
    }
    #[cfg(any(target_arch = "aarch64", test))]
    pub fn top(&self) -> u64 {
        self.base + self.span as u64
    }
    #[cfg(any(target_arch = "aarch64", test))]
    pub fn dtb(&self) -> Result<u64, String> {
        self.top()
            .checked_sub(65536)
            .filter(|v| *v >= self.base)
            .ok_or_else(|| "PV RAM too small for DTB".into())
    }
    /// Highest aligned contiguous RAM placement under a boot-protocol ceiling.
    #[cfg(any(target_arch = "x86_64", test))]
    pub fn place(
        &self,
        length: u64,
        ceiling: u64,
        floor: u64,
        alignment: u64,
    ) -> Result<u64, String> {
        for region in self.segments.iter().rev() {
            let top = region.end.min(ceiling);
            if let Some(start) = top.checked_sub(length) {
                let start = start & !(alignment - 1);
                if start >= region.start.max(floor) && length != 0 {
                    return Ok(start);
                }
            }
        }
        Err("PV initrd is empty or exceeds RAM layout".into())
    }
}

#[cfg(test)]
mod tests;
