//! Bounded fixed-useful-work corpus. Native is deliberately unmetered. Time is
//! CLOCK_MONOTONIC on ordinary Linux, NOT guest simulated time. One untimed
//! warm-up precedes the measured body; setup/translation is reported separately.
use super::*;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Compute,
    Short,
    Jit,
}
#[derive(Clone, Copy)]
pub struct Bench {
    pub kind: Kind,
    pub native: bool,
    pub gate: bool,
}
impl Bench {
    pub fn parse(mode: &[u8]) -> Option<Self> {
        let (native, gate, name) = if let Some(s) = mode.strip_prefix(b"native-") {
            (true, false, s)
        } else if let Some(s) = mode.strip_prefix(b"bench-") {
            (false, false, s)
        } else {
            (false, true, mode.strip_prefix(b"gate-")?)
        };
        let kind = match name {
            b"compute" => Kind::Compute,
            b"short" => Kind::Short,
            b"jit" => Kind::Jit,
            _ => fail("benchmark name", 0),
        };
        Some(Self { kind, native, gate })
    }
    pub fn iterations(self) -> u64 {
        if self.gate {
            64
        } else {
            match self.kind {
                Kind::Compute => 2_000_000,
                Kind::Short => 10_000_000,
                Kind::Jit => 250_000,
            }
        }
    }
    pub fn rounds(self) -> u64 {
        if self.kind == Kind::Jit {
            8
        } else {
            1
        }
    }
    pub fn offsets(self) -> Vec<u64> {
        if self.kind == Kind::Short {
            vec![0, 8, 16, 64]
        } else {
            let mut v = vec![0, 512];
            if self.kind == Kind::Jit {
                v.extend((0..8).map(|n| 2048 + n * 8));
            }
            v
        }
    }
    pub fn initial(self) -> Vec<u64> {
        if self.kind == Kind::Short {
            vec![0, 8, 16, 64]
        } else {
            vec![0, 512]
        }
    }
    pub fn source(self) -> Vec<u8> {
        let mut bytes = vec![0; 4096];
        let mut put = |at: usize, w: u32| bytes[at..at + 4].copy_from_slice(&w.to_le_bytes());
        if self.kind == Kind::Short {
            put(0, 0x91000400); // ADD X0,X0,#1
            put(4, 0x9400000f); // BL source+64
            put(8, 0xf1000421); // SUBS X1,X1,#1
            put(12, 0x54ffffa1); // B.NE source
            put(16, arch::FAULT_WORD);
            put(20, 0x17ffffff);
            put(64, 0x91000c42); // ADD X2,X2,#3
            put(68, 0xd65f03c0); // RET (original LR)
        } else {
            for n in 0..126 {
                put(n * 4, 0x91000400);
            }
            put(504, 0xf1000421); // SUBS X1,X1,#1
            put(508, 0x54fff021); // B.NE source (imm19=-127)
            put(512, arch::FAULT_WORD);
            put(516, 0x17ffffff);
        }
        bytes
    }
}
struct Run {
    warmed: bool,
    round: u64,
    start: u64,
    cold: u64,
    first_pass: u64,
    checksum: u64,
    reclaimed: usize,
    pinned_peak: usize,
}
static RUN: Cell<Run> = Cell(UnsafeCell::new(Run {
    warmed: false,
    round: 0,
    start: 0,
    cold: 0,
    first_pass: 0,
    checksum: 0,
    reclaimed: 0,
    pinned_peak: 0,
}));
pub fn now() -> u64 {
    let mut t = [0u64; 2];
    checked(unsafe { syscall(113, 1, t.as_mut_ptr() as usize, 0, 0, 0, 0) });
    t[0] * 1_000_000_000 + t[1]
}
pub fn cold(ns: u64) {
    unsafe {
        (*RUN.0.get()).cold = ns;
    }
}
fn initialize(ctx: &mut arch::Context, b: Bench, iterations: u64) {
    let c = config();
    ctx.initialize(
        (c.stack.address + c.stack.len) as u64,
        c.guard.address as u64,
    );
    ctx.machine.regs[1] = iterations;
    ctx.machine.regs[2] = 0;
    let pc = c.source.address as u64;
    ctx.set_pc(if b.native {
        pc
    } else {
        unsafe { engine().gate(pc) }
    });
}
/// Called only at an actual application load fault, never a helper fault.
fn verify_finished(ctx: &mut arch::Context, b: Bench, iterations: u64, round: u64) {
    let c = config();
    let x0 = if b.kind == Kind::Short {
        iterations
    } else {
        iterations * (126 + if b.kind == Kind::Jit { round % 2 } else { 0 })
    };
    for n in 0..31 {
        let expected = match n {
            0 => x0,
            1 => 0,
            2 => {
                if b.kind == Kind::Short {
                    iterations * 3
                } else {
                    0
                }
            }
            20 => c.guard.address as u64,
            30 => {
                if b.kind == Kind::Short {
                    c.source.address as u64 + 8
                } else {
                    pattern(30)
                }
            }
            _ => pattern(n),
        };
        if ctx.machine.regs[n] != expected {
            fail("benchmark gpr/checksum", n as u64)
        }
    }
    if ctx.machine.sp != (c.stack.address + c.stack.len) as u64
        || ctx.machine.pstate & 0xf0000000 != 0x60000000
        || arch::tls_get() != c.tls.address as u64
    {
        fail("benchmark sp/flags/tls", ctx.machine.pstate)
    }
    let fp = &ctx.machine.reserved;
    if u32::from_le_bytes(fp[..4].try_into().unwrap()) != 0x46508001 {
        fail("benchmark fpsimd", 0)
    }
    for n in 0..32 {
        for lane in 0..2 {
            let at = 16 + n * 16 + lane * 8;
            let expected = n as u64 + if lane == 0 { 1 } else { 101 };
            if u64::from_le_bytes(fp[at..at + 8].try_into().unwrap()) != expected {
                fail("benchmark simd", (n * 2 + lane) as u64)
            }
        }
    }
    if u32::from_le_bytes(fp[8..12].try_into().unwrap()) != 0x10
        || u32::from_le_bytes(fp[12..16].try_into().unwrap()) != 0x400000
    {
        fail("benchmark fp controls", 0)
    }
}
pub unsafe fn signal(sig: i32, info: *const u8, ctx: &mut arch::Context, b: Bench) {
    let c = config();
    let base = c.source.address as u64;
    if sig == 5 && ctx.pc() == proof_boot as *const () as u64 {
        initialize(ctx, b, 1000);
        (*RUN.0.get()).start = now();
        return;
    }
    if !b.native && sig == 5 && ctx.pc() == engine().arena.address as u64 {
        ctx.machine.regs[16] = (*STATE.0.get()).x16;
        ctx.machine.regs[17] = (*STATE.0.get()).x17;
        arch::tls_set((*STATE.0.get()).tls);
        let charge = (*STATE.0.get()).gateway_charge as u32;
        let pc = (*STATE.0.get()).gateway_pc;
        if charge == 0 {
            fail("benchmark indirect miss", (*STATE.0.get()).target)
        }
        let pin = engine().index.lookup(pc).unwrap();
        let mut m = Meter::from_inline_debit(
            DEFAULT_BUDGET,
            (*STATE.0.get()).left,
            charge,
            *PAYMENTS.0.get(),
        )
        .unwrap();
        loop {
            checked(syscall(nr::GETPID, PROGRESS_MARK as usize, 0, 0, 0, 0, 0));
            if !m.paid_one().unwrap() {
                break;
            }
        }
        *PAYMENTS.0.get() = m.payments();
        m.refund_for_dispatch().unwrap();
        (*STATE.0.get()).left = m.left();
        if !engine().index.is_live(&pin) {
            fail("benchmark stale due pin", pin.id)
        }
        ctx.set_pc(engine().gate(pc));
        return;
    }
    if sig != 11 {
        fail("benchmark unexpected trap", ctx.pc())
    }
    let original = if b.native {
        ctx.pc()
    } else {
        let (pin, map) = engine()
            .at(ctx.pc())
            .unwrap_or_else(|| fail("benchmark unmapped fault", ctx.pc()));
        if map.instrumentation
            || !engine().index.is_live(&pin)
            || !matches!(map.phase, Phase::Running { .. })
        {
            fail("benchmark helper/stale fault", ctx.pc())
        }
        map.reconstruct(&mut ctx.machine.regs, arch::tls_get(), &*STATE.0.get());
        map.source
    };
    if original != base + if b.kind == Kind::Short { 16 } else { 512 }
        || (info.add(16) as *const u64).read_unaligned() != c.guard.address as u64
    {
        fail("benchmark fault identity", original)
    }
    let r = &mut *RUN.0.get();
    if !r.warmed {
        r.first_pass = now() - r.start;
        verify_finished(ctx, b, 1000, 0);
        r.warmed = true;
        *PAYMENTS.0.get() = 0;
        (*STATE.0.get()).left = 50000;
        initialize(ctx, b, b.iterations());
        r.start = now();
        return;
    }
    verify_finished(ctx, b, b.iterations(), r.round);
    r.checksum += ctx.machine.regs[0] + ctx.machine.regs[2];
    r.round += 1;
    if r.round < b.rounds() {
        // Append beside live code, retain the old generation across mutation,
        // revoke ALL incoming gates before writing, then release without waiting.
        let old = if b.native {
            None
        } else {
            engine().index.lookup(base)
        };
        let weak = old.as_ref().map(Arc::downgrade);
        let append = 2048 + (r.round - 1) * 8;
        ((c.alias.address + append as usize) as *mut u32).write(0xd28000e7);
        ((c.alias.address + append as usize + 4) as *mut u32).write(0xd65f03c0);
        arch::synchronize(c.alias.address + append as usize, 8);
        arch::synchronize(c.source.address + append as usize, 8);
        if !b.native {
            if !engine().index.is_live(old.as_ref().unwrap()) {
                fail("benchmark append evicted neighbor", 0)
            }
            engine().lookup(base + append);
            if engine().invalidate(0) != 1 {
                fail("benchmark overwrite dependency", 0)
            }
            r.pinned_peak = r.pinned_peak.max(old.as_ref().unwrap().value.memory.len);
            if old.as_ref().unwrap().value.block.maps.is_empty() {
                fail("benchmark pinned maps", 0)
            }
        }
        (c.alias.address as *mut u32).write(if r.round.is_multiple_of(2) {
            0x91000400
        } else {
            0x91000800
        });
        arch::synchronize(c.alias.address, 4);
        arch::synchronize(c.source.address, 4);
        if let Some(old) = old {
            let len = old.value.memory.len;
            drop(old);
            if weak.unwrap().upgrade().is_some() {
                fail("benchmark code leaked", 0)
            }
            r.reclaimed += len;
            engine().lookup(base);
        }
        initialize(ctx, b, b.iterations());
        return;
    }
    let elapsed = now() - r.start;
    let logical =
        b.iterations() * b.rounds() * if b.kind == Kind::Short { 6 } else { 128 } + b.rounds() * 2;
    if !b.native
        && (*STATE.0.get()).left != 50000 + *PAYMENTS.0.get() as i64 * 50000 - logical as i64
    {
        fail("benchmark logical ledger", logical)
    }
    let mut usage = [0u64; 32];
    checked(syscall(165, 0, usage.as_mut_ptr() as usize, 0, 0, 0, 0));
    log(format_args!("EXECUTOR_BENCH kind={:?} native={} gate={} iterations={} rounds={} checksum={} logical={} payments={} remaining={} warm_ns={} install_ns={} first_pass_ns={} translations={} emitted={} live={} live_code_bytes={} arena_reserved_bytes=1048576 pinned_peak={} reclaimed={} maxrss_kib={}\n",b.kind,b.native,b.gate,b.iterations(),b.rounds(),r.checksum,logical,*PAYMENTS.0.get(),(*STATE.0.get()).left,elapsed,r.cold,r.first_pass,engine().translations,engine().emitted,engine().index.len(),engine().index.len()*4096,r.pinned_peak,r.reclaimed,usage[4]));
    linux::exit(0)
}
