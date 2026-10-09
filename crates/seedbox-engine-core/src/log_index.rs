//! Observation-only serial chunk index. Never participates in machine state or witnesses.
//! The engine writes it beside each raw UART stream; `seedbox logs` reads it back.
//! File layout: `MAGIC`, then one little-endian `ChunkRecord` per burst.
use std::io::{BufWriter, Write};

pub const MAGIC: &[u8; 8] = b"SBLOG001";
/// Records one stream's index may hold.
pub const MAX_CHUNKS: usize = 65_536;
/// Bytes of one encoded `ChunkRecord`.
pub const RECORD_BYTES: usize = 40;

/// Index filename shared by every UART sink and the retained-log reader.
pub fn index_name(raw: &str) -> String {
    format!("{raw}.chunks")
}

/// One burst of raw stream bytes. Encoded as five little-endian u64s in field order.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ChunkRecord {
    /// Virtual time of the burst's first byte.
    pub time_ns: u64,
    /// Machine incarnation; a valid record fits in u32.
    pub incarnation: u64,
    /// Machine-wide burst order across both streams.
    pub order: u64,
    /// Byte offset in the raw log.
    pub offset: u64,
    /// Byte length in the raw log.
    pub length: u64,
}

impl ChunkRecord {
    pub fn to_bytes(&self) -> [u8; RECORD_BYTES] {
        let mut bytes = [0u8; RECORD_BYTES];
        let fields = [
            self.time_ns,
            self.incarnation,
            self.order,
            self.offset,
            self.length,
        ];
        for (chunk, value) in bytes.chunks_exact_mut(8).zip(fields) {
            chunk.copy_from_slice(&value.to_le_bytes());
        }
        bytes
    }

    pub fn from_bytes(bytes: &[u8; RECORD_BYTES]) -> Self {
        let field = |i: usize| {
            let mut word = [0u8; 8];
            word.copy_from_slice(&bytes[i * 8..i * 8 + 8]);
            u64::from_le_bytes(word)
        };
        Self {
            time_ns: field(0),
            incarnation: field(1),
            order: field(2),
            offset: field(3),
            length: field(4),
        }
    }
}

#[derive(Default)]
pub struct Index {
    pub records: Vec<ChunkRecord>,
    pub file: Option<BufWriter<std::fs::File>>,
    pub limit: Option<usize>,
    pub seen: u64,
    pending: bool,
    failed: bool,
}
impl Index {
    /// Reopen a restored stream with its captured bytes and timed records.
    pub fn reopen_stream(
        &mut self,
        raw: &str,
        prefix: &[u8],
        stream: bool,
    ) -> Result<Option<std::fs::File>, String> {
        self.open(&index_name(raw));
        if !stream {
            return Ok(None);
        }
        let mut file = std::fs::File::create(raw).map_err(|e| e.to_string())?;
        file.write_all(prefix).map_err(|e| e.to_string())?;
        Ok(Some(file))
    }

    pub fn snapshot(&self) -> Self {
        Self {
            records: self.records.clone(),
            file: None,
            limit: self.limit,
            seen: self.seen,
            pending: self.pending,
            failed: self.failed,
        }
    }
    /// Sink failures disable indexing only. Raw bytes and execution keep their existing behavior.
    pub fn open(&mut self, path: &str) {
        self.finish();
        self.file = None;
        match std::fs::File::create(path).and_then(|f| {
            let mut f = BufWriter::new(f);
            f.write_all(MAGIC)?;
            for r in &self.records {
                f.write_all(&r.to_bytes())?;
            }
            f.flush()?;
            Ok(f)
        }) {
            Ok(file) => self.file = Some(file),
            Err(_) => self.failed = true,
        }
    }
    fn close(&mut self) {
        if !self.pending {
            return;
        }
        self.pending = false;
        if self.failed {
            return;
        }
        if let (Some(file), Some(record)) = (self.file.as_mut(), self.records.last()) {
            if file.write_all(&record.to_bytes()).is_err() {
                self.failed = true;
            }
        }
    }
    /// Close the burst and flush at every world-slice return, including errors.
    pub fn finish(&mut self) {
        self.close();
        if self.file.as_mut().is_some_and(|file| file.flush().is_err()) {
            self.failed = true;
        }
    }
    pub fn capture(&mut self, end: u64, time: u64, incarnation: u32, order: &mut u64) {
        if end <= self.seen {
            return;
        }
        let offset = self.seen;
        self.seen = end;
        if let Some(last) = self
            .records
            .last_mut()
            .filter(|_| !self.failed && self.pending)
        {
            // Only the latest machine-wide burst can grow. Keep its first
            // byte's time even when later bytes advance time in this slice.
            if last.incarnation == incarnation as u64
                && last.order.checked_add(1) == Some(*order)
                && last.offset + last.length == offset
            {
                last.length = end - last.offset;
                return;
            }
        }
        self.close();
        let record = ChunkRecord {
            time_ns: time,
            incarnation: incarnation as u64,
            order: *order,
            offset,
            length: end - offset,
        };
        *order = order.saturating_add(1);
        if self.failed || self.records.len() >= self.limit.unwrap_or(MAX_CHUNKS).min(MAX_CHUNKS) {
            return;
        }
        self.records.push(record);
        self.pending = true;
    }
}
impl Drop for Index {
    fn drop(&mut self) {
        self.finish();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_encode_as_five_little_endian_words_in_field_order() {
        let record = ChunkRecord {
            time_ns: 1,
            incarnation: 2,
            order: 3,
            offset: 4,
            length: 0x0102_0304_0506_0708,
        };
        let bytes = record.to_bytes();
        let mut expected = Vec::new();
        for value in [1u64, 2, 3, 4, 0x0102_0304_0506_0708] {
            expected.extend_from_slice(&value.to_le_bytes());
        }
        assert_eq!(bytes.as_slice(), expected);
        assert_eq!(ChunkRecord::from_bytes(&bytes), record);
    }
}
