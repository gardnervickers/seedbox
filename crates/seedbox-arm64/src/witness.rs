//! Versioned fixed-width little-endian encoding; JSON is never hashed.
use crate::backend::Exit;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
pub const DOMAIN: &[u8] = b"arch=arm64 timebase=pv\0witness=1\0";
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Record {
    pub kind: u8,
    pub time: u64,
    pub pc: u64,
    pub addr: u64,
    pub data: u64,
    pub len: u8,
    pub write: bool,
}
impl Record {
    pub fn new(exit: &Exit, time: u64, pc: u64, response: Option<u64>) -> Self {
        let mut r = Self {
            kind: 0,
            time,
            pc,
            addr: 0,
            data: 0,
            len: 0,
            write: false,
        };
        match *exit {
            Exit::Mmio {
                addr,
                len,
                write,
                data,
                ..
            } => {
                r.kind = 1;
                r.addr = addr;
                r.len = len;
                r.write = write;
                r.data = if write { data } else { response.unwrap_or(0) };
            }
            Exit::Hvc { imm } => {
                r.kind = 2;
                r.data = imm as u64;
            }
            Exit::Wfi => r.kind = 3,
            Exit::Wfe => r.kind = 5,
            Exit::SysReg {
                reg,
                write,
                rt,
                data,
            } => {
                r.kind = 4;
                r.addr = reg as u64;
                r.len = rt;
                r.write = write;
                r.data = if write { data } else { response.unwrap_or(0) };
            }
            Exit::Other(raw) => {
                r.kind = 255;
                r.data = raw;
            }
        }
        r
    }
    pub fn encode(&self) -> Vec<u8> {
        let mut b = vec![self.kind];
        for n in [self.time, self.pc, self.addr, self.data] {
            b.extend(n.to_le_bytes());
        }
        b.extend([self.len, u8::from(self.write)]);
        b
    }
}
#[derive(Debug, Serialize, Deserialize)]
pub struct Report {
    pub backend: String,
    pub arch: String,
    pub timebase: String,
    pub format: u32,
    pub seed: u64,
    pub witness: String,
    pub trace_hash: String,
    pub console_hash: String,
    pub ram_hash: String,
    pub kernel_sha256: String,
    pub initrd_sha256: String,
    pub exits: usize,
    pub virtual_ns: u64,
    pub wall_seconds: f64,
    /// Newly queued masked SPI assertions, including later cancellations.
    /// Diagnostic metadata only; neither counter is an input to finish().
    #[serde(default)]
    pub deferred_spi_events: u64,
    /// Timer SPI deferrals at event+24 clock-read yields with PSTATE.I set.
    #[serde(default)]
    pub deferred_clock_read_spis: u64,
    pub console: String,
    pub trace: Vec<Record>,
}
pub fn hash(b: &[u8]) -> String {
    hex::encode(Sha256::digest(b))
}
pub fn finish(
    trace: &[Record],
    console: &[u8],
    ram: &[u8],
    page: &[u8],
) -> (String, String, String, String) {
    let mut th = Sha256::new();
    th.update(DOMAIN);
    for r in trace {
        th.update(r.encode());
    }
    let trace_digest = th.finalize();
    let console_digest = Sha256::digest(console);
    let mut rh = Sha256::new();
    rh.update((ram.len() as u64).to_le_bytes());
    rh.update(ram);
    rh.update((page.len() as u64).to_le_bytes());
    rh.update(page);
    let ram_digest = rh.finalize();
    let mut all = Sha256::new();
    all.update(DOMAIN);
    all.update((trace.len() as u64).to_le_bytes());
    all.update(trace_digest);
    all.update((console.len() as u64).to_le_bytes());
    all.update(console_digest);
    all.update(ram_digest);
    (
        hex::encode(all.finalize()),
        hex::encode(trace_digest),
        hex::encode(console_digest),
        hex::encode(ram_digest),
    )
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn encoding_is_unambiguous_and_order_sensitive() {
        let a = Record::new(
            &Exit::Mmio {
                addr: 3,
                len: 4,
                write: false,
                data: 0,
                reg: Some(7),
            },
            1,
            2,
            Some(9),
        );
        let mut expected = vec![1];
        for n in [1u64, 2, 3, 9] {
            expected.extend(n.to_le_bytes());
        }
        expected.extend([4, 0]);
        assert_eq!(a.encode(), expected);
        let mut b = a.clone();
        b.pc = 4;
        assert_ne!(
            finish(&[a.clone(), b.clone()], b"hi", b"ram", b"page").0,
            finish(&[b, a], b"hi", b"ram", b"page").0
        );
        assert_ne!(
            finish(&[], b"ab", b"c", b"").0,
            finish(&[], b"a", b"bc", b"").0
        );
    }
    #[test]
    fn sysreg_read_hashes_emulated_response() {
        let exit = Exit::SysReg {
            reg: 0x808c,
            write: false,
            rt: 0,
            data: 0,
        };
        assert_ne!(
            Record::new(&exit, 0, 4, Some(0)).encode(),
            Record::new(&exit, 0, 4, Some(2)).encode()
        );
    }
}
