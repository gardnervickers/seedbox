use super::{
    fail,
    linux::{self, Stack},
};
use alloc::vec::Vec;
use core::arch::global_asm;
use seedbox_executor::x86_64::{self, Target, Terminal};
pub type Block = seedbox_executor::Block<Terminal>;
pub use x86_64::translate;
#[repr(C)]
pub struct MachineContext {
    pub regs: [u64; 23],
    fp: usize,
    reserved: [u64; 8],
}
#[repr(C)]
pub struct Context {
    flags: u64,
    link: u64,
    stack: Stack,
    pub machine: MachineContext,
}
// ModRM register order -> Linux UAPI gregset.
const REG: [usize; 16] = [13, 14, 12, 11, 15, 10, 9, 8, 0, 1, 2, 3, 4, 5, 6, 7];
impl Context {
    pub fn pc(&self) -> u64 {
        self.machine.regs[16]
    }
    pub fn set_pc(&mut self, pc: u64) {
        self.machine.regs[16] = pc;
    }
    pub fn trap_pc(&self) -> u64 {
        self.pc() - 1
    }
    pub fn initialize(&mut self, stack: u64, guard: u64) {
        for (n, &r) in REG.iter().enumerate() {
            self.machine.regs[r] = super::pattern(n);
        }
        self.machine.regs[13] = 0;
        self.machine.regs[6] = guard;
        self.machine.regs[15] = stack;
        self.machine.regs[17] = 0x246;
        let fp = self.fp();
        for n in 0..16 {
            fp[160 + n * 16..168 + n * 16].copy_from_slice(&(n as u64 + 1).to_le_bytes());
            fp[168 + n * 16..176 + n * 16].copy_from_slice(&(n as u64 + 101).to_le_bytes());
        }
        fp[0..2].copy_from_slice(&0x27fu16.to_le_bytes());
        fp[24..28].copy_from_slice(&0x3fa0u32.to_le_bytes());
        // Linux may use XSAVE's init-state optimization. Mark the modified
        // legacy components present in the signal frame's XSAVE header.
        if u32::from_le_bytes(fp[464..468].try_into().unwrap()) == 0x46505853 {
            unsafe {
                let bits = (self.machine.fp + 512) as *mut u64;
                bits.write_unaligned(bits.read_unaligned() | 3);
            }
        }
    }
    fn fp(&mut self) -> &mut [u8] {
        if self.machine.fp == 0 {
            fail("missing x86 fp", 0)
        }
        unsafe { core::slice::from_raw_parts_mut(self.machine.fp as *mut u8, 512) }
    }
    pub fn dispatch(&mut self, b: &Block, stack: core::ops::Range<u64>) -> u64 {
        let target = |t: Target| match t {
            Target::Direct(a) => a,
            Target::Register(r) => self.machine.regs[REG[r as usize]],
        };
        match b.terminal {
            Terminal::Jump(t) => target(t),
            Terminal::Conditional {
                target: t,
                condition,
            } => {
                if x86_64::condition(self.machine.regs[17], condition) {
                    t
                } else {
                    b.source.end
                }
            }
            Terminal::Call(t) => {
                let pc = target(t);
                let sp = self.machine.regs[15].checked_sub(8).unwrap();
                if !stack.contains(&sp) || sp + 8 > stack.end {
                    fail("proof call stack boundary", sp)
                }
                unsafe {
                    (sp as *mut u64).write_unaligned(b.source.end);
                }
                self.machine.regs[15] = sp;
                pc
            }
            Terminal::Return => {
                let sp = self.machine.regs[15];
                if !stack.contains(&sp) || sp + 8 > stack.end {
                    fail("proof return stack boundary", sp)
                }
                let pc = unsafe { (sp as *const u64).read_unaligned() };
                self.machine.regs[15] = sp + 8;
                pc
            }
        }
    }
    pub fn verify(&mut self, base: u64, loops: u64, stack: u64, guard: u64) {
        for (n, &r) in REG.iter().enumerate() {
            let expected = match n {
                0 => loops,
                2 => base + 3072,
                4 => stack,
                14 => guard,
                _ => super::pattern(n),
            };
            if self.machine.regs[r] != expected {
                fail("gpr", n as u64)
            }
        }
        // RF can be set by the hardware page fault; all other flags must survive.
        if self.machine.regs[17] & !0x10000 != 0x246 {
            fail("rflags", self.machine.regs[17])
        }
        let fp = self.fp();
        for n in 0..16 {
            for lane in 0..2 {
                let at = 160 + n * 16 + lane * 8;
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
                    fail("xmm register", (n * 2 + lane) as u64)
                }
            }
        }
        if u16::from_le_bytes(fp[0..2].try_into().unwrap()) != 0x27f
            || u32::from_le_bytes(fp[24..28].try_into().unwrap()) != 0x3fa0
        {
            fail("fp controls", 0)
        }
    }
    pub fn store(&self) -> (u64, u32) {
        (self.machine.regs[8], self.machine.regs[9] as u32)
    }
}
pub fn source() -> Vec<u8> {
    let mut code = alloc::vec![0u8;4096];
    code[0] = 0xe8;
    code[1..5].copy_from_slice(&1531i32.to_le_bytes());
    code[5..9].copy_from_slice(&[0x48, 0x8d, 0x40, 1]); // LEA RAX,[RAX+1]
    code[9..263].fill(0x90);
    code[263] = 0xe9;
    code[264..268].copy_from_slice(&(-263i32).to_le_bytes());
    code[1536..1539].copy_from_slice(&[0x48, 0x8d, 0x15]);
    code[1539..1543].copy_from_slice(&1529i32.to_le_bytes()); // LEA RDX,[RIP+...]
    code[1543..1548].copy_from_slice(&[0x66, 0x0f, 0xd4, 0xc1, 0xc3]);
    code
}
pub const LOOP_OFFSET: u64 = 5;
pub const OVERLAP_OFFSET: u64 = 9;
pub const APPEND_WORDS: [u32; 2] = [0x07c7c748, 0xc3000000]; // MOV RDI,7; RET
pub const FAULT_WORD: u32 = 0x901e8b49; // MOV RBX,[R14]; NOP
pub const DEAD_SCRATCH_WORD: u32 = 0x90db314d; // XOR R11,R11; NOP after armed fault
pub fn synchronize(_: usize, _: usize) {}
pub fn tls_set(v: u64) {
    linux::checked(unsafe { linux::syscall(158, 0x1002, v as usize, 0, 0, 0, 0) });
}
pub fn tls_get() -> u64 {
    let mut v = 0;
    linux::checked(unsafe { linux::syscall(158, 0x1003, &mut v as *mut _ as usize, 0, 0, 0, 0) });
    v
}

global_asm!(
    r#"
.global _start
_start:
    mov rdi, rsp
    and rsp, -16
    call proof_main
    ud2
.global proof_boot
proof_boot:
    int3
    ret
.global proof_restorer
proof_restorer:
    mov rax, 15
    syscall
.global proof_store
proof_store:
    mov dword ptr [rdi], esi
.global proof_store_done
proof_store_done:
    ret
.global proof_clone
proof_clone:
    // rdi = flags, rsi = child stack top
    xor rdx, rdx
    xor r10, r10
    xor r8, r8
    mov rax, 56
    syscall
    test rax, rax
    jz 1f
    ret
1:  call proof_writer
    xor rdi, rdi
    mov rax, 60
    syscall
    ud2
"#
);
