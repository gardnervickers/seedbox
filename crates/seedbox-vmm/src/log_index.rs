//! Observation-only serial chunk index. Never participates in machine state or witnesses.
use std::io::Write;

pub const MAGIC: &[u8;8] = b"SBLOG001";
pub const MAX_CHUNKS: usize = 65_536;

#[derive(Default)]
pub struct Index {
    pub records: Vec<[u64;5]>,
    pub file: Option<std::fs::File>,
    pub limit: Option<usize>,
    pub seen: u64,
    failed: bool,
}
impl Index {
    pub fn snapshot(&self) -> Self {
        Self {records:self.records.clone(),file:None,limit:self.limit,seen:self.seen,failed:self.failed}
    }
    /// Sink failures disable indexing only. Raw bytes and execution keep their existing behavior.
    pub fn open(&mut self,path:&str) {
        self.file=None;
        match std::fs::File::create(path).and_then(|mut f|{f.write_all(MAGIC)?;for r in &self.records {for v in r {f.write_all(&v.to_le_bytes())?;}}Ok(f)}) {
            Ok(file)=>self.file=Some(file),
            Err(_)=>self.failed=true,
        }
    }
    pub fn capture(&mut self,end:u64,time:u64,incarnation:u32,order:&mut u64) {
        if end<=self.seen {return;}
        let record=[time,incarnation as u64,*order,self.seen,end-self.seen];
        self.seen=end;
        *order=order.saturating_add(1);
        if self.failed || self.records.len()>=self.limit.unwrap_or(MAX_CHUNKS).min(MAX_CHUNKS) {return;}
        if let Some(file)=self.file.as_mut() {
            let mut bytes=[0u8;40];
            for (i,v) in record.iter().enumerate() {bytes[i*8..i*8+8].copy_from_slice(&v.to_le_bytes());}
            if file.write_all(&bytes).is_err() {self.failed=true;return;}
        }
        self.records.push(record);
    }
}
