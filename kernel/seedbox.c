// SPDX-License-Identifier: GPL-2.0
/*
 * Seedbox deterministic time.
 *
 * Every kernel TSC read (rdtsc/rdtsc_ordered) returns virtual time published
 * by the VMM, never the hardware counter. Before the shared page is mapped,
 * reads use two port reads. Repeated reads with no new publication advance
 * by SEEDBOX_PV_READ_NS, so polling loops make progress deterministically.
 * User-mode RDTSC/RDTSCP trap through CR4.TSD and are emulated here.
 *
 * Once the page is active, interrupt masking (8259) and virtio-mmio interrupt
 * status go through the page instead of trapping port and MMIO accesses, and
 * the PIT is armed through the page when advertised, otherwise with a
 * single port write. The VMM applies page state only
 * at deterministic points.
 *
 * Guest progress is not measured by the host: compute is free, and the guest
 * exits at the clock bound and the syscall target the VMM publishes.
 */
#include <linux/build_bug.h>
#include <linux/stddef.h>
#include <linux/init.h>
#include <linux/io.h>
#include <linux/i8253.h>
#include <linux/kernel.h>
#include <linux/preempt.h>
#include <linux/sched.h>
#include <linux/printk.h>
#include <asm/cpufeature.h>
#include <asm/fixmap.h>
#include <asm/i8259.h>
#include <asm/insn-eval.h>
#include <asm/seedbox.h>
#include <asm/tlbflush.h>

struct seedbox_time_page *seedbox_page;
static unsigned long long seedbox_early_last;
struct task_struct *seedbox_user_charge;

/*
 * Account the current tick as user time (kernel/time/timer.c): it arrived
 * directly at a progress call's yield, not in a softirq or a nested interrupt.
 * One guest CPU, and no preemption between the mark and the yield.
 */
bool seedbox_user_tick(void)
{
	return READ_ONCE(seedbox_user_charge) == current && irq_count() == HARDIRQ_OFFSET;
}

noinstr unsigned long long seedbox_rdtsc(void)
{
	struct seedbox_time_page *p = READ_ONCE(seedbox_page);
	unsigned long long v, last;

	if (likely(p)) {
again:
		v = READ_ONCE(p->tsc);
		last = READ_ONCE(p->last);
		/*
		 * A read costs SEEDBOX_PV_READ_NS: compute is free, and a loop
		 * polling the clock would otherwise need a read per nanosecond
		 * it waits.
		 */
		last += SEEDBOX_PV_READ_NS - 1;
		if (v <= last)
			v = last + 1;
		/*
		 * Never show a time past the VMM's bound. The exit lets the VMM
		 * move time there (firing timers) and raise the bound.
		 */
		if (v > READ_ONCE(p->bound)) {
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
 * enabling. Before the page is mapped this is a no-op, so early boot does
 * not pay two port exits per cpu_relax.
 * The vCPU can stop at the bound with IF clear; IRQ delivery remains deferred
 * until the kernel permits it, exactly as for an ordinary kernel clock read.
 */
noinstr void seedbox_cpu_relax(void)
{
	if (READ_ONCE(seedbox_page))
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
	BUILD_BUG_ON(offsetof(struct seedbox_time_page, progress_ns) != 19 * sizeof(u64));
	BUILD_BUG_ON(offsetof(struct seedbox_time_page, progress_jitter_ns) != 20 * sizeof(u64));
	BUILD_BUG_ON(offsetof(struct seedbox_time_page, progress_jitter_seed) != 21 * sizeof(u64));
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
	WRITE_ONCE(p->pv_clock, 2);	/* tell the VMM this kernel has the PV clock */
	WRITE_ONCE(p->pic_imr, cached_irq_mask);
	outl(SEEDBOX_ABI, SEEDBOX_PORT_TIME_READY);
	WRITE_ONCE(seedbox_page, p);
	pr_info("seedbox: deterministic time page active\n");
}

/* splitmix64 on the page-seeded state. */
static u64 seedbox_jitter_next(u64 *state)
{
	u64 z = (*state += 0x9e3779b97f4a7c15ULL);

	z = (z ^ (z >> 30)) * 0xbf58476d1ce4e5b9ULL;
	z = (z ^ (z >> 27)) * 0x94d049bb133111ebULL;
	return z ^ (z >> 31);
}

/*
 * A progress call charges the compute it stands for: the VMM publishes the
 * nominal cost of one call (the instructions between calls at the configured
 * CPU speed) and a jitter bound, and the call costs the nominal charge plus a
 * seeded draw within the bound either way, so equal work takes a little
 * longer on some seeds than on others while the count-based plan stays
 * linear. The charge is counted in whole syscall costs; the remainder
 * carries to the next call, so the total charged converges on the total
 * owed. The VMM seeds the stream per machine at boot, at a reboot and at a
 * time fork; the state lives here, in guest RAM, so a restored snapshot
 * continues its own stream until it is reseeded. Called only from the
 * syscall hook on the one guest CPU.
 */
u64 seedbox_progress_syscalls(struct seedbox_time_page *p)
{
	static u64 state, seeded, carry;
	u64 base = READ_ONCE(p->progress_ns) ?: SEEDBOX_SYSCALL_NS;
	u64 bound = READ_ONCE(p->progress_jitter_ns);
	u64 charge = base + carry;

	if (bound > base)
		bound = base;
	if (bound) {
		u64 seed = READ_ONCE(p->progress_jitter_seed);

		if (seed != seeded) {
			state = seed;
			seeded = seed;
		}
		charge += seedbox_jitter_next(&state) % (2 * bound + 1) - bound;
	}
	carry = charge % SEEDBOX_SYSCALL_NS;
	return charge / SEEDBOX_SYSCALL_NS;
}

/*
 * A translator syscall costs a fraction of a syscall, as on ARM. The charge
 * is counted in whole syscall costs, so every SEEDBOX_TRANSLATOR_DIVISOR-th
 * one counts as a syscall; the count between them lives in guest RAM, like
 * the progress carry, so a restored snapshot continues it. Called only from
 * the syscall hook on the one guest CPU.
 */
u64 seedbox_translator_syscalls(void)
{
	static u64 pending;

	if (++pending < SEEDBOX_TRANSLATOR_DIVISOR)
		return 0;
	pending = 0;
	return 1;
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
