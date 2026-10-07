//! Observation-only serial chunk index. Never participates in machine state or witnesses.
use std::io::{BufWriter,Write};

pub const MAGIC: &[u8;8] = b"SBLOG001";
pub const MAX_CHUNKS: usize = 65_536;

#[derive(Default)]
pub struct Index {
    pub records: Vec<[u64;5]>,
    pub file: Option<BufWriter<std::fs::File>>,
    pub limit: Option<usize>,
    pub seen: u64,
    pending: bool,
    failed: bool,
}
impl Index {
    pub fn snapshot(&self) -> Self {
        Self {records:self.records.clone(),file:None,limit:self.limit,seen:self.seen,pending:self.pending,failed:self.failed}
    }
    /// Sink failures disable indexing only. Raw bytes and execution keep their existing behavior.
    pub fn open(&mut self,path:&str) {
        self.finish();
        self.file=None;
        match std::fs::File::create(path).and_then(|f|{let mut f=BufWriter::new(f);f.write_all(MAGIC)?;for r in &self.records {for v in r {f.write_all(&v.to_le_bytes())?;}}f.flush()?;Ok(f)}) {
            Ok(file)=>self.file=Some(file),
            Err(_)=>self.failed=true,
        }
    }
    fn close(&mut self) {
        if !self.pending {return;}
        self.pending=false;
        if self.failed {return;}
        if let (Some(file),Some(record))=(self.file.as_mut(),self.records.last()) {
            let mut bytes=[0u8;40];
            for (i,v) in record.iter().enumerate() {bytes[i*8..i*8+8].copy_from_slice(&v.to_le_bytes());}
            if file.write_all(&bytes).is_err() {self.failed=true;}
        }
    }
    /// Close the burst and flush at every world-slice return, including errors.
    pub fn finish(&mut self) {
        self.close();
        if let Some(file)=self.file.as_mut() {if file.flush().is_err() {self.failed=true;}}
    }
    pub fn capture(&mut self,end:u64,time:u64,incarnation:u32,order:&mut u64) {
        if end<=self.seen {return;}
        let offset=self.seen;
        self.seen=end;
        if !self.failed && self.pending {
            if let Some(last)=self.records.last_mut() {
                // Only the latest machine-wide burst can grow. Keep its first
                // byte's time even when later bytes advance time in this slice.
                if last[1]==incarnation as u64 && last[2].checked_add(1)==Some(*order) && last[3]+last[4]==offset {
                    last[4]=end-last[3];return;
                }
            }
        }
        self.close();
        let record=[time,incarnation as u64,*order,offset,end-offset];
        *order=order.saturating_add(1);
        if self.failed || self.records.len()>=self.limit.unwrap_or(MAX_CHUNKS).min(MAX_CHUNKS) {return;}
        self.records.push(record);
        self.pending=true;
    }
}
impl Drop for Index {
    fn drop(&mut self) {self.finish();}
}
