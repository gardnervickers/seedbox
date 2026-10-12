use super::{fail, linux::Stack};
use alloc::vec::Vec;
use core::arch::{asm, global_asm};
use seedbox_executor::aarch64::{self, Terminal};
pub type Block = seedbox_executor::Block<Terminal>;
pub use aarch64::translate;

// Linux UAPI layouts, not libc's potentially different typedefs. The kernel
// owns saving/restoring the entire FPSIMD record; Rust only edits test values.
#[repr(C, align(16))]
pub struct MachineContext {
    pub fault: u64,
    pub regs: [u64; 31],
    pub sp: u64,
    pub pc: u64,
    pub pstate: u64,
    pad: u64,
    pub reserved: [u8; 4096],
}
#[repr(C)]
pub struct Context {
    flags: u64,
    link: u64,
    stack: Stack,
    mask: [u64; 16],
    pub machine: MachineContext,
}
impl Context {
    pub fn pc(&self) -> u64 {
        self.machine.pc
    }
    pub fn set_pc(&mut self, pc: u64) {
        self.machine.pc = pc;
    }
    pub fn trap_pc(&self) -> u64 {
        self.pc()
    }
    pub fn initialize(&mut self, stack: u64, guard: u64) {
        for (n, r) in self.machine.regs.iter_mut().enumerate() {
            *r = super::pattern(n);
        }
        self.machine.regs[0] = 0;
        self.machine.regs[20] = guard;
        self.machine.sp = stack;
        self.machine.pstate = (self.machine.pstate & !0xf0000000) | 0xa0000000;
        let fp = self.fp();
        for n in 0..32 {
            fp[16 + n * 16..24 + n * 16].copy_from_slice(&(n as u64 + 1).to_le_bytes());
            fp[24 + n * 16..32 + n * 16].copy_from_slice(&(n as u64 + 101).to_le_bytes());
        }
        fp[8..12].copy_from_slice(&0x10u32.to_le_bytes()); // FPSR IXC
        fp[12..16].copy_from_slice(&0x400000u32.to_le_bytes()); // FPCR rounding
    }
    fn fp(&mut self) -> &mut [u8] {
        let r = &mut self.machine.reserved;
        let mut at = 0;
        while at + 8 <= r.len() {
            let magic = u32::from_le_bytes(r[at..at + 4].try_into().unwrap());
            let size = u32::from_le_bytes(r[at + 4..at + 8].try_into().unwrap()) as usize;
            if magic == 0x46508001 && size == 528 {
                return &mut r[at..at + size];
            }
            if size < 16 || size % 16 != 0 || at + size > r.len() {
                break;
            }
            at += size;
        }
        fail("missing fpsimd", 0)
    }
    pub fn dispatch(&mut self, b: &Block, _stack: core::ops::Range<u64>) -> u64 {
        aarch64::dispatch(
            b.terminal,
            b.source.end,
            &mut self.machine.regs,
            self.machine.pstate,
        )
    }
    pub fn verify(&mut self, base: u64, loops: u64, stack: u64, guard: u64) {
        for n in 0..31 {
            let expected = match n {
                0 => loops,
                2 => base + 3072,
                20 => guard,
                30 => base + 4,
                _ => super::pattern(n),
            };
            if self.machine.regs[n] != expected {
                fail("gpr", n as u64)
            }
        }
        if self.machine.sp != stack {
            fail("sp", self.machine.sp)
        }
        if self.machine.pstate & 0xf0000000 != 0xa0000000 {
            fail("nzcv", self.machine.pstate)
        }
        let fp = self.fp();
        for n in 0..32 {
            for lane in 0..2 {
                let at = 16 + n * 16 + lane * 8;
                let got = u64::from_le_bytes(fp[at..at + 8].try_into().unwrap());
                let expected = if n == 0 {
                    if lane == 0 {
                        3
                    } else {
                        203
                    }
                } else {
                    n as u64 + if lane == 0 { 1 } else { 101 }
                };
                if got != expected {
                    fail("q register", (n * 2 + lane) as u64)
                }
            }
        }
        if u32::from_le_bytes(fp[8..12].try_into().unwrap()) != 0x10
            || u32::from_le_bytes(fp[12..16].try_into().unwrap()) != 0x400000
        {
            fail("fp controls", 0)
        }
    }
    pub fn store(&self) -> (u64, u32) {
        (self.machine.regs[0], self.machine.regs[1] as u32)
    }
}
pub fn source() -> Vec<u8> {
    let mut code = alloc::vec![0u8;4096];
    let mut put = |at: usize, w: u32| code[at..at + 4].copy_from_slice(&w.to_le_bytes());
    put(0, 0x94000180); // BL helper at 1536
    put(4, 0x91000400); // ADD X0, X0, #1
    for n in 1..255 {
        put(4 + n * 4, 0xd503201f);
    }
    put(4 + 255 * 4, 0x14000000 | ((-255i32 as u32) & 0x03ffffff));
    put(1536, 0x10003002); // ADR X2, source+3072
    put(1540, 0x4ee18400); // ADD V0.2D, V0.2D, V1.2D
    put(1544, 0xd65f03c0); // RET
    code
}
pub const LOOP_OFFSET: u64 = 4;
pub const OVERLAP_OFFSET: u64 = 8;
pub const APPEND_WORDS: [u32; 2] = [0xd28000e7, 0xd65f03c0]; // MOV X7,#7; RET
pub const FAULT_WORD: u32 = 0xf9400283; // LDR X3,[X20]
pub const DEAD_SCRATCH_WORD: u32 = 0xaa1f03f0; // MOV X16,XZR after armed fault
pub fn synchronize(address: usize, len: usize) {
    unsafe {
        let ctr: u64;
        asm!("mrs {}, ctr_el0",out(reg)ctr);
        let d = 4usize << ((ctr >> 16) & 15);
        let i = 4usize << (ctr & 15);
        for at in ((address & !(d - 1))..address + len).step_by(d) {
            asm!("dc cvau, {}",in(reg)at);
        }
        asm!("dsb ish");
        for at in ((address & !(i - 1))..address + len).step_by(i) {
            asm!("ic ivau, {}",in(reg)at);
        }
        asm!("dsb ish", "isb");
    }
}
pub fn tls_set(v: u64) {
    unsafe {
        asm!("msr tpidr_el0, {}",in(reg)v);
    }
}
pub fn tls_get() -> u64 {
    let v: u64;
    unsafe {
        asm!("mrs {}, tpidr_el0",out(reg)v);
    }
    v
}

global_asm!(
    r#"
.global _start
_start:
    mov x0, sp
    bl proof_main
    brk #0
.global proof_boot
proof_boot:
    brk #0x5e1
    ret
.global proof_restorer
proof_restorer:
    mov x8, #139
    svc #0
.global proof_store
proof_store:
    str w1, [x0]
.global proof_store_done
proof_store_done:
    ret
.global proof_clone
proof_clone:
    // x0 = flags, x1 = stack top, child always enters proof_writer.
    mov x2, xzr
    mov x3, xzr
    mov x4, xzr
    mov x8, #220
    svc #0
    cbz x0, 1f
    ret
1:  bl proof_writer
    mov x0, #0
    mov x8, #93
    svc #0
    brk #0
"#
);
