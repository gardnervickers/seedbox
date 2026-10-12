//! Diagnostic observations of the stable raw gateway. Timing never traps here.
//! The observer reconstructs a COPY and resumes the identical hardware frame;
//! it neither makes the marked syscall nor pays/refunds on behalf of assembly.
use super::*;
use linked::gateway::{Observe, Stage};
static PIN: Cell<Option<Pin<Code>>> = Cell(UnsafeCell::new(None));
static LOOPS: Cell<u64> = Cell(UnsafeCell::new(0));

pub unsafe fn install_hooks() {
    let e = engine();
    let page = Memory {
        address: e.arena.address,
        len: 4096,
    };
    page.protect(3);
    for map in &e.raw_gateway.as_ref().unwrap().maps {
        if map.observe.is_none() {
            e.hooks.push((map.pc, (map.pc as *const u32).read()));
            (map.pc as *mut u32).write(aarch64::TRAP);
        }
    }
    arch::synchronize(page.address, page.len);
    page.protect(5);
    core::mem::forget(page);
}

pub unsafe fn signal(sig: i32, ctx: &mut arch::Context) -> bool {
    let Some(map) = engine().raw_gateway.as_ref().and_then(|g| g.map(ctx.pc())) else {
        return false;
    };
    if sig != 5 {
        fail("raw gateway fault", ctx.pc());
    }
    let c = config();
    if c.benchmark.is_some() || c.jit.is_some() {
        fail("unexpected raw timing trap", ctx.pc());
    }
    let state = &*STATE.0.get();
    let pc = state.gateway_pc;
    let charge = state.gateway_charge;
    // The first observational stop owns a generation pin BEFORE the actual
    // syscall. Later calls in the same overshoot transaction keep that pin.
    if (&*PIN.0.get()).is_none() && map.stage == Stage::Debited {
        *PIN.0.get() = Some(engine().index.lookup(pc).unwrap());
        *LOOPS.0.get() =
            ((c.loop_budget + state.payments as i64 * 50000 - state.left - charge as i64) / 256)
                as u64;
    }
    let hardware = ctx.machine.regs;
    map.reconstruct(&mut ctx.machine.regs, state);
    verify_at(ctx, pc, *LOOPS.0.get());
    if arch::tls_get() != c.tls.address as u64 {
        fail("raw hardware tls", ctx.pc());
    }
    // Independent expected balance at each partial transition. Returned has
    // not committed time; BudgetCommitted has not committed the counter yet.
    let committed = state.payments + u64::from(map.stage == Stage::BudgetCommitted);
    let expected = c.loop_budget + committed as i64 * 50000
        - *LOOPS.0.get() as i64 * 256
        - if map.stage == Stage::Refunded {
            0
        } else {
            charge as i64
        };
    if state.left != expected {
        fail("raw partial ledger", ctx.pc());
    }
    ctx.machine.regs = hardware;
    match map.observe {
        Some(Observe::Enter) => linux::write("EXECUTOR_PV_ENTER\n"),
        Some(Observe::Returned) => linux::write("EXECUTOR_PV_RETURN\n"),
        Some(Observe::Paid) if state.payments == 3 => {
            let pin = (&*PIN.0.get()).as_ref().unwrap().clone();
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
                fail("raw parked generation", status as u64);
            }
            linux::write("EXECUTOR_PIN raw-paid-old-map-readable writer-finished-before-refund\n");
        }
        Some(Observe::Refunded) => {
            // No cache return PC exists in the raw gateway. Old reconstruction
            // ownership ends here; the upcoming branch still passes its gate.
            *PIN.0.get() = None;
        }
        Some(Observe::Paid) => (),
        None => {
            let slot = engine()
                .hooks
                .iter()
                .position(|(pc, _)| *pc == map.pc)
                .unwrap_or_else(|| fail("unknown raw hook", map.pc));
            let (_, word) = engine().hooks.remove(slot);
            let page = Memory {
                address: engine().arena.address,
                len: 4096,
            };
            page.protect(3);
            (map.pc as *mut u32).write(word);
            arch::synchronize(map.pc as usize, 4);
            page.protect(5);
            core::mem::forget(page);
            engine().hook_count += 1;
            log(format_args!("EXECUTOR_RAW_BOUNDARY offset={} stage={:?} x0_saved={} x8_saved={} left={} exact\n",
                (map.pc-engine().arena.address as u64-256)/4,map.stage,map.x0_saved,map.x8_saved,state.left));
        }
    }
    if map.observe.is_some() {
        ctx.set_pc(ctx.pc() + 4);
    }
    true
}
