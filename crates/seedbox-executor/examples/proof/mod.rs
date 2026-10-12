//! Linux-only executable evidence, not a transparent application launcher.
//!
//! The kernel loads this static ELF and owns task/signal/VM state. Native
//! interiors execute in RX cache mappings; executor traps return full Linux
//! contexts to a stable dispatcher. Every logical precharge happens with the
//! application context already saved and pinned. No application signal delivery
//! is admitted inside helpers: arbitrary/nested signals are NOT implemented.
//! This deliberately adds kernel work at every block and is not the production
//! dispatch design. Neither wall time nor trap counts are a PV proof.
#[cfg(target_arch = "aarch64")]
#[path = "aarch64.rs"]
mod arch;
#[cfg(target_arch = "x86_64")]
#[path = "x86_64.rs"]
mod arch;
mod linux;
use alloc::{
    sync::{Arc, Weak},
    vec,
};
use core::{
    cell::UnsafeCell,
    fmt::{self, Write},
    sync::atomic::{AtomicU32, Ordering},
};
use linux::{checked, nr, syscall, Memory};
use seedbox_executor::{
    meter::{Meter, DEFAULT_BUDGET, PROGRESS_MARK},
    source::{Backing, Pin, SourceIndex, Span},
};

extern "C" {
    fn proof_boot();
    fn proof_restorer();
    fn proof_store(address: usize, value: u32);
    fn proof_store_done();
    fn proof_clone(flags: usize, stack: usize) -> isize;
}
struct Output;
impl Write for Output {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        linux::write(s);
        Ok(())
    }
}
fn log(args: fmt::Arguments<'_>) {
    let _ = Output.write_fmt(args);
}
pub fn fail(message: &str, value: u64) -> ! {
    log(format_args!("EXECUTOR_FAIL {message} {value:#x}\n"));
    linux::exit(2)
}
#[panic_handler]
fn panic(info: &core::panic::PanicInfo<'_>) -> ! {
    log(format_args!("EXECUTOR_PANIC {info}\n"));
    linux::exit(2)
}
pub fn pattern(n: usize) -> u64 {
    0x1122334400000000 | ((n as u64 + 1) * 0x010101)
}

// The fixture's two tasks hand sole index ownership across a futex barrier.
// No &mut index or source slice crosses park(), no signal handler reenters it.
struct Cell<T>(UnsafeCell<T>);
unsafe impl<T> Sync for Cell<T> {}
static INDEX: Cell<Option<SourceIndex<Code>>> = Cell(UnsafeCell::new(None));
static CONFIG: Cell<Option<Config>> = Cell(UnsafeCell::new(None));
static TASK: Cell<Option<Task>> = Cell(UnsafeCell::new(None));
static BARRIER: AtomicU32 = AtomicU32::new(0);
struct Config {
    source: Memory,
    alias: Memory,
    guard: Memory,
    app_stack: Memory,
    tls: Memory,
    child: usize,
}
struct Code {
    block: arch::Block,
    _memory: Memory,
}
struct Task {
    current: Option<Pin<Code>>,
    meter: Meter,
    loops: u64,
    retired: Option<Weak<seedbox_executor::source::Generation<Code>>>,
    started: bool,
}
fn config() -> &'static Config {
    unsafe { (&*CONFIG.0.get()).as_ref().unwrap() }
}
// Callers enforce guest barrier ownership; never retain this reference at a syscall that parks A.
unsafe fn index() -> &'static mut SourceIndex<Code> {
    (&mut *INDEX.0.get()).as_mut().unwrap()
}
fn span(start: u64, end: u64) -> Span {
    Span {
        backing: Backing(1),
        bytes: start..end,
    }
}
fn lookup(pc: u64) -> Pin<Code> {
    unsafe {
        if let Some(p) = index().lookup(pc) {
            return p;
        }
        let c = config();
        let base = c.source.address as u64;
        let offset = pc
            .checked_sub(base)
            .filter(|n| *n < 4096)
            .unwrap_or_else(|| fail("target outside proof source", pc));
        let memory = Memory::anonymous(4096, 3);
        let source = core::slice::from_raw_parts(
            (c.source.address + offset as usize) as *const u8,
            4096 - offset as usize,
        );
        let block = arch::translate(source, pc, memory.address as u64)
            .unwrap_or_else(|_| fail("unsupported translation", pc));
        if block.bytes.len() > memory.len {
            fail("cache size", block.bytes.len() as u64)
        }
        core::ptr::copy_nonoverlapping(
            block.bytes.as_ptr(),
            memory.address as *mut u8,
            block.bytes.len(),
        );
        arch::synchronize(memory.address, block.bytes.len());
        memory.protect(5);
        let end = block.source.end - base;
        index()
            .install(
                pc,
                vec![span(offset, end)],
                Code {
                    block,
                    _memory: memory,
                },
            )
            .unwrap()
    }
}
fn wake() {
    checked(unsafe { syscall(nr::FUTEX, &BARRIER as *const _ as usize, 1, 1, 0, 0, 0) });
}
fn wait(value: u32) {
    while BARRIER.load(Ordering::Acquire) == value {
        let r = unsafe {
            syscall(
                nr::FUTEX,
                &BARRIER as *const _ as usize,
                0,
                value as usize,
                0,
                0,
                0,
            )
        };
        if r < 0 && r != -11 && r != -4 {
            fail("futex", r as u64)
        }
    }
}

#[no_mangle]
extern "C" fn proof_writer() {
    wait(0);
    let base = config().source.address;
    // These really fault against the R-X application alias. The SIGSEGV assist
    // below writes the existing memfd backing, never replaces anonymous/COW VMAs.
    unsafe {
        proof_store(base + 2048, arch::APPEND_WORDS[0]);
        proof_store(base + 2052, arch::APPEND_WORDS[1]);
        if index().lookup(base as u64 + arch::LOOP_OFFSET).is_none() {
            fail("append invalidated neighbor", 0)
        }
        let b = lookup(base as u64 + 2048);
        if b.value.block.instructions != 2 {
            fail("appended decode", 0)
        }
        proof_store(
            base + arch::OVERLAP_OFFSET as usize,
            arch::DEAD_SCRATCH_WORD,
        );
        proof_store(base + arch::LOOP_OFFSET as usize, arch::FAULT_WORD);
        if index().lookup(base as u64 + arch::LOOP_OFFSET).is_some()
            || index().lookup(base as u64 + arch::OVERLAP_OFFSET).is_some()
        {
            fail("stale lookup", 0)
        }
        if !index().is_live(&b) {
            fail("overwrite invalidated neighbor B", 0)
        }
    }
    linux::write("EXECUTOR_WRITER append-retained overwrite-retired-all no-reader-wait\n");
    BARRIER.store(2, Ordering::Release);
    wake();
}

extern "C" fn signal(signal: i32, info: *const u8, context: *mut arch::Context) {
    unsafe {
        let ctx = &mut *context;
        if signal == 11 && ctx.pc() == proof_store as *const () as u64 {
            if BARRIER.load(Ordering::Acquire) != 1 {
                fail("write outside ownership gate", 0)
            }
            let (address, value) = ctx.store();
            let c = config();
            let offset = address
                .checked_sub(c.source.address as u64)
                .filter(|n| n.checked_add(4).is_some_and(|end| end <= 4096))
                .unwrap_or_else(|| fail("write assist range", address));
            // Only four precise native fixture stores are admitted to this helper.
            let expected = if offset == 2048 {
                arch::APPEND_WORDS[0]
            } else if offset == 2052 {
                arch::APPEND_WORDS[1]
            } else if offset == arch::LOOP_OFFSET {
                arch::FAULT_WORD
            } else if offset == arch::OVERLAP_OFFSET {
                arch::DEAD_SCRATCH_WORD
            } else {
                fail("unknown write assist", offset)
            };
            let fault = (info.add(16) as *const u64).read_unaligned();
            if value != expected || fault != address {
                fail("write assist context", fault)
            }
            let removed = index().invalidate(span(offset, offset + 4)).unwrap();
            if offset == arch::OVERLAP_OFFSET && removed.len() != 2 {
                fail("dependent spans", removed.len() as u64)
            }
            if offset >= 2048 && !removed.is_empty() {
                fail("append overlap", 0)
            }
            ((c.alias.address + offset as usize) as *mut u32).write_unaligned(value);
            arch::synchronize(c.alias.address + offset as usize, 4);
            ctx.set_pc(proof_store_done as *const () as u64);
            return;
        }
        let task = (&mut *TASK.0.get()).as_mut().unwrap();
        let c = config();
        if signal == 11 {
            let pin = task.current.as_ref().unwrap();
            let original = pin
                .value
                .block
                .source_pc(ctx.pc())
                .unwrap_or_else(|| fail("unmapped fault", ctx.pc()));
            let fault = (info.add(16) as *const u64).read_unaligned();
            if original != c.source.address as u64 + arch::LOOP_OFFSET
                || fault != c.guard.address as u64
            {
                fail("fault identity", fault)
            }
            ctx.set_pc(original); // Construct the logical application context, not a cache PC.
            ctx.verify(
                c.source.address as u64,
                task.loops,
                (c.app_stack.address + c.app_stack.len) as u64,
                c.guard.address as u64,
            );
            if arch::tls_get() != c.tls.address as u64 {
                fail("application tls", arch::tls_get())
            }
            if task.meter.phase()
                != (seedbox_executor::meter::Phase::Running {
                    instructions: pin.value.block.instructions,
                })
            {
                fail("fault charge phase", 0)
            }
            if task.meter.payments() != 3 || task.loops != 585 {
                fail("charge or execution count", task.loops)
            }
            if task.meter.left()
                != (50000 + 3 * 50000 - 4 - 585 * 256 - i64::from(pin.value.block.instructions))
            {
                fail("missing or duplicate precharge", task.meter.left() as u64)
            }
            if task.retired.as_ref().unwrap().upgrade().is_some() {
                fail("retired generation leaked", 0)
            }
            log(format_args!("EXECUTOR_FAULT pc={original:#x} address={fault:#x} loops={} payments={} left={} gpr-flags-fp-tls-stack=exact old-pin=reclaimed\n",task.loops,task.meter.payments(),task.meter.left()));
            linux::write("SEEDBOX_INIT_PASS\n");
            linux::finish()
        }
        if signal != 5 {
            fail("unexpected signal", signal as u64)
        }
        let next = if !task.started {
            if ctx.trap_pc() != proof_boot as *const () as u64 {
                fail("bootstrap trap", ctx.pc())
            }
            task.started = true;
            ctx.initialize(
                (c.app_stack.address + c.app_stack.len) as u64,
                c.guard.address as u64,
            );
            c.source.address as u64
        } else {
            let pin = task.current.as_ref().unwrap();
            if ctx.trap_pc() != pin.value.block.trap {
                fail("not executor trap", ctx.pc())
            }
            task.meter.complete_block().unwrap();
            if pin.entry == c.source.address as u64 + arch::LOOP_OFFSET {
                task.loops += 1;
            }
            ctx.dispatch(
                &pin.value.block,
                c.app_stack.address as u64..(c.app_stack.address + c.app_stack.len) as u64,
            )
        };
        loop {
            let pin = lookup(next);
            if task.meter.precharge(pin.value.block.instructions).unwrap() {
                ctx.verify(
                    c.source.address as u64,
                    task.loops,
                    (c.app_stack.address + c.app_stack.len) as u64,
                    c.guard.address as u64,
                );
                // All app state lives in ctx. The marked syscall changes only the
                // helper's registers and returns into this permanent gateway.
                loop {
                    linux::write("EXECUTOR_PV_ENTER\n");
                    checked(syscall(nr::GETPID, PROGRESS_MARK as usize, 0, 0, 0, 0, 0));
                    linux::write("EXECUTOR_PV_RETURN\n");
                    if !task.meter.paid_one().unwrap() {
                        break;
                    }
                }
                if task.meter.payments() == 3 {
                    let old_map = pin.value.block.source_pc(pin.value.block.trap).unwrap();
                    task.retired = Some(Arc::downgrade(&pin));
                    BARRIER.store(1, Ordering::Release);
                    wake();
                    wait(1);
                    let mut status = 0i32;
                    checked(syscall(
                        nr::WAIT,
                        c.child,
                        &mut status as *mut _ as usize,
                        0,
                        0,
                        0,
                        0,
                    ));
                    if status != 0 {
                        fail("writer exit", status as u64)
                    }
                    if index().is_live(&pin)
                        || pin.value.block.source_pc(pin.value.block.trap) != Some(old_map)
                    {
                        fail("parked immutable map", 0)
                    }
                    linux::write(
                        "EXECUTOR_PIN invalidated-map-readable writer-finished-before-release\n",
                    );
                }
                ctx.verify(
                    c.source.address as u64,
                    task.loops,
                    (c.app_stack.address + c.app_stack.len) as u64,
                    c.guard.address as u64,
                );
                task.meter.refund_for_dispatch().unwrap();
                ctx.verify(
                    c.source.address as u64,
                    task.loops,
                    (c.app_stack.address + c.app_stack.len) as u64,
                    c.guard.address as u64,
                );
                // Source may have changed while parked. Refund the OLD block's
                // charge, release its pin, then lookup/precharge the NEW generation.
                continue;
            }
            if !index().is_live(&pin) {
                fail("invalid entry", pin.id)
            }
            task.meter.enter().unwrap();
            ctx.set_pc(pin.value.block.maps[0].cache.start);
            task.current = Some(pin);
            return;
        }
    }
}

#[no_mangle]
extern "C" fn proof_main(initial_stack: *const usize) -> ! {
    unsafe {
        linux::initialize(initial_stack);
        linux::write("EXECUTOR_PROOF signal-gated libc-free experimental\n");
        let fd = checked(syscall(
            nr::MEMFD,
            c"seedbox-executor-proof".as_ptr() as usize,
            0,
            0,
            0,
            0,
            0,
        ));
        checked(syscall(nr::FTRUNCATE, fd, 4096, 0, 0, 0, 0));
        let source = Memory::map(4096, 5, 1, fd);
        let alias = Memory::map(4096, 3, 1, fd);
        checked(syscall(nr::CLOSE, fd, 0, 0, 0, 0, 0));
        let bytes = arch::source();
        core::ptr::copy_nonoverlapping(bytes.as_ptr(), alias.address as *mut u8, bytes.len());
        arch::synchronize(alias.address, 4096);
        let guard = Memory::anonymous(4096, 0);
        let app_stack = Memory::anonymous(65536, 3);
        let tls = Memory::anonymous(4096, 3);
        arch::tls_set(tls.address as u64);
        let signal_stack = Memory::anonymous(131072, 3);
        linux::altstack(&signal_stack);
        let writer_stack = Memory::anonymous(131072, 3);
        linux::install(
            signal as *const () as usize,
            proof_restorer as *const () as usize,
        );
        *CONFIG.0.get() = Some(Config {
            source,
            alias,
            guard,
            app_stack,
            tls,
            child: 0,
        });
        *INDEX.0.get() = Some(SourceIndex::default());
        *TASK.0.get() = Some(Task {
            current: None,
            meter: Meter::new(DEFAULT_BUDGET).unwrap(),
            loops: 0,
            retired: None,
            started: false,
        });
        lookup(config().source.address as u64 + arch::LOOP_OFFSET);
        lookup(config().source.address as u64 + arch::OVERLAP_OFFSET);
        linux::write("EXECUTOR_UNMARKED_ENTER\n");
        checked(syscall(nr::GETPID, 0, 0, 0, 0, 0, 0));
        linux::write("EXECUTOR_UNMARKED_RETURN\n");
        let child = checked(proof_clone(
            0x100 | 17,
            writer_stack.address + writer_stack.len,
        ));
        (&mut *CONFIG.0.get()).as_mut().unwrap().child = child;
        proof_boot();
        fail("returned from boot", 0)
    }
}
