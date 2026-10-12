//! Single execution task, native linked blocks, rare signal-owned due/miss
//! gateway. The second Linux task can mutate only while A is parked. All signal
//! helpers are libc/TLS independent. Arbitrary application handlers are excluded.
// Reuse the diagnostic Linux context/entry ABI; its trap dispatcher and source
// constants are intentionally unused by this separate linked executable.
#[allow(dead_code, unused_imports)]
#[path = "../proof/aarch64.rs"]
mod arch;
mod bench;
#[path = "../proof/linux.rs"]
mod linux;
use alloc::{
    sync::{Arc, Weak},
    vec,
    vec::Vec,
};
use core::{
    cell::UnsafeCell,
    fmt::{self, Write},
    sync::atomic::{AtomicU32, Ordering},
};
use linux::{checked, nr, syscall, Memory};
use seedbox_executor::{
    aarch64::{
        self,
        linked::{self, LinkedBlock, State, Target},
    },
    meter::{Meter, Phase, DEFAULT_BUDGET, PROGRESS_MARK},
    source::{Backing, Generation, Pin, SourceIndex, Span},
};
extern "C" {
    fn proof_boot();
    fn proof_restorer();
    fn proof_clone(flags: usize, stack: usize) -> isize;
    fn proof_store(address: usize, value: u32);
    fn proof_store_done();
}
struct Cell<T>(UnsafeCell<T>);
unsafe impl<T> Sync for Cell<T> {}
static ENGINE: Cell<Option<Engine>> = Cell(UnsafeCell::new(None));
static CONFIG: Cell<Option<Config>> = Cell(UnsafeCell::new(None));
static STATE: Cell<State> = Cell(UnsafeCell::new(State {
    x16: 0,
    x17: 0,
    tls: 0,
    left: 50000,
    gateway_pc: 0,
    gateway_charge: 0,
    target: 0,
}));
static BARRIER: AtomicU32 = AtomicU32::new(0);
static PAYMENTS: Cell<u64> = Cell(UnsafeCell::new(0));
static RETIRED: Cell<Option<Weak<Generation<Code>>>> = Cell(UnsafeCell::new(None));
struct Config {
    source: Memory,
    alias: Memory,
    guard: Memory,
    stack: Memory,
    tls: Memory,
    child: usize,
    loop_budget: i64,
    benchmark: Option<bench::Bench>,
    gateway_fault: bool,
    target_register: Option<(usize, u64)>,
}
struct Code {
    block: LinkedBlock,
    memory: Memory,
}
struct Engine {
    index: SourceIndex<Code>,
    targets: Vec<Target>,
    generations: Vec<u64>,
    codes: Vec<Weak<Generation<Code>>>,
    arena: Memory,
    next: usize,
    hooks: Vec<(u64, u32)>,
    hook_count: usize,
    translations: usize,
    emitted: usize,
    revoked: usize,
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
    if let Some((register, target)) =
        unsafe { (&*CONFIG.0.get()).as_ref().and_then(|c| c.target_register) }
    {
        if n == register {
            return target;
        }
    }
    0x1122334400000000 | ((n as u64 + 1) * 0x010101)
}
fn config() -> &'static Config {
    unsafe { (&*CONFIG.0.get()).as_ref().unwrap() }
}
// No reference to this engine crosses task handoff or a potentially blocking
// gateway syscall. Signal helpers cannot reenter index mutation.
unsafe fn engine() -> &'static mut Engine {
    (&mut *ENGINE.0.get()).as_mut().unwrap()
}
fn span(start: u64, end: u64) -> Span {
    Span {
        backing: Backing(1),
        bytes: start..end,
    }
}
impl Engine {
    fn gate(&self, source: u64) -> u64 {
        self.targets
            .iter()
            .find(|t| t.source == source)
            .unwrap_or_else(|| fail("unknown target", source))
            .gate
    }
    fn patch_gate(&self, at: u64, word: u32) {
        let page = Memory {
            address: self.arena.address,
            len: 4096,
        };
        page.protect(3);
        unsafe {
            (at as *mut u32).write(word);
        }
        arch::synchronize(at as usize, 4);
        page.protect(5);
        core::mem::forget(page);
    }
    fn lookup(&mut self, pc: u64) -> Pin<Code> {
        if let Some(p) = self.index.lookup(pc) {
            return p;
        }
        let c = config();
        let base = c.source.address as u64;
        let offset = pc
            .checked_sub(base)
            .filter(|n| *n < 4096)
            .unwrap_or_else(|| fail("source range", pc));
        if self.next + 4096 > self.arena.len {
            fail("cache capacity", self.next as u64);
        }
        let memory = Memory {
            address: self.arena.address + self.next,
            len: 4096,
        };
        self.next += 4096;
        let source =
            unsafe { core::slice::from_raw_parts(pc as *const u8, 4096 - offset as usize) };
        let block = linked::emit(
            source,
            pc,
            memory.address as u64,
            if c.gateway_fault {
                c.guard.address as u64
            } else {
                STATE.0.get() as u64
            },
            self.arena.address as u64,
            &self.targets,
        )
        .unwrap_or_else(|e| {
            log(format_args!("emit: {e:?}\n"));
            fail("translation", pc)
        });
        if block.bytes.len() > 4096 {
            fail("cache block size", block.bytes.len() as u64);
        }
        memory.protect(3);
        unsafe {
            core::ptr::copy_nonoverlapping(
                block.bytes.as_ptr(),
                memory.address as *mut u8,
                block.bytes.len(),
            );
        }
        // Test hooks only on the old loop, not production emission. Every
        // instrumentation PC starts trapped and is restored on first visit.
        if c.benchmark.is_none() && matches!(offset, 0 | 4 | 1536) && self.translations < 4 {
            for m in &block.maps {
                if m.instrumentation {
                    let word = unsafe { (m.pc as *const u32).read() };
                    self.hooks.push((m.pc, word));
                    unsafe {
                        (m.pc as *mut u32).write(aarch64::TRAP);
                    }
                }
            }
        }
        arch::synchronize(memory.address, block.bytes.len());
        memory.protect(5);
        self.translations += 1;
        self.emitted += block.bytes.len();
        if c.benchmark.is_none() && c.loop_budget == 49996 && !c.gateway_fault {
            let digits = b"0123456789abcdef";
            let hex: Vec<u8> = block
                .bytes
                .iter()
                .flat_map(|b| [digits[(b >> 4) as usize], digits[(b & 15) as usize]])
                .collect();
            log(format_args!(
                "EXECUTOR_CODE source={offset} cache={:#x} instructions={} bytes={}\n",
                memory.address,
                block.instructions,
                core::str::from_utf8(&hex).unwrap()
            ));
        }
        let end = block.source.end - base;
        let p = self
            .index
            .install(pc, vec![span(offset, end)], Code { block, memory })
            .unwrap();
        self.codes.push(Arc::downgrade(&p));
        let slot = self.targets.iter().position(|t| t.source == pc).unwrap();
        self.patch_gate(
            self.targets[slot].gate,
            linked::branch(self.targets[slot].gate, p.value.memory.address as u64).unwrap(),
        );
        self.generations[slot] = p.id;
        p
    }
    fn invalidate(&mut self, offset: u64) -> usize {
        let removed = self.index.invalidate(span(offset, offset + 4)).unwrap();
        // There is no executable task while this transaction runs. Revoke all
        // shared incoming gates BEFORE the caller commits any source store.
        for slot in 0..self.generations.len() {
            if removed.contains(&self.generations[slot]) {
                self.patch_gate(self.targets[slot].gate, aarch64::TRAP);
                self.generations[slot] = 0;
                self.revoked += 1;
            }
        }
        removed.len()
    }
    fn at(&self, pc: u64) -> Option<(Pin<Code>, linked::Map)> {
        self.codes
            .iter()
            .filter_map(Weak::upgrade)
            .find_map(|p| p.value.block.map(pc).map(|m| (p, m)))
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
    unsafe {
        proof_store(base + 2048, arch::APPEND_WORDS[0]);
        proof_store(base + 2052, arch::APPEND_WORDS[1]);
        if engine().index.lookup(base as u64 + 4).is_none() {
            fail("append invalidated neighbor", 0)
        }
        let appended = engine().lookup(base as u64 + 2048);
        proof_store(base + 8, arch::DEAD_SCRATCH_WORD);
        proof_store(base + 12, 0x17fffffe); // B source+4: replacement is THREE instructions.
        proof_store(base + 4, arch::FAULT_WORD);
        if !engine().index.is_live(&appended) {
            fail("append lost", 0)
        }
        for entry in [base as u64 + 4, base as u64 + 8] {
            let gate = engine().gate(entry);
            if (gate as *const u32).read() != aarch64::TRAP {
                fail("incoming gate not revoked", gate)
            }
        }
    }
    linux::write("EXECUTOR_WRITER append-retained overwrite-retired-all incoming-direct-and-indirect-revoked no-reader-wait\n");
    BARRIER.store(2, Ordering::Release);
    wake();
}
fn verify(ctx: &mut arch::Context, loops: u64) {
    let c = config();
    ctx.verify(
        c.source.address as u64,
        loops,
        (c.stack.address + c.stack.len) as u64,
        c.guard.address as u64,
    );
}
fn verify_at(ctx: &mut arch::Context, source: u64) {
    let c = config();
    let base = c.source.address as u64;
    let before_helper = source == base || source == base + 1536;
    for n in 0..31 {
        let expected = match n {
            0 => {
                if before_helper {
                    0
                } else {
                    ctx.machine.regs[0]
                }
            }
            2 => {
                if before_helper {
                    pattern(2)
                } else {
                    base + 3072
                }
            }
            20 => c.guard.address as u64,
            30 => {
                if source == base {
                    pattern(30)
                } else {
                    base + 4
                }
            }
            _ => pattern(n),
        };
        if ctx.machine.regs[n] != expected {
            fail("boundary gpr", n as u64)
        }
    }
    if ctx.machine.sp != (c.stack.address + c.stack.len) as u64
        || ctx.machine.pstate & 0xf0000000 != 0xa0000000
    {
        fail("boundary sp/nzcv", source)
    }
    let fp = &ctx.machine.reserved;
    // Linux puts its fixed FPSIMD record first on the fixture's exposed ISA.
    if u32::from_le_bytes(fp[..4].try_into().unwrap()) != 0x46508001
        || u32::from_le_bytes(fp[4..8].try_into().unwrap()) != 528
    {
        fail("boundary fpsimd record", 0)
    }
    for n in 0..32 {
        for lane in 0..2 {
            let at = 16 + n * 16 + lane * 8;
            let expected = if n == 0 && !before_helper {
                if lane == 0 {
                    3
                } else {
                    203
                }
            } else {
                n as u64 + if lane == 0 { 1 } else { 101 }
            };
            if u64::from_le_bytes(fp[at..at + 8].try_into().unwrap()) != expected {
                fail("boundary simd", (n * 2 + lane) as u64)
            }
        }
    }
    if u32::from_le_bytes(fp[8..12].try_into().unwrap()) != 0x10
        || u32::from_le_bytes(fp[12..16].try_into().unwrap()) != 0x400000
    {
        fail("boundary fp controls", 0)
    }
}
extern "C" fn signal(sig: i32, info: *const u8, ctx: *mut arch::Context) {
    unsafe {
        let ctx = &mut *ctx;
        let c = config();
        let base = c.source.address as u64;
        if let Some(b) = c.benchmark {
            bench::signal(sig, info, ctx, b);
            return;
        }
        if sig == 11 && ctx.pc() == proof_store as *const () as u64 {
            if BARRIER.load(Ordering::Acquire) != 1 {
                fail("writer ownership", 0)
            }
            let (address, value) = ctx.store();
            let offset = address - base;
            let expected = match offset {
                2048 => arch::APPEND_WORDS[0],
                2052 => arch::APPEND_WORDS[1],
                8 => arch::DEAD_SCRATCH_WORD,
                12 => 0x17fffffe,
                4 => arch::FAULT_WORD,
                _ => fail("writer range", offset),
            };
            if value != expected || (info.add(16) as *const u64).read_unaligned() != address {
                fail("writer fault", address)
            }
            let removed = engine().invalidate(offset);
            if (offset == 8 && removed != 2) || (offset >= 2048 && removed != 0) {
                fail("invalidation count", removed as u64)
            }
            ((c.alias.address + offset as usize) as *mut u32).write(value);
            arch::synchronize(c.alias.address + offset as usize, 4);
            arch::synchronize(c.source.address + offset as usize, 4);
            ctx.set_pc(proof_store_done as *const () as u64);
            return;
        }
        if sig == 5 && ctx.pc() == proof_boot as *const () as u64 {
            ctx.initialize(
                (c.stack.address + c.stack.len) as u64,
                c.guard.address as u64,
            );
            ctx.set_pc(engine().gate(base));
            return;
        }
        if let Some((pin, map)) = engine().at(ctx.pc()) {
            if !engine().index.is_live(&pin) {
                fail("stale cache entered", pin.id)
            }
            if sig == 5 {
                let slot = engine()
                    .hooks
                    .iter()
                    .position(|(pc, _)| *pc == ctx.pc())
                    .unwrap_or_else(|| fail("unknown cache trap", ctx.pc()));
                let (_, word) = engine().hooks.remove(slot);
                let hardware = ctx.machine.regs;
                if matches!(map.saved, linked::Saved::Native | linked::Saved::X16InState)
                    && arch::tls_get() != c.tls.address as u64
                {
                    fail("boundary hardware tls not restored", map.pc);
                }
                let tls = map.reconstruct(&mut ctx.machine.regs, arch::tls_get(), &*STATE.0.get());
                if tls != c.tls.address as u64 {
                    fail("hook tls", tls)
                }
                verify_at(ctx, map.source);
                let offset = (map.pc - pin.value.memory.address as u64) / 4;
                if pin.entry == base + 4 {
                    if offset == 0 {
                        (*STATE.0.get()).left = c.loop_budget;
                    }
                    let first_due = c.loop_budget - (c.loop_budget.max(0) / 256 + 1) * 256;
                    let first_payments = if c.loop_budget >= 256 {
                        0
                    } else {
                        (256 - c.loop_budget + 49999) / 50000
                    };
                    let first_running = c.loop_budget + first_payments * 50000 - 256;
                    let expected = if offset < 10 {
                        c.loop_budget
                    } else if offset == 10 {
                        c.loop_budget - 256
                    } else if offset >= 269 {
                        first_due
                    } else {
                        first_running
                    };
                    if (*STATE.0.get()).left != expected {
                        fail("hook charge commit", offset)
                    }
                }
                log(format_args!("EXECUTOR_BOUNDARY source={:#x} offset={} saved={:?} phase={:?} left={} exact\n",map.source-base,offset,map.saved,map.phase,(*STATE.0.get()).left));
                ctx.machine.regs = hardware;
                pin.value.memory.protect(3);
                (map.pc as *mut u32).write(word);
                arch::synchronize(map.pc as usize, 4);
                pin.value.memory.protect(5);
                engine().hook_count += 1;
                return;
            }
            if sig != 11 || map.instrumentation {
                fail("executor interior fault", ctx.pc())
            }
            let fault = (info.add(16) as *const u64).read_unaligned();
            if map.source != base + 4
                || fault != c.guard.address as u64
                || map.phase != (Phase::Running { instructions: 3 })
            {
                fail("application fault identity", fault)
            }
            let loops = ((c.loop_budget + 100000).max(0) / 256) as u64;
            let remaining = c.loop_budget + 150000 - loops as i64 * 256 - 3;
            map.reconstruct(&mut ctx.machine.regs, arch::tls_get(), &*STATE.0.get());
            ctx.set_pc(map.source);
            verify(ctx, loops);
            if arch::tls_get() != c.tls.address as u64
                || (*STATE.0.get()).left != remaining
                || *PAYMENTS.0.get() != 3
            {
                fail("fault ledger or tls", (*STATE.0.get()).left as u64)
            }
            if (&*RETIRED.0.get()).as_ref().unwrap().upgrade().is_some() {
                fail("old code not reclaimed", 0)
            }
            // Default run visits every loop instrumentation PC; unselected
            // indirect-cache arms and never-due helper stubs are not reachable.
            if c.loop_budget == 49996
                && engine()
                    .hooks
                    .iter()
                    .any(|(pc, _)| engine().at(*pc).is_some_and(|(p, _)| p.entry == base + 4))
            {
                fail("unvisited loop hooks", 0)
            }
            log(format_args!("EXECUTOR_FAULT pc={:#x} address={:#x} loops={} payments=3 left={} gpr-flags-fp-tls-stack=exact old-pin=reclaimed hooks={} translations={} emitted={} revoked={}\n",map.source,fault,loops,remaining,engine().hook_count,engine().translations,engine().emitted,engine().revoked));
            linux::write("SEEDBOX_INIT_PASS\n");
            linux::finish();
        }
        if sig != 5 {
            fail("unmapped helper fault", ctx.pc())
        }
        if ctx.pc() == engine().arena.address as u64 {
            // Stable gateway: restore original application scratch and TLS
            // before any external effect. No cache return continuation is used.
            ctx.machine.regs[16] = (*STATE.0.get()).x16;
            ctx.machine.regs[17] = (*STATE.0.get()).x17;
            arch::tls_set((*STATE.0.get()).tls);
            let pc = (*STATE.0.get()).gateway_pc;
            let charge = (*STATE.0.get()).gateway_charge as u32;
            if charge == 0 {
                fail("unpopulated indirect target", (*STATE.0.get()).target)
            }
            let pin = engine().index.lookup(pc).unwrap();
            verify_at(ctx, pc);
            let mut meter = Meter::from_inline_debit(
                DEFAULT_BUDGET,
                (*STATE.0.get()).left,
                charge,
                *PAYMENTS.0.get(),
            )
            .unwrap();
            loop {
                linux::write("EXECUTOR_PV_ENTER\n");
                checked(syscall(nr::GETPID, PROGRESS_MARK as usize, 0, 0, 0, 0, 0));
                linux::write("EXECUTOR_PV_RETURN\n");
                if !meter.paid_one().unwrap() {
                    break;
                }
            }
            *PAYMENTS.0.get() = meter.payments();
            if meter.payments() == 3 {
                if c.loop_budget == 49996
                    && engine()
                        .hooks
                        .iter()
                        .any(|(pc, _)| pin.value.block.map(*pc).is_some())
                {
                    fail("unvisited live loop instrumentation", 0);
                }
                *RETIRED.0.get() = Some(Arc::downgrade(&pin));
                let old_map = pin.value.block.maps.clone();
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
                if status != 0 || engine().index.is_live(&pin) || pin.value.block.maps != old_map {
                    fail("parked generation", status as u64)
                }
                linux::write(
                    "EXECUTOR_PIN invalidated-map-readable writer-finished-before-release\n",
                );
            }
            meter.refund_for_dispatch().unwrap();
            (*STATE.0.get()).left = meter.left();
            drop(pin); // release reconstruction ownership before fresh lookup
            let new = engine().lookup(pc);
            ctx.set_pc(new.value.memory.address as u64);
            return;
        }
        // A stable revoked/unresolved direct or indirect gate can never authorize
        // old code. Cold lookup binds it to the current source generation.
        let pc = engine()
            .targets
            .iter()
            .find(|t| t.gate == ctx.pc())
            .map(|t| t.source)
            .unwrap_or_else(|| fail("unknown gateway", ctx.pc()));
        let new = engine().lookup(pc);
        ctx.set_pc(new.value.memory.address as u64);
    }
}
#[no_mangle]
extern "C" fn proof_main(stack: *const usize) -> ! {
    unsafe {
        linux::initialize(stack);
        linux::write("EXECUTOR_PROOF linked-arm-task-bound experimental\n");
        let mode = if *stack > 1 && syscall(nr::GETPID, 0, 0, 0, 0, 0, 0) != 1 {
            let arg = *stack.add(2) as *const u8;
            let len = (0..64)
                .find(|n| arg.add(*n).read_volatile() == 0)
                .unwrap_or_else(|| fail("argument length", 0));
            core::slice::from_raw_parts(arg, len)
        } else {
            b"default"
        };
        let benchmark = bench::Bench::parse(mode);
        let loop_budget = if benchmark.is_some() {
            49996
        } else {
            match mode {
                b"default" | b"gateway-fault" | b"indirect16" | b"indirect17" | b"return16"
                | b"return17" => 49996,
                b"zero" => 256,
                b"negative" => 0,
                b"overshoot" => -100001,
                _ => fail("unknown proof mode", 0),
            }
        };
        log(format_args!(
            "EXECUTOR_TEST loop-initial-budget={loop_budget}\n"
        ));
        let arena = Memory::anonymous(1024 * 1024, 3);
        let fd = checked(syscall(
            nr::MEMFD,
            c"linked-proof".as_ptr() as usize,
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
        let mut bytes = benchmark.map_or_else(arch::source, bench::Bench::source);
        let target_register = match mode {
            b"indirect16" | b"indirect17" => {
                let register = if mode == b"indirect16" { 16 } else { 17 };
                bytes[..4].copy_from_slice(&(0xd63f0000u32 | (register << 5)).to_le_bytes());
                Some((register as usize, source.address as u64 + 1536))
            }
            b"return16" | b"return17" => {
                let register = if mode == b"return16" { 16 } else { 17 };
                bytes[1544..1548].copy_from_slice(&(0xd65f0000u32 | (register << 5)).to_le_bytes());
                Some((register as usize, source.address as u64 + 4))
            }
            _ => None,
        };
        core::ptr::copy_nonoverlapping(bytes.as_ptr(), alias.address as *mut u8, 4096);
        arch::synchronize(alias.address, 4096);
        arch::synchronize(source.address, 4096);
        let guard = Memory::anonymous(4096, 0);
        let app_stack = Memory::anonymous(65536, 3);
        let tls = Memory::anonymous(4096, 3);
        arch::tls_set(tls.address as u64);
        (*STATE.0.get()).tls = tls.address as u64;
        let signal_stack = Memory::anonymous(131072, 3);
        linux::altstack(&signal_stack);
        let writer_stack = Memory::anonymous(131072, 3);
        linux::install(
            signal as *const () as usize,
            proof_restorer as *const () as usize,
        );
        let base = source.address as u64;
        if let Some(b) = benchmark {
            log(format_args!(
                "EXECUTOR_CORPUS base={base:#x} kind={:?}\n",
                b.kind
            ));
        }
        let offsets = benchmark.map_or_else(|| vec![0, 4, 8, 1536, 2048], bench::Bench::offsets);
        let targets: Vec<_> = offsets
            .iter()
            .enumerate()
            .map(|(i, offset)| Target {
                source: base + offset,
                gate: arena.address as u64 + 64 + i as u64 * 4,
            })
            .collect();
        (arena.address as *mut u32).write(aarch64::TRAP);
        for target in &targets {
            (target.gate as *mut u32).write(aarch64::TRAP);
        }
        arch::synchronize(arena.address, 4096);
        arena.protect(5);
        *CONFIG.0.get() = Some(Config {
            source,
            alias,
            guard,
            stack: app_stack,
            tls,
            child: 0,
            loop_budget,
            benchmark,
            gateway_fault: mode == b"gateway-fault",
            target_register,
        });
        *ENGINE.0.get() = Some(Engine {
            index: SourceIndex::default(),
            generations: vec![0; targets.len()],
            targets,
            codes: Vec::new(),
            arena,
            next: 4096,
            hooks: Vec::new(),
            hook_count: 0,
            translations: 0,
            emitted: 0,
            revoked: 0,
        });
        let start = bench::now();
        if !benchmark.is_some_and(|b| b.native) {
            for offset in benchmark.map_or_else(|| vec![0, 4, 8, 1536], bench::Bench::initial) {
                engine().lookup(base + offset);
            }
        }
        bench::cold(bench::now() - start);
        if benchmark.is_some() {
            proof_boot();
            fail("benchmark returned", 0)
        }
        linux::write("EXECUTOR_UNMARKED_ENTER\n");
        checked(syscall(nr::GETPID, 0, 0, 0, 0, 0, 0));
        linux::write("EXECUTOR_UNMARKED_RETURN\n");
        // This expected fatal helper fault occurs before any payment. Do not
        // create a writer that would remain blocked after the diagnostic exits.
        let child = if config().gateway_fault {
            0
        } else {
            checked(proof_clone(
                0x100 | 17,
                writer_stack.address + writer_stack.len,
            ))
        };
        (&mut *CONFIG.0.get()).as_mut().unwrap().child = child;
        proof_boot();
        fail("bootstrap returned", 0)
    }
}
