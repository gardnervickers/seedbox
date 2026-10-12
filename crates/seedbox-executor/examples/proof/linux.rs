//! Raw Linux ABI for the libc-free fixture, not an application syscall layer.
use core::{
    alloc::{GlobalAlloc, Layout},
    arch::asm,
    ffi::c_void,
};

#[cfg(target_arch = "aarch64")]
pub unsafe fn syscall(
    n: usize,
    a: usize,
    b: usize,
    c: usize,
    d: usize,
    e: usize,
    f: usize,
) -> isize {
    let r: isize;
    asm!("svc #0",in("x8")n,inlateout("x0")a=>r,in("x1")b,in("x2")c,in("x3")d,in("x4")e,in("x5")f,options(nostack));
    r
}
#[cfg(target_arch = "x86_64")]
pub unsafe fn syscall(
    n: usize,
    a: usize,
    b: usize,
    c: usize,
    d: usize,
    e: usize,
    f: usize,
) -> isize {
    let r: isize;
    asm!("syscall",inlateout("rax")n=>r,in("rdi")a,in("rsi")b,in("rdx")c,in("r10")d,in("r8")e,in("r9")f,lateout("rcx")_,lateout("r11")_,options(nostack));
    r
}
#[cfg(target_arch = "aarch64")]
pub mod nr {
    pub const WRITE: usize = 64;
    pub const EXIT_GROUP: usize = 94;
    pub const MMAP: usize = 222;
    pub const MUNMAP: usize = 215;
    pub const MPROTECT: usize = 226;
    pub const MEMFD: usize = 279;
    pub const FTRUNCATE: usize = 46;
    pub const CLOSE: usize = 57;
    pub const SIGACTION: usize = 134;
    pub const SIGALTSTACK: usize = 132;
    pub const FUTEX: usize = 98;
    pub const WAIT: usize = 260;
    pub const GETPID: usize = 172;
    pub const REBOOT: usize = 142;
}
#[cfg(target_arch = "x86_64")]
pub mod nr {
    pub const WRITE: usize = 1;
    pub const EXIT_GROUP: usize = 231;
    pub const MMAP: usize = 9;
    pub const MUNMAP: usize = 11;
    pub const MPROTECT: usize = 10;
    pub const MEMFD: usize = 319;
    pub const FTRUNCATE: usize = 77;
    pub const CLOSE: usize = 3;
    pub const SIGACTION: usize = 13;
    pub const SIGALTSTACK: usize = 131;
    pub const FUTEX: usize = 202;
    pub const WAIT: usize = 61;
    pub const GETPID: usize = 39;
    pub const REBOOT: usize = 169;
}
pub fn checked(v: isize) -> usize {
    if v < 0 {
        super::fail("linux syscall", v as u64);
    }
    v as usize
}
pub fn write(s: &str) {
    let mut b = s.as_bytes();
    while !b.is_empty() {
        let n = checked(unsafe { syscall(nr::WRITE, 1, b.as_ptr() as usize, b.len(), 0, 0, 0) });
        if n == 0 {
            exit(2)
        }
        b = &b[n..];
    }
}
pub fn exit(code: usize) -> ! {
    unsafe {
        syscall(nr::EXIT_GROUP, code, 0, 0, 0, 0, 0);
    }
    loop {
        core::hint::spin_loop()
    }
}
pub fn finish() -> ! {
    if unsafe { syscall(nr::GETPID, 0, 0, 0, 0, 0, 0) } == 1 {
        #[cfg(target_arch = "x86_64")]
        unsafe {
            // Existing low-level x86 fixture exit, identical to guest/sbx.c.
            checked(syscall(173, 0xf4, 1, 1, 0, 0, 0));
            asm!("out dx, al", in("dx") 0xf4u16, in("al") 0u8, options(nostack));
        }
        unsafe {
            syscall(nr::REBOOT, 0xfee1dead, 0x28121969, 0x4321fedc, 0, 0, 0);
        }
    }
    exit(0)
}
pub struct Memory {
    pub address: usize,
    pub len: usize,
}
impl Memory {
    pub fn map(len: usize, prot: usize, flags: usize, fd: usize) -> Self {
        let address = checked(unsafe { syscall(nr::MMAP, 0, len, prot, flags, fd, 0) });
        Self { address, len }
    }
    pub fn anonymous(len: usize, prot: usize) -> Self {
        Self::map(len, prot, 0x22, usize::MAX)
    }
    pub fn protect(&self, prot: usize) {
        checked(unsafe { syscall(nr::MPROTECT, self.address, self.len, prot, 0, 0, 0) });
    }
}
impl Drop for Memory {
    fn drop(&mut self) {
        checked(unsafe { syscall(nr::MUNMAP, self.address, self.len, 0, 0, 0, 0) });
    }
}
// Diagnostic allocator: independent mmap per allocation, no runtime/TLS or
// allocator lock can be stranded when task A parks. Not a proposed hot path.
struct Allocator;
#[global_allocator]
static ALLOCATOR: Allocator = Allocator;
unsafe impl GlobalAlloc for Allocator {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        if l.align() > 4096 {
            return core::ptr::null_mut();
        }
        let p = syscall(nr::MMAP, 0, l.size().max(1), 3, 0x22, usize::MAX, 0);
        if p < 0 {
            core::ptr::null_mut()
        } else {
            p as *mut u8
        }
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        syscall(nr::MUNMAP, p as usize, l.size().max(1), 0, 0, 0, 0);
    }
}
#[no_mangle]
unsafe extern "C" fn memcpy(d: *mut c_void, s: *const c_void, n: usize) -> *mut c_void {
    let d = d.cast::<u8>();
    let s = s.cast::<u8>();
    for i in 0..n {
        d.add(i).write_volatile(s.add(i).read_volatile());
    }
    d.cast()
}
#[no_mangle]
unsafe extern "C" fn memmove(d: *mut c_void, s: *const c_void, n: usize) -> *mut c_void {
    let dest = d.cast::<u8>();
    let src = s.cast::<u8>();
    if (d as usize) < s as usize {
        memcpy(d, s, n)
    } else {
        for i in (0..n).rev() {
            dest.add(i).write_volatile(src.add(i).read_volatile());
        }
        d
    }
}
#[no_mangle]
unsafe extern "C" fn memset(d: *mut c_void, c: i32, n: usize) -> *mut c_void {
    for i in 0..n {
        d.cast::<u8>().add(i).write_volatile(c as u8);
    }
    d
}
#[no_mangle]
unsafe extern "C" fn memcmp(a: *const c_void, b: *const c_void, n: usize) -> i32 {
    let a = a.cast::<u8>();
    let b = b.cast::<u8>();
    for i in 0..n {
        let d = a.add(i).read_volatile() as i32 - b.add(i).read_volatile() as i32;
        if d != 0 {
            return d;
        }
    }
    0
}
#[no_mangle]
extern "C" fn rust_eh_personality() -> ! {
    super::fail("unwind in abort-only ELF", 0)
}
// Prebuilt liballoc includes error-formatting cleanup edges even when the
// fixture uses panic=abort. Unwinding is a fatal diagnostic, never a resume.
#[no_mangle]
extern "C" fn _Unwind_Resume(_: *mut c_void) -> ! {
    super::fail("unwind in abort-only ELF", 0)
}

#[repr(C)]
pub struct SignalAction {
    pub handler: usize,
    pub flags: usize,
    pub restorer: usize,
    pub mask: u64,
}
#[repr(C)]
pub struct Stack {
    pub ptr: usize,
    pub flags: i32,
    pub size: usize,
}
pub fn install(handler: usize, restorer: usize) {
    // Block asynchronous delivery in this proof. SIGSEGV remains enabled so B's
    // protected write and A's armed fault are independently recognized. Nested
    // application handlers/edits/restarts are explicitly outside this slice.
    let sa = SignalAction {
        handler,
        flags: 4 | 0x08000000 | 0x04000000,
        restorer,
        mask: !(1u64 << 10),
    };
    for sig in [5, 11] {
        checked(unsafe { syscall(nr::SIGACTION, sig, &sa as *const _ as usize, 0, 8, 0, 0) });
    }
}
pub fn altstack(m: &Memory) {
    let s = Stack {
        ptr: m.address,
        flags: 0,
        size: m.len,
    };
    checked(unsafe { syscall(nr::SIGALTSTACK, &s as *const _ as usize, 0, 0, 0, 0, 0) });
}

// The static compiler-builtins library uses Linux HWCAP constructors on ARM.
// Supply the actual ELF aux vector and run its init array, rather than linking
// libc or lying about hardware support. No application feature filtering.
static mut AUXV: *const usize = core::ptr::null();
#[no_mangle]
static mut __stack_chk_guard: usize = 0;
#[no_mangle]
extern "C" fn __stack_chk_fail() -> ! {
    super::fail("stack protector", 0)
}
#[no_mangle]
unsafe extern "C" fn getauxval(key: usize) -> usize {
    let mut p = AUXV;
    while !p.is_null() && *p != 0 {
        if *p == key {
            return *p.add(1);
        }
        p = p.add(2);
    }
    0
}
pub unsafe fn initialize(initial_stack: *const usize) {
    let mut p = initial_stack.add(*initial_stack + 2);
    while *p != 0 {
        p = p.add(1);
    }
    AUXV = p.add(1);
    let random = getauxval(25) as *const usize;
    if !random.is_null() {
        __stack_chk_guard = random.read_unaligned();
    }
    extern "C" {
        static __init_array_start: usize;
        static __init_array_end: usize;
    }
    let mut init = &__init_array_start as *const usize;
    let end = &__init_array_end as *const usize;
    while init < end {
        let call: extern "C" fn() = core::mem::transmute(*init);
        call();
        init = init.add(1);
    }
}
