//! Observation-only serial chunk index. Never participates in machine state or witnesses.
//! The engine writes it beside each raw UART stream; `seedbox logs` reads it back.
//! File layout: `MAGIC`, then one little-endian `ChunkRecord` per burst.
use std::io::{BufWriter, Write};

pub use seedbox_engine_protocol::log_index::*;

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
#[path = "log_index_tests.rs"]
mod tests;
