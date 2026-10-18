//! Closed backend snapshot framing support, shared by concrete backend codecs.
//!
//! These are logical quotas, not allocator/RSS isolation or native-restore
//! admission. Native register contents and device state versus RAM still require
//! native restore validation. Integrity of a snapshot is not authenticity.
// TODO: Re-evaluate: concrete backends need the same wire and budget accounting
// across crate boundaries; no serializer trait or machine inspection is exposed.
use std::io::{self, Read, Write};

/// Caller-selected limits for one owned backend snapshot decode. All limits
/// apply before allocation. Logical allocation includes retained element/byte
/// storage, explicit scratch and ownership copies, excluding allocator overhead,
/// stack and resources allocated by a later native restore. Item limits separately
/// bound collection overhead. The fixed schema has bounded nesting.
// TODO: Re-evaluate: backend imports share explicit logical quotas, not a
// process sandbox or a native resource-admission policy.
#[derive(Clone, Copy, Debug)]
pub struct DecodeLimits {
    /// Whole object including the fixed version/architecture header.
    pub encoded_bytes: u64,
    /// Cumulative logical retained and scratch storage, including copies.
    pub allocation_bytes: u64,
    /// Maximum elements in one non-byte collection or explicit scratch index.
    /// Byte/string buffers use blob/string/allocation quotas, not item quotas.
    pub collection_items: u64,
    /// Aggregate non-byte collection and explicit scratch-index elements.
    /// Byte/string buffers do not consume items; their bytes remain charged.
    pub total_items: u64,
    /// UTF-8 bytes in any one string.
    pub string_bytes: u64,
    /// Raw bytes in any one byte vector or length-declared body. A live PV
    /// RAM body includes its 16,384-byte page plus configured RAM capacity;
    /// callers must cover that sum, not only the configured capacity.
    pub blob_bytes: u64,
}
/// Invalid or unsupported records, explicit quota exhaustion, or ordinary I/O.
#[derive(Debug)]
pub enum CodecError {
    Invalid(&'static str),
    Limit(&'static str),
    Io(io::Error),
}
impl std::fmt::Display for CodecError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Invalid(s) => write!(f, "invalid backend snapshot: {s}"),
            Self::Limit(s) => write!(f, "backend snapshot decode limit: {s}"),
            Self::Io(e) => write!(f, "backend snapshot I/O: {e}"),
        }
    }
}
impl std::error::Error for CodecError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            _ => None,
        }
    }
}
impl From<io::Error> for CodecError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}
pub type Result<T> = std::result::Result<T, CodecError>;

/// A finite field-ordered body reader. `header_bytes` counts the already-read
/// fixed header toward the whole encoded limit. It never reads outside the
/// declared body except for the single-byte EOF check in `finish`.
// TODO: Re-evaluate: shared bridge records must consume the backend's remaining
// wire and logical quotas, rather than start a new independent budget.
pub struct Reader<'a, R> {
    input: &'a mut R,
    remaining: u64,
    limits: DecodeLimits,
    allocated: u64,
    items: u64,
}
impl<'a, R: Read> Reader<'a, R> {
    pub fn new(
        input: &'a mut R,
        body: u64,
        header_bytes: u64,
        limits: &DecodeLimits,
    ) -> Result<Self> {
        if body
            .checked_add(header_bytes)
            .is_none_or(|n| n > limits.encoded_bytes)
        {
            return Err(CodecError::Limit("encoded bytes"));
        }
        Ok(Self {
            input,
            remaining: body,
            limits: *limits,
            allocated: 0,
            items: 0,
        })
    }
    pub fn remaining(&self) -> u64 {
        self.remaining
    }
    /// Charge logical allocation cumulatively, including temporary copies.
    /// Conservatively charging released scratch again is allowed.
    pub fn charge(&mut self, bytes: u64) -> Result<()> {
        self.allocated = self
            .allocated
            .checked_add(bytes)
            .ok_or(CodecError::Limit("allocation bytes"))?;
        if self.allocated > self.limits.allocation_bytes {
            return Err(CodecError::Limit("allocation bytes"));
        }
        Ok(())
    }
    pub fn exact(&mut self, bytes: &mut [u8]) -> Result<()> {
        let count = bytes.len() as u64;
        if count > self.remaining {
            return Err(CodecError::Invalid("truncated body"));
        }
        self.input.read_exact(bytes)?;
        self.remaining -= count;
        Ok(())
    }
    pub fn u8(&mut self) -> Result<u8> {
        let mut v = [0; 1];
        self.exact(&mut v)?;
        Ok(v[0])
    }
    pub fn u16(&mut self) -> Result<u16> {
        let mut v = [0; 2];
        self.exact(&mut v)?;
        Ok(u16::from_le_bytes(v))
    }
    pub fn u32(&mut self) -> Result<u32> {
        let mut v = [0; 4];
        self.exact(&mut v)?;
        Ok(u32::from_le_bytes(v))
    }
    pub fn u64(&mut self) -> Result<u64> {
        let mut v = [0; 8];
        self.exact(&mut v)?;
        Ok(u64::from_le_bytes(v))
    }
    pub fn boolean(&mut self) -> Result<bool> {
        match self.u8()? {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(CodecError::Invalid("boolean tag")),
        }
    }
    /// Read a count, prove its minimum remaining wire size and charge its
    /// logical element storage and item quotas before any reservation.
    pub fn count<T>(&mut self, minimum_wire_bytes: u64) -> Result<usize> {
        let n = self.u64()?;
        if minimum_wire_bytes == 0 || n > self.remaining / minimum_wire_bytes {
            return Err(CodecError::Invalid("collection count exceeds body"));
        }
        self.claim_items(n)?;
        self.charge(
            n.checked_mul(std::mem::size_of::<T>() as u64)
                .ok_or(CodecError::Limit("element storage"))?,
        )?;
        usize::try_from(n).map_err(|_| CodecError::Limit("host collection length"))
    }
    /// Charge additional element identities (for a separately retained index).
    pub fn claim_items(&mut self, n: u64) -> Result<()> {
        if n > self.limits.collection_items {
            return Err(CodecError::Limit("collection items"));
        }
        self.items = self
            .items
            .checked_add(n)
            .ok_or(CodecError::Limit("aggregate items"))?;
        if self.items > self.limits.total_items {
            return Err(CodecError::Limit("aggregate items"));
        }
        Ok(())
    }
    /// Validate and charge one length-prefixed byte body before reading it.
    /// The caller may append it directly into its final retained representation.
    pub fn blob_length(&mut self) -> Result<usize> {
        let n = self.u64()?;
        if n > self.remaining {
            return Err(CodecError::Invalid("blob exceeds body"));
        }
        if n > self.limits.blob_bytes {
            return Err(CodecError::Limit("blob bytes"));
        }
        self.charge(n)?;
        usize::try_from(n).map_err(|_| CodecError::Limit("host blob length"))
    }
    pub fn bytes(&mut self) -> Result<Vec<u8>> {
        let n = self.blob_length()?;
        let mut out = Vec::new();
        reserve(&mut out, n)?;
        out.resize(n, 0);
        for chunk in out.chunks_mut(65536) {
            self.exact(chunk)?;
        }
        Ok(out)
    }
    pub fn string(&mut self) -> Result<String> {
        let n = self.u64()?;
        if n > self.remaining {
            return Err(CodecError::Invalid("string exceeds body"));
        }
        if n > self.limits.string_bytes {
            return Err(CodecError::Limit("string bytes"));
        }
        self.charge(n)?;
        let n = usize::try_from(n).map_err(|_| CodecError::Limit("host string length"))?;
        let mut out = Vec::new();
        reserve(&mut out, n)?;
        out.resize(n, 0);
        for chunk in out.chunks_mut(65536) {
            self.exact(chunk)?;
        }
        String::from_utf8(out).map_err(|_| CodecError::Invalid("UTF-8"))
    }
    /// Consume exactly the declared body, then require EOF. On an arbitrary
    /// blocking reader, this EOF check has that reader's normal blocking behavior.
    pub fn finish(self) -> Result<()> {
        if self.remaining != 0 {
            return Err(CodecError::Invalid("unconsumed body"));
        }
        let mut byte = [0];
        loop {
            match self.input.read(&mut byte) {
                Ok(0) => return Ok(()),
                Ok(_) => return Err(CodecError::Invalid("trailing bytes")),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e.into()),
            }
        }
    }
}
/// Reserve only after the owning parser has charged lengths and element storage.
pub fn reserve<T>(v: &mut Vec<T>, n: usize) -> Result<()> {
    v.try_reserve_exact(n)
        .map_err(|_| CodecError::Limit("allocation failed"))
}
/// Streaming field writer. All raw writes are at most 64 KiB. A sink writer
/// supplies exact encoded sizing without staging a body. Failure leaves an
/// ordinary caller-owned partial output and does not roll it back.
// TODO: Re-evaluate: the bridge and both backends share field encoding, not
// arbitrary serde serialization or a registry of caller-defined records.
pub struct Writer<'a, W> {
    output: &'a mut W,
    count: u64,
}
impl<'a, W: Write> Writer<'a, W> {
    pub fn new(output: &'a mut W) -> Self {
        Self { output, count: 0 }
    }
    pub fn count(&self) -> u64 {
        self.count
    }
    pub fn raw(&mut self, bytes: &[u8]) -> Result<()> {
        self.count = self
            .count
            .checked_add(bytes.len() as u64)
            .ok_or(CodecError::Invalid("encoded length overflow"))?;
        for chunk in bytes.chunks(65536) {
            self.output.write_all(chunk)?;
        }
        Ok(())
    }
    pub fn u8(&mut self, v: u8) -> Result<()> {
        self.raw(&[v])
    }
    pub fn u16(&mut self, v: u16) -> Result<()> {
        self.raw(&v.to_le_bytes())
    }
    pub fn u32(&mut self, v: u32) -> Result<()> {
        self.raw(&v.to_le_bytes())
    }
    pub fn u64(&mut self, v: u64) -> Result<()> {
        self.raw(&v.to_le_bytes())
    }
    pub fn boolean(&mut self, v: bool) -> Result<()> {
        self.u8(u8::from(v))
    }
    pub fn bytes(&mut self, v: &[u8]) -> Result<()> {
        self.u64(v.len() as u64)?;
        self.raw(v)
    }
    pub fn string(&mut self, v: &str) -> Result<()> {
        self.bytes(v.as_bytes())
    }
}

pub(crate) mod record;
