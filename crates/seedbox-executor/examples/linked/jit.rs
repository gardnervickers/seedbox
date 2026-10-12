//! Matched, executed publication experiment. All source instructions execute
//! through the selected engine; only the controlled stopped writer/driver is
//! native in candidate mode (DR instruments that same driver). No eager JIT
//! translation, unmetered application escape, or general writer discovery.
use super::*;
const ROUNDS: u64 = 64;
const SLOTS: u64 = 8;
const NEIGHBOR: u64 = 512;
const SLOT: u64 = 1024;
const WIDTH: usize = 56;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Zero,
    Append,
    Patch,
}
#[derive(Clone, Copy, Debug)]
pub struct Jit {
    pub native: bool,
    pub diagnostic: bool,
    pub kind: Kind,
    pub calls: u64,
}
impl Jit {
    pub fn parse(mode: &[u8]) -> Option<Self> {
        let (native, name) = if let Some(s) = mode.strip_prefix(b"jit-native-") {
            (true, s)
        } else {
            (false, mode.strip_prefix(b"jit-linked-")?)
        };
        let (diagnostic, name) = if let Some(s) = name.strip_prefix(b"diag-") {
            (true, s)
        } else {
            (false, name)
        };
        let (kind, calls) = match name {
            b"zero" => (Kind::Zero, 8192),
            b"zero-medium" => (Kind::Zero, 512),
            b"zero-high" => (Kind::Zero, 32),
            b"append-low" => (Kind::Append, 8192),
            b"append-medium" => (Kind::Append, 512),
            b"append-high" => (Kind::Append, 32),
            b"patch-low" => (Kind::Patch, 8192),
            b"patch-medium" => (Kind::Patch, 512),
            b"patch-high" => (Kind::Patch, 32),
            _ => fail("jit mode", 0),
        };
        Some(Self {
            native,
            diagnostic,
            kind,
            calls,
        })
    }
    pub fn offsets(self) -> Vec<u64> {
        let mut v = vec![0, 4, 12, NEIGHBOR];
        v.extend((0..SLOTS).map(|n| SLOT + n * 64));
        v
    }
    pub fn source(self) -> Vec<u8> {
        let mut bytes = vec![0; 4096];
        for (i, word) in [
            0xd63f0060u32,
            0xf1000421,
            0x54ffffc1,
            arch::FAULT_WORD,
            0x17ffffff,
        ]
        .iter()
        .enumerate()
        {
            bytes[i * 4..i * 4 + 4].copy_from_slice(&word.to_le_bytes());
        }
        for offset in [NEIGHBOR, SLOT] {
            bytes[offset as usize..offset as usize + WIDTH].copy_from_slice(&function(1));
        }
        bytes
    }
    fn target(self, round: u64) -> u64 {
        SLOT + if self.kind == Kind::Append {
            round % SLOTS * 64
        } else {
            0
        }
    }
    fn version(self, round: u64) -> u64 {
        if self.kind == Kind::Zero {
            1
        } else {
            round + 2
        }
    }
}
fn function(version: u64) -> [u8; WIDTH] {
    let mut bytes = [0; WIDTH];
    for n in 0..12 {
        let immediate = if n == 0 { version } else { 1 };
        bytes[n * 4..n * 4 + 4]
            .copy_from_slice(&(0x91000000u32 | (immediate as u32) << 10).to_le_bytes());
    }
    bytes[48..52].copy_from_slice(&0x91000442u32.to_le_bytes()); // independent call count X2
    bytes[52..56].copy_from_slice(&0xd65f03c0u32.to_le_bytes());
    bytes
}
struct Run {
    warmup: u64,
    round: u64,
    phase: u64,
    start: u64,
    total_start: u64,
    first_ns: u64,
    warm_ns: u64,
    publish_ns: u64,
    index_ns: u64,
    cold_ns: u64,
    calls: u64,
    checksum: u64,
    checks: u64,
    probes: usize,
    gate_probes: usize,
    removed: usize,
    reclaimed: usize,
    neighbor_id: u64,
    neighbor_reuse: u64,
    resolutions: u64,
    translation_ns: u64,
}
static RUN: Cell<Run> = Cell(UnsafeCell::new(Run {
    warmup: 0,
    round: 0,
    phase: 0,
    start: 0,
    total_start: 0,
    first_ns: 0,
    warm_ns: 0,
    publish_ns: 0,
    index_ns: 0,
    cold_ns: 0,
    calls: 0,
    checksum: 0,
    checks: 0,
    probes: 0,
    gate_probes: 0,
    removed: 0,
    reclaimed: 0,
    neighbor_id: 0,
    neighbor_reuse: 0,
    resolutions: 0,
    translation_ns: 0,
}));
fn enter(ctx: &mut arch::Context, b: Jit, target: u64, calls: u64) {
    let c = config();
    ctx.initialize(
        (c.stack.address + c.stack.len) as u64,
        c.guard.address as u64,
    );
    ctx.machine.regs[1] = calls;
    ctx.machine.regs[2] = 0;
    ctx.machine.regs[3] = c.source.address as u64 + target;
    ctx.set_pc(if b.native {
        c.source.address as u64
    } else {
        unsafe { engine().gate(c.source.address as u64) }
    });
}
fn verify(ctx: &arch::Context, calls: u64, version: u64, target: u64) {
    let c = config();
    for n in 0..31 {
        let expected = match n {
            0 => calls * (version + 11),
            1 => 0,
            2 => calls,
            3 => c.source.address as u64 + target,
            20 => c.guard.address as u64,
            30 => c.source.address as u64 + 4,
            _ => pattern(n),
        };
        if ctx.machine.regs[n] != expected {
            fail("jit version/call/register", n as u64)
        }
    }
    if ctx.machine.sp != (c.stack.address + c.stack.len) as u64
        || ctx.machine.pstate & 0xf0000000 != 0x60000000
        || arch::tls_get() != c.tls.address as u64
    {
        fail("jit sp/flags/tls", ctx.machine.pstate)
    }
    let fp = &ctx.machine.reserved;
    if u32::from_le_bytes(fp[..4].try_into().unwrap()) != 0x46508001
        || u32::from_le_bytes(fp[4..8].try_into().unwrap()) != 528
        || u32::from_le_bytes(fp[8..12].try_into().unwrap()) != 0x10
        || u32::from_le_bytes(fp[12..16].try_into().unwrap()) != 0x400000
    {
        fail("jit fp record", 0)
    }
    for n in 0..32 {
        for lane in 0..2 {
            let at = 16 + n * 16 + lane * 8;
            if u64::from_le_bytes(fp[at..at + 8].try_into().unwrap())
                != n as u64 + if lane == 0 { 1 } else { 101 }
            {
                fail("jit simd", (n * 2 + lane) as u64)
            }
        }
    }
}
unsafe fn publish(b: Jit) {
    let r = &mut *RUN.0.get();
    if b.kind == Kind::Zero {
        return;
    }
    let c = config();
    let offset = b.target(r.round);
    let width = if b.kind == Kind::Patch { 4 } else { WIDTH };
    // No pin crosses publication here. The separate parked256->3 test owns
    // that proof. Revocation occurs before source mutation, not after IC.
    if !b.native {
        let start = bench::now();
        let weak = engine()
            .index
            .lookup(c.source.address as u64 + offset)
            .map(|p| Arc::downgrade(&p));
        let (removed, probes) = engine().invalidate_range(offset, width as u64);
        r.index_ns += bench::now() - start;
        r.probes += probes;
        r.gate_probes += engine().targets.len();
        r.removed += removed;
        if removed > 1 {
            fail("jit unrelated invalidation", removed as u64)
        }
        if let Some(weak) = weak {
            if weak.upgrade().is_some() {
                fail("jit reclaim", 0)
            }
        }
        r.reclaimed += removed * 4096;
        let neighbor = engine()
            .index
            .lookup(c.source.address as u64 + NEIGHBOR)
            .unwrap();
        if neighbor.id != r.neighbor_id {
            fail("jit neighbor generation", neighbor.id)
        }
        r.neighbor_reuse += 1;
    }
    let bytes = function(b.version(r.round));
    let start = bench::now();
    for at in (0..width).step_by(4) {
        ((c.alias.address + offset as usize + at) as *mut u32)
            .write_volatile(u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap()));
    }
    arch::synchronize(c.alias.address + offset as usize, width);
    arch::synchronize(c.source.address + offset as usize, width);
    r.publish_ns += bench::now() - start;
}
pub unsafe fn signal(sig: i32, info: *const u8, ctx: &mut arch::Context, b: Jit) {
    let c = config();
    let base = c.source.address as u64;
    if sig == 5 && ctx.pc() == proof_boot as *const () as u64 {
        enter(ctx, b, NEIGHBOR, 32);
        (*RUN.0.get()).start = bench::now();
        return;
    }
    if !b.native && sig == 5 {
        if ctx.pc() == engine().arena.address as u64 {
            ctx.machine.regs[16] = (*STATE.0.get()).x16;
            ctx.machine.regs[17] = (*STATE.0.get()).x17;
            arch::tls_set((*STATE.0.get()).tls);
            let charge = (*STATE.0.get()).gateway_charge as u32;
            if charge == 0 {
                fail("jit unknown indirect target", (*STATE.0.get()).target)
            }
            let pc = (*STATE.0.get()).gateway_pc;
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
                fail("jit stale due", pin.id)
            }
            ctx.set_pc(engine().gate(pc));
        } else {
            let pc = engine()
                .targets
                .iter()
                .find(|t| t.gate == ctx.pc())
                .map(|t| t.source)
                .unwrap_or_else(|| fail("jit unmapped trap", ctx.pc()));
            // The finite target set is declared before emission. Every initial
            // entry/reused slot resolves through its revocable stable gate.
            let start = if b.diagnostic { bench::now() } else { 0 };
            let pin = engine().lookup(pc);
            if b.diagnostic {
                (*RUN.0.get()).translation_ns += bench::now() - start;
            }
            (*RUN.0.get()).resolutions += 1;
            ctx.set_pc(pin.value.memory.address as u64);
        }
        return;
    }
    if sig != 11 {
        fail("jit signal", sig as u64)
    }
    let stop = bench::now();
    let original = if b.native {
        ctx.pc()
    } else {
        let (pin, map) = engine()
            .at(ctx.pc())
            .unwrap_or_else(|| fail("jit helper fault", ctx.pc()));
        if map.instrumentation
            || !engine().index.is_live(&pin)
            || map.phase != (Phase::Running { instructions: 2 })
        {
            fail("jit stale/fault phase", ctx.pc())
        }
        map.reconstruct(&mut ctx.machine.regs, arch::tls_get(), &*STATE.0.get());
        map.source
    };
    if original != base + 12
        || (info.add(16) as *const u64).read_unaligned() != c.guard.address as u64
    {
        fail("jit fault identity", original)
    }
    let r = &mut *RUN.0.get();
    if r.warmup < 2 {
        let second_target = if b.kind == Kind::Append {
            NEIGHBOR
        } else {
            SLOT
        };
        verify(
            ctx,
            32,
            1,
            if r.warmup == 0 {
                NEIGHBOR
            } else {
                second_target
            },
        );
        r.cold_ns += stop - r.start;
        r.warmup += 1;
        if r.warmup == 1 {
            enter(ctx, b, second_target, 32);
            r.start = bench::now();
            return;
        }
        *PAYMENTS.0.get() = 0;
        (*STATE.0.get()).left = 50000;
        if !b.native {
            r.neighbor_id = engine().index.lookup(base + NEIGHBOR).unwrap().id;
        }
        checked(syscall(nr::GETPID, 0x4a495401, 0, 0, 0, 0, 0));
        r.total_start = bench::now();
        publish(b);
        enter(ctx, b, b.target(0), 1);
        r.start = bench::now();
        return;
    }
    let neighbor = r.phase >= 2;
    let first = r.phase.is_multiple_of(2);
    let calls = if first { 1 } else { b.calls - 1 };
    let version = if neighbor { 1 } else { b.version(r.round) };
    let target = if neighbor {
        NEIGHBOR
    } else {
        b.target(r.round)
    };
    verify(ctx, calls, version, target);
    if first {
        r.first_ns += stop - r.start;
    } else {
        r.warm_ns += stop - r.start;
    }
    r.calls += calls;
    r.checksum += calls * (version + 11);
    r.checks += 1;
    if b.diagnostic {
        log(format_args!(
            "JIT_VERSION round={} phase={} target={} version={} calls={} checksum={}\n",
            r.round,
            r.phase,
            target,
            version,
            calls,
            calls * (version + 11)
        ));
    }
    r.phase += 1;
    if r.phase == 4 {
        r.phase = 0;
        r.round += 1;
        if r.round == ROUNDS {
            let total = bench::now() - r.total_start;
            checked(syscall(nr::GETPID, 0x4a495402, 0, 0, 0, 0, 0));
            let logical = r.calls * 17 + r.checks * 2;
            if r.calls != ROUNDS * b.calls * 2 || r.checks != ROUNDS * 4 {
                fail("jit exact work", r.calls)
            }
            if !b.native
                && (*STATE.0.get()).left
                    != 50000 + *PAYMENTS.0.get() as i64 * 50000 - logical as i64
            {
                fail("jit ledger", logical)
            }
            let e = engine();
            let maps: usize = e
                .codes
                .iter()
                .filter_map(Weak::upgrade)
                .map(|p| p.value.block.maps.len())
                .sum();
            log(format_args!("JIT_RESULT kind={:?} native={} diagnostic={} rounds={} calls={} publications={} store_width={} checksum={} checks={} logical={} payments={} remaining={} total_ns={} write_barrier_ns={} index_gate_ns={} first_ns={} warm_ns={} cold_ns={} translations={} resolutions={} translation_diagnostic_ns={} invalidated={} neighbor_reuse={} overlap_probes={} gate_probes={} gate_updates={} emitted={} live={} live_code_bytes={} pinned_bytes=0 reclaimed={} arena_used={} weak_slots={} live_maps={} map_bytes={}\n", b.kind,b.native,b.diagnostic,ROUNDS,r.calls,if b.kind==Kind::Zero{0}else{ROUNDS},if b.kind==Kind::Patch{4}else{WIDTH},r.checksum,r.checks,logical,*PAYMENTS.0.get(),(*STATE.0.get()).left,total,r.publish_ns,r.index_ns,r.first_ns,r.warm_ns,r.cold_ns,e.translations,r.resolutions,r.translation_ns,r.removed,r.neighbor_reuse,r.probes,r.gate_probes,e.translations+e.revoked,e.emitted,e.index.len(),e.index.len()*4096,r.reclaimed,e.next,e.codes.len(),maps,maps*core::mem::size_of::<linked::Map>()));
            linux::exit(0)
        }
        publish(b);
    }
    let target = if r.phase >= 2 {
        NEIGHBOR
    } else {
        b.target(r.round)
    };
    let calls = if r.phase.is_multiple_of(2) {
        1
    } else {
        b.calls - 1
    };
    enter(ctx, b, target, calls);
    r.start = bench::now();
}
