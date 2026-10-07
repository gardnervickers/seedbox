//! Diagnostic: yield-free stretches of guest execution. A yield point is one
//! an exit-driven engine could deliver an interrupt at without a counter: a
//! guest I/O, MMIO or HLT exit, or a syscall. Stretches of at least `LOG_MIN`
//! branches are logged with register samples taken at the VMM's own exits
//! (overflows, interrupt windows) inside them. Observation only: nothing here
//! touches guest state.
//!
//! A syscall is seen only as a change of the time page's counter across one
//! KVM_RUN, so it closes the stretch at the start of that run and reopens it
//! at the end. Lengths are therefore lower bounds by up to one run.

use crate::stepper::Paging;
use kvm_bindings::{kvm_regs, kvm_sregs};
use serde_json::{json, Value};

pub const LOG_MIN: u64 = 1 << 21;
const SAMPLES: usize = 32;
const INCIDENTS: usize = 256;
const CODE: usize = 24;

#[derive(Debug, Clone)]
struct Sample {
    branches: u64,
    rip: u64,
    rflags: u64,
    cpl: u16,
    cr3: u64,
    gprs: [u64; 16],
    code: Vec<u8>,
}

#[derive(Default, Debug)]
pub struct SpinDiag {
    start: u64,
    irqs_at_start: u64,
    cr3_start: u64,
    samples: Vec<Sample>,
    /// Stretch count and total branches by floor(log2(length)).
    hist: Vec<(u64, u64)>,
    incidents: Vec<Value>,
    dropped: u64,
}

fn gprs(r: &kvm_regs) -> [u64; 16] {
    [
        r.rax, r.rbx, r.rcx, r.rdx, r.rsi, r.rdi, r.rsp, r.rbp, r.r8, r.r9, r.r10, r.r11, r.r12,
        r.r13, r.r14, r.r15,
    ]
}

impl SpinDiag {
    pub fn sample(&mut self, at: u64, r: &kvm_regs, s: &kvm_sregs, mem: &[u8]) {
        let code = Paging::from(s)
            .translate(mem, r.rip)
            .and_then(|pa| mem.get(pa as usize..pa as usize + CODE))
            .map(|b| b.to_vec())
            .unwrap_or_default();
        if self.samples.len() == SAMPLES {
            self.samples.remove(0);
        }
        self.samples.push(Sample {
            branches: at,
            rip: r.rip,
            rflags: r.rflags,
            cpl: s.cs.selector & 3,
            cr3: s.cr3,
            gprs: gprs(r),
            code,
        });
    }

    /// A yield point: the stretch ends at `close` and the next starts at `open`.
    pub fn yield_point(&mut self, close: u64, open: u64, kind: &str, irqs: u64, s: &kvm_sregs) {
        let len = close.saturating_sub(self.start);
        if len > 0 {
            let b = 63 - len.leading_zeros() as usize;
            if self.hist.len() <= b {
                self.hist.resize(b + 1, (0, 0));
            }
            self.hist[b].0 += 1;
            self.hist[b].1 += len;
        }
        if len >= LOG_MIN {
            if self.incidents.len() < INCIDENTS {
                self.incidents.push(self.incident(len, kind, irqs, s));
            } else {
                self.dropped += 1;
            }
        }
        self.start = open;
        self.irqs_at_start = irqs;
        self.cr3_start = s.cr3;
        self.samples.clear();
    }

    fn incident(&self, len: u64, kind: &str, irqs: u64, s: &kvm_sregs) -> Value {
        let ss = &self.samples;
        // Same RIP, flags and general registers at two samples: a candidate
        // fixed point (memory and vector state are not compared here).
        let repeat = ss.iter().enumerate().any(|(i, a)| {
            ss[i + 1..]
                .iter()
                .any(|b| a.rip == b.rip && a.rflags == b.rflags && a.gprs == b.gprs)
        });
        let span = match (ss.iter().map(|x| x.rip).min(), ss.iter().map(|x| x.rip).max()) {
            (Some(lo), Some(hi)) => Some(hi - lo),
            _ => None,
        };
        json!({
            "start": self.start,
            "length": len,
            "ended_by": kind,
            "irqs_inside": irqs - self.irqs_at_start,
            "cr3_start": format!("{:#x}", self.cr3_start),
            "cr3_end": format!("{:#x}", s.cr3),
            "cpl_end": s.cs.selector & 3,
            "rip_span": span,
            "repeat": repeat,
            "samples": ss.iter().map(|x| json!({
                "at": x.branches,
                "rip": format!("{:#x}", x.rip),
                "cpl": x.cpl,
                "cr3": format!("{:#x}", x.cr3),
                "rflags": format!("{:#x}", x.rflags),
                "gprs": x.gprs.iter().map(|g| format!("{g:#x}")).collect::<Vec<_>>(),
                "code": x.code.iter().map(|b| format!("{b:02x}")).collect::<String>(),
            })).collect::<Vec<_>>(),
        })
    }

    pub fn report(&self) -> Value {
        json!({
            "log_min": LOG_MIN,
            "hist_log2": self.hist.iter().enumerate().filter(|(_, h)| h.0 > 0)
                .map(|(b, h)| json!({"log2": b, "count": h.0, "branches": h.1}))
                .collect::<Vec<_>>(),
            "incidents": self.incidents,
            "incidents_dropped": self.dropped,
        })
    }
}
