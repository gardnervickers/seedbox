// SPDX-License-Identifier: GPL-2.0
/*
 * Seedbox deterministic time.
 *
 * Every kernel TSC read (rdtsc/rdtsc_ordered) returns virtual time published
 * by the VMM, never the hardware counter. Before the shared page is mapped,
 * reads use two port reads. Repeated reads with no new publication advance
 * by one tick (SEEDBOX_PV_READ_NS under the PV clock), so polling loops make
 * progress deterministically. User-mode
 * RDTSC/RDTSCP trap through CR4.TSD and are emulated here.
 *
 * Once the page is active, interrupt masking (8259) and virtio-mmio interrupt
 * status go through the page instead of trapping port and MMIO accesses, and
 * the PIT is armed through the page when advertised, otherwise with a
 * single port write. The VMM applies page state only
 * at deterministic points.
 *
 * In the VMM's PV clock timebase (no host perf counter), guest progress is
 * not measured: the guest exits at a clock bound and a syscall budget the VMM
 * publishes instead. Both checks sit behind a static key, so in the counter
 * timebase they compile to no-ops and retire no extra branches.
 */
#include <linux/build_bug.h>
#include <linux/stddef.h>
#include <linux/init.h>
#include <linux/io.h>
#include <linux/i8253.h>
#include <linux/kernel.h>
#include <linux/printk.h>
#include <asm/cpufeature.h>
#include <asm/fixmap.h>
#include <asm/i8259.h>
#include <asm/insn-eval.h>
#include <asm/seedbox.h>
#include <asm/tlbflush.h>

struct seedbox_time_page *seedbox_page;
static unsigned long long seedbox_early_last;
DEFINE_STATIC_KEY_FALSE(seedbox_pv_clock);

noinstr unsigned long long seedbox_rdtsc(void)
{
	struct seedbox_time_page *p = READ_ONCE(seedbox_page);
	unsigned long long v, last;

	if (likely(p)) {
again:
		v = READ_ONCE(p->tsc);
		last = READ_ONCE(p->last);
		/*
		 * A read costs 1 ns, or SEEDBOX_PV_READ_NS under the PV clock,
		 * where compute is free and a loop polling the clock would
		 * otherwise need a read per nanosecond it waits. The static key
		 * retires no conditional branch in the counter timebase.
		 */
		if (static_branch_unlikely(&seedbox_pv_clock))
			last += SEEDBOX_PV_READ_NS - 1;
		if (v <= last)
			v = last + 1;
		/*
		 * PV clock: never show a time past the VMM's bound. The exit lets
		 * the VMM move time there (firing timers) and raise the bound.
		 */
		if (static_branch_unlikely(&seedbox_pv_clock) &&
		    v > READ_ONCE(p->bound)) {
			outl(1, SEEDBOX_PORT_YIELD);
			goto again;
		}
		WRITE_ONCE(p->last, v);
		return v;
	}
	v = inl(SEEDBOX_PORT_TIME_LO);
	v |= (unsigned long long)inl(SEEDBOX_PORT_TIME_HI) << 32;
	if (v <= seedbox_early_last)
		v = seedbox_early_last + 1;
	seedbox_early_last = v;
	return v;
}

/*
 * cpu_relax/rep_nop can run with locks held and interrupts disabled. Reuse the
 * bounded clock-read path: no allocation, scheduling, locking or interrupt
 * enabling. Before PV activation (and in counter mode) this is a no-op.
 * The vCPU can stop at the bound with IF clear; IRQ delivery remains deferred
 * until the kernel permits it, exactly as for an ordinary kernel clock read.
 */
noinstr void seedbox_cpu_relax(void)
{
	if (static_branch_unlikely(&seedbox_pv_clock))
		(void)seedbox_rdtsc();
}

bool seedbox_emulate_user_tsc(struct pt_regs *regs)
{
	unsigned char buf[MAX_INSN_SIZE];
	unsigned long long v;
	int n, len;
	bool tscp;

	n = insn_fetch_from_user(regs, buf);
	if (n >= 2 && buf[0] == 0x0f && buf[1] == 0x31) {
		len = 2;
		tscp = false;
	} else if (n >= 3 && buf[0] == 0x0f && buf[1] == 0x01 && buf[2] == 0xf9) {
		len = 3;
		tscp = true;
	} else {
		return false;
	}
	v = seedbox_rdtsc();
	regs->ax = (u32)v;
	regs->dx = (u32)(v >> 32);
	if (tscp)
		regs->cx = 0;
	regs->ip += len;
	return true;
}

/* The virtual TSC runs at exactly 1 GHz: no calibration against the PIT. */
unsigned long seedbox_tsc_khz(void)
{
	setup_force_cpu_cap(X86_FEATURE_TSC_KNOWN_FREQ);
	return SEEDBOX_TSC_KHZ;
}

void __init seedbox_time_init(void)
{
	struct seedbox_time_page *p;

	/* Pin the u64-indexed ABI shared with the VMM. */
	BUILD_BUG_ON(offsetof(struct seedbox_time_page, pv_clock) != 6 * sizeof(u64));
	BUILD_BUG_ON(offsetof(struct seedbox_time_page, bound) != 7 * sizeof(u64));
	BUILD_BUG_ON(offsetof(struct seedbox_time_page, syscall_target) != 8 * sizeof(u64));
	BUILD_BUG_ON(offsetof(struct seedbox_time_page, entropy_gen) != 9 * sizeof(u64));
	BUILD_BUG_ON(offsetof(struct seedbox_time_page, entropy[0]) != 10 * sizeof(u64));
	BUILD_BUG_ON(offsetof(struct seedbox_time_page, entropy[1]) != 11 * sizeof(u64));
	BUILD_BUG_ON(offsetof(struct seedbox_time_page, entropy[2]) != 12 * sizeof(u64));
	BUILD_BUG_ON(offsetof(struct seedbox_time_page, entropy[3]) != 13 * sizeof(u64));
	BUILD_BUG_ON(offsetof(struct seedbox_time_page, timer_deadline) != 14 * sizeof(u64));
	BUILD_BUG_ON(offsetof(struct seedbox_time_page, capabilities) != 15 * sizeof(u64));
	BUILD_BUG_ON(offsetof(struct seedbox_time_page, syscall_ns) != 16 * sizeof(u64));
	BUILD_BUG_ON(offsetof(struct seedbox_time_page, syscalls0) != 17 * sizeof(u64));
	BUILD_BUG_ON(offsetof(struct seedbox_time_page, plan_time) != 18 * sizeof(u64));
	BUILD_BUG_ON(SEEDBOX_ABI != 4);
	BUILD_BUG_ON(SEEDBOX_CAP_PAGE_TIMER != 1);

	/*
	 * setup_arch calls us after jump_label_init and early_ioremap_init,
	 * before tsc_early_init selects delay_tsc. A permanent, write-back
	 * fixmap needs no allocator and survives the boot page-table switch.
	 * In particular, the PIC's 8 * 2 us + 100 us initialization delays
	 * must poll this page, not make two port exits per nanosecond.
	 */
	set_fixmap(FIX_SEEDBOX_TIME, SEEDBOX_TIME_PAGE_PHYS);
	p = (struct seedbox_time_page *)fix_to_virt(FIX_SEEDBOX_TIME);
	WRITE_ONCE(p->last, seedbox_early_last);
	if (READ_ONCE(p->pv_clock)) {
		static_branch_enable(&seedbox_pv_clock);
		WRITE_ONCE(p->pv_clock, 2);	/* tell the VMM this kernel has it */
	}
	WRITE_ONCE(p->pic_imr, cached_irq_mask);
	outl(SEEDBOX_ABI, SEEDBOX_PORT_TIME_READY);
	WRITE_ONCE(seedbox_page, p);
	pr_info("seedbox: deterministic time page active\n");
}

bool seedbox_set_next_event(unsigned long ticks)
{
	struct seedbox_time_page *p = READ_ONCE(seedbox_page);
	u64 now, deadline, reload = (u16)ticks ?: 65536;

	if (!p || !(READ_ONCE(p->capabilities) & SEEDBOX_CAP_PAGE_TIMER))
		return false;
	/* Match the PIT's round-up conversion, without another clock read. */
	now = max(READ_ONCE(p->tsc), READ_ONCE(p->last));
	deadline = now + DIV_ROUND_UP_ULL(reload * 1000000000ULL, PIT_TICK_RATE);
	WRITE_ONCE(p->timer_deadline, deadline);
	if (static_branch_unlikely(&seedbox_pv_clock)) {
		if (deadline < READ_ONCE(p->bound)) {
			u64 t0 = READ_ONCE(p->plan_time);
			u64 s0 = READ_ONCE(p->syscalls0);
			u64 ns = READ_ONCE(p->syscall_ns);
			u64 left = deadline > t0 ? deadline - t0 : 0;

			/* The VMM keeps this origin fixed through watchdog splits.
			 * Keep an earlier bound on a later re-arm: only a legal
			 * exit may publish a new plan. This also keeps left=0's
			 * target at s0+1, even if that syscall already happened.
			 */
			WRITE_ONCE(p->bound, deadline);
			WRITE_ONCE(p->syscall_target,
				   s0 + max(1ULL, DIV_ROUND_UP_ULL(left, ns)));
		}
	}
	return true;
}

void seedbox_timer_disarm(void)
{
	struct seedbox_time_page *p = READ_ONCE(seedbox_page);

	if (p && (READ_ONCE(p->capabilities) & SEEDBOX_CAP_PAGE_TIMER))
		WRITE_ONCE(p->timer_deadline, SEEDBOX_TIMER_DISARMED);
}

static int __init seedbox_user_tsc_init(void)
{
	/* cpu_init clears TSD after setup_arch; enable user traps afterwards. */
	cr4_set_bits(X86_CR4_TSD);
	return 0;
}
early_initcall(seedbox_user_tsc_init);
