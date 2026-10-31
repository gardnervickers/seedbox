// SPDX-License-Identifier: GPL-2.0
/* Single-CPU PV ABI 3. Matches seedbox-machine stopped worker; no architectural timer. */
#include <linux/clockchips.h>
#include <linux/clocksource.h>
#include <linux/init.h>
#include <linux/interrupt.h>
#include <linux/io.h>
#include <linux/irq.h>
#include <linux/of.h>
#include <linux/of_irq.h>
#include <linux/preempt.h>
#include <linux/sched.h>
#include <linux/sched_clock.h>
#include <linux/sizes.h>
#include <asm/ptrace.h>
#include <asm/seedbox.h>
#include <asm/sysreg.h>

#define TIME_IPA 0x0a000000UL
#define EVENT_IPA 0x0a010000UL
/* A progress call: getpid with this first argument (guest/progress/progress.c). */
#define SEEDBOX_PROGRESS_NR 172
#define SEEDBOX_PROGRESS_MARK 0x5eedb0c5UL
/* Time syscalls (clock_gettime, clock_getres, gettimeofday). */
#define SEEDBOX_CLOCK_GETTIME_NR 113
#define SEEDBOX_CLOCK_GETRES_NR 114
#define SEEDBOX_GETTIMEOFDAY_NR 169
/* x7 of a syscall the translator makes for itself (the vendor/dynamorio
 * "Seedbox: translator-syscalls" change); arm64 syscalls leave x7 unused. */
#define SEEDBOX_TRANSLATOR_MARK 0x5eedd1a900000000UL
/* Translator syscalls cost this fraction of an application syscall. */
#define SEEDBOX_TRANSLATOR_DIVISOR 10
struct seedbox_page {
	u64 now, last, bound, abi, deferred_irq_pending, exclusive_epoch;
	u64 syscall_ns;
	u64 x86_only[4];	/* Words 56-80: x86 PIC and virtio publications. */
	u64 preempt_ppm, preempt_seed;
};
static struct seedbox_page *page;
static void __iomem *event;
static u64 early_last;
/* Shared with the stopped VMM, which invalidates reservations on guest exits
 * and device writes. Initialized before the first return to userspace. */
u64 *seedbox_exclusive_epoch;
static struct clock_event_device ce;

u64 notrace seedbox_read(void)
{
	unsigned long flags;
	u64 value, last;
	/* Serialize reads against IRQ clock reads on the one guest CPU. */
	local_irq_save(flags);
	if (unlikely(!page)) {
		value = ++early_last;
		goto out;
	}
again:
	value = READ_ONCE(page->now);
	last = READ_ONCE(page->last);
	if (value <= last)
		value = last + 1;
	if (unlikely(!value))
		panic("seedbox: clock overflow");
	if (value > READ_ONCE(page->bound) ||
	    (!irqs_disabled_flags(flags) && READ_ONCE(page->deferred_irq_pending))) {
		/* Deliver at the yield when the caller permits IRQs. Restore the
		 * caller's state before the unmasked rendezvous, then serialize
		 * the retry against IRQ clock reads again. Masked callers leave
		 * the level pending; the page word makes the next unmasked read
		 * rendezvous even when no deadline remains armed.
		 */
		if (!irqs_disabled_flags(flags)) {
			local_irq_restore(flags);
			local_irq_enable();
			isb();
			writeq(1, event + 24);
			local_irq_disable();
		} else {
			writeq(1, event + 24);
		}
		goto again;
	}
	WRITE_ONCE(page->last, value);
out:
	local_irq_restore(flags);
	return value;
}

/* The preempt fault's draw: one step of a splitmix64 stream kept in kernel
 * memory, so checkpoints continue it, reseeded whenever the host publishes a
 * new seed word (each boot has its own). Zero rate is off. Interrupts are
 * masked so the one-CPU stream advances once per call.
 */
static u64 preempt_seed, preempt_state;
static bool seedbox_preempt_draw(void)
{
	unsigned long flags;
	u64 ppm, seed, z;
	bool hit;

	if (!page)
		return false;
	local_irq_save(flags);
	ppm = READ_ONCE(page->preempt_ppm);
	seed = READ_ONCE(page->preempt_seed);
	if (!ppm) {
		local_irq_restore(flags);
		return false;
	}
	if (seed != preempt_seed) {
		preempt_seed = seed;
		preempt_state = seed;
	}
	z = (preempt_state += 0x9e3779b97f4a7c15ULL);
	z = (z ^ (z >> 30)) * 0xbf58476d1ce4e5b9ULL;
	z = (z ^ (z >> 27)) * 0x94d049bb133111ebULL;
	z ^= z >> 31;
	hit = z % 1000000 < ppm;
	local_irq_restore(flags);
	return hit;
}

/* Charge work through the existing bounded clock rendezvous. Keeping each
 * read bounded also preserves pending IRQ delivery at an exact deadline. The
 * remaining charge survives a rendezvous in guest registers or stack.
 */
static void seedbox_charge(u64 remaining)
{
	struct seedbox_page *p;

	while (remaining) {
		unsigned long flags;
		u64 last, bound, count;

		/* Retain each segment's first read and unmask point for pending IRQs.
		 * New time or IRQs arrive only at a stopped rendezvous; the remaining
		 * reads before the published bound can then be charged together. */
		(void)seedbox_read();
		if (!--remaining)
			break;
		local_irq_save(flags);
		p = READ_ONCE(page);
		if (p && (irqs_disabled_flags(flags) ||
			  !READ_ONCE(p->deferred_irq_pending))) {
			last = READ_ONCE(p->last);
			bound = READ_ONCE(p->bound);
			if (last < bound) {
				count = bound - last;
				if (count > remaining)
					count = remaining;
				WRITE_ONCE(p->last, last + count);
				remaining -= count;
			}
		}
		local_irq_restore(flags);
	}
}

/* The task whose progress call is charging its translated compute, if any.
 * The clock advances only in the kernel, so every tick lands at EL1; a tick
 * that interrupts this charge samples user work. One guest CPU, and no
 * preemption inside the charge. */
static struct task_struct *user_charge;

/* Account the current tick as user time (kernel/time/timer.c): it interrupted
 * a progress charge directly, not a softirq or a nested interrupt. */
bool seedbox_user_tick(void)
{
	return READ_ONCE(user_charge) == current && irq_count() == HARDIRQ_OFFSET;
}

/* Charge one syscall. Load its price once: throttle/heal changes at a
 * rendezvous affect the next syscall. Some syscalls are not application work
 * at full price. The PV clock disables the vDSO, so time syscalls stand in for
 * vDSO reads and cost only the clock read they make. The translator's own
 * syscalls (thread suspension, signal returns, futex waits) are bookkeeping
 * and cost a tenth of a syscall, still at least one bounded read.
 */
void seedbox_syscall_progress(struct pt_regs *regs, unsigned int scno)
{
	u64 ns;

	if (scno == SEEDBOX_CLOCK_GETTIME_NR || scno == SEEDBOX_CLOCK_GETRES_NR ||
	    scno == SEEDBOX_GETTIMEOFDAY_NR)
		return;
	ns = page ? READ_ONCE(page->syscall_ns) : 1000;
	/* A progress call stands for the application's own compute: ticks during
	 * its charge are user time. The charge itself is unchanged. */
	if (scno == SEEDBOX_PROGRESS_NR && regs->regs[0] == SEEDBOX_PROGRESS_MARK) {
		WRITE_ONCE(user_charge, current);
		seedbox_charge(ns);
		WRITE_ONCE(user_charge, NULL);
	} else {
		if (regs->regs[7] == SEEDBOX_TRANSLATOR_MARK)
			ns = max(ns / SEEDBOX_TRANSLATOR_DIVISOR, 1ULL);
		seedbox_charge(ns);
	}
	/* Syscall entry runs in process context with IRQs enabled, so a seeded
	 * hit can switch to another runnable thread here, after the charge. */
	if (seedbox_preempt_draw())
		yield();
}

static u64 cs_read(struct clocksource *cs) { return seedbox_read(); }
static struct clocksource cs = {
	.name = "seedbox-pv", .rating = 500, .read = cs_read,
	.mask = CLOCKSOURCE_MASK(64), .flags = CLOCK_SOURCE_IS_CONTINUOUS,
	/* Zero-initialized vdso_clock_mode = VDSO_CLOCKMODE_NONE. */
};
static int next_event(unsigned long delta, struct clock_event_device *dev)
{
	u64 now = seedbox_read();
	if (U64_MAX - now < delta)
		panic("seedbox: deadline overflow");
	writeq(now + delta, event);
	return 0;
}
static int shutdown_event(struct clock_event_device *dev)
{
	writeq(0, event);
	writeq(1, event + 8);
	return 0;
}
static int oneshot_event(struct clock_event_device *dev) { return 0; }
static irqreturn_t timer_irq(int irq, void *dev)
{
	/* Deassert before invoking the clock core; it may arm another deadline. */
	writeq(1, event + 8);
	ce.event_handler(&ce);
	return IRQ_HANDLED;
}
void seedbox_idle(void)
{
	unsigned long flags;

	if (unlikely(!event))
		panic("seedbox: idle before PV initialization");
	/*
	 * cpu_do_idle() enters with DAIF.I masked. If the VMM asserts the
	 * timer there, the IRQ can skid after the later DAIFClr instruction;
	 * its interrupted PC then contaminates the stack and entropy pool.
	 * Reach the PV idle exit with IRQs unmasked, before the VMM asserts
	 * the line. No host interrupt is queued while executing this unmask.
	 * Preserve the caller's mask on return, as native cpu_do_idle does.
	 * Both backends use this rendezvous; native KVM WFI is not exposed.
	 */
	local_irq_save(flags);
	dsb(sy);
	local_irq_enable();
	isb();
	writeq(1, event + 16);
	local_irq_restore(flags);
}
void __init seedbox_time_init(void)
{
	struct device_node *node;
	unsigned int irq;
	int err;
	/* Disable EL0 counters, event stream and user timer register access. */
	write_sysreg(0, cntkctl_el1);
	write_sysreg(0, cntv_ctl_el0);
	write_sysreg(0, cntp_ctl_el0);
	isb();
	event = ioremap(EVENT_IPA, SZ_16K);
	page = memremap(TIME_IPA, SZ_16K, MEMREMAP_WB);
	if (!event || !page || READ_ONCE(page->abi) != 3)
		panic("seedbox: PV ABI 3 page unavailable");
	seedbox_exclusive_epoch = &page->exclusive_epoch;
	WRITE_ONCE(page->last, early_last);
	writeq(3, event + 32);
	node = of_find_compatible_node(NULL, NULL, "seedbox,pv-time-v3");
	if (!node)
		panic("seedbox: missing PV FDT node");
	irq = irq_of_parse_and_map(node, 0);
	of_node_put(node);
	if (!irq)
		panic("seedbox: missing PV SPI");
	err = clocksource_register_hz(&cs, 1000000000);
	if (err)
		panic("seedbox: clocksource registration %d", err);
	sched_clock_register(seedbox_read, 64, 1000000000);
	ce.name = "seedbox-pv";
	ce.features = CLOCK_EVT_FEAT_ONESHOT;
	ce.rating = 500;
	ce.set_next_event = next_event;
	ce.set_state_shutdown = shutdown_event;
	ce.set_state_oneshot = oneshot_event;
	ce.cpumask = cpumask_of(0);
	ce.irq = irq;
	err = request_irq(irq, timer_irq, IRQF_TIMER, "seedbox-pv", &ce);
	if (err)
		panic("seedbox: request_irq %d", err);
	clockevents_config_and_register(&ce, 1000000000, 1, 0x7fffffff);
	pr_info("seedbox: PV clock and SPI %u ready\n", irq);
}
