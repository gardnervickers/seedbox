// SPDX-License-Identifier: GPL-2.0
/* Single-CPU PV ABI 3. Matches crates/seedbox-arm64; no architectural timer. */
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
struct seedbox_page {
	u64 now, last, bound, abi, deferred_irq_pending, exclusive_epoch;
	u64 syscall_ns;
	/* The nominal charge per progress call (0 charges one syscall), the
	 * bound on its seeded variation each way (0 is off), and the
	 * VMM-seeded stream that variation is drawn from. */
	u64 progress_ns, progress_jitter_ns, progress_jitter_seed;
};
/* Nominal virtual time one syscall costs; the page's syscall_ns is 1000 unthrottled. */
#define SYSCALL_NS 1000ULL
/* A progress call: getpid with this first argument (guest/progress/budget.h). */
#define SEEDBOX_PROGRESS_NR 172
#define SEEDBOX_PROGRESS_MARK 0x5eedb0c5UL
/* Time syscalls (clock_gettime, clock_getres, gettimeofday). */
#define SEEDBOX_CLOCK_GETTIME_NR 113
#define SEEDBOX_CLOCK_GETRES_NR 114
#define SEEDBOX_GETTIMEOFDAY_NR 169
/* x7 of a syscall the translator makes for itself
 * (the translator-syscalls commit in the DynamoRIO fork, nix/dynamorio.nix). */
#define SEEDBOX_TRANSLATOR_MARK 0x5eedd1a900000000UL
/* Translator syscalls cost this fraction of an application syscall. */
#define SEEDBOX_TRANSLATOR_DIVISOR 10
static struct seedbox_page *page;
static void __iomem *event;
static u64 early_last;
/* Shared with the stopped VMM, which invalidates reservations on guest exits
 * and device writes. Initialized before the first return to userspace. */
u64 *seedbox_exclusive_epoch;
static struct clock_event_device ce;

static u64 notrace seedbox_read_step(u64 step)
{
	unsigned long flags;
	u64 value, last;
	/* Serialize reads against IRQ clock reads on the one guest CPU. */
	local_irq_save(flags);
	if (unlikely(!page)) {
		value = early_last += step;
		goto out;
	}
again:
	value = READ_ONCE(page->now);
	last = READ_ONCE(page->last);
	if (unlikely(last > ~0ULL - step))
		panic("seedbox: clock overflow");
	if (value < last + step)
		value = last + step;
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

/* Public clock reads and kernel polling use the same 100 ns policy as x86. */
u64 notrace seedbox_read(void)
{
	return seedbox_read_step(100);
}

/* Safe with locks held and IRQs masked. Early boot has no PV page yet. */
void notrace seedbox_cpu_relax(void)
{
	if (page)
		(void)seedbox_read();
}

/* splitmix64 on the page-seeded state. */
static u64 jitter_next(u64 *state)
{
	u64 z = (*state += 0x9e3779b97f4a7c15ULL);

	z = (z ^ (z >> 30)) * 0xbf58476d1ce4e5b9ULL;
	z = (z ^ (z >> 27)) * 0x94d049bb133111ebULL;
	return z ^ (z >> 31);
}

/* The compute a progress call stands for: the nominal charge the VMM
 * publishes (the instructions between calls at the configured CPU speed) plus
 * a seeded draw within the jitter bound either way, so equal work takes a
 * little longer on some seeds than on others. Scaled like a syscall by the
 * page's per-syscall cost, so a throttle slows compute too. The VMM seeds the
 * stream per machine at boot, at a reboot and at a time fork; the state lives
 * in guest RAM, so a snapshot continues its own stream. One guest CPU,
 * syscall context only. */
static u64 progress_charge(u64 syscall_ns)
{
	static u64 state, seeded;
	u64 base = READ_ONCE(page->progress_ns) ?: SYSCALL_NS;
	u64 bound = READ_ONCE(page->progress_jitter_ns);
	u64 charge = base;

	if (bound > base)
		bound = base;
	if (bound) {
		u64 seed = READ_ONCE(page->progress_jitter_seed);

		if (seed != seeded) {
			state = seed;
			seeded = seed;
		}
		charge += jitter_next(&state) % (2 * bound + 1) - bound;
	}
	return charge * syscall_ns / SYSCALL_NS;
}

/* Charge `ns` of CPU time: advance the clock as that many reads would, in as
 * few steps as the bound allows. At the bound, or with a deferred IRQ to
 * deliver, one read does the rendezvous with the same rules as every clock
 * read, so pending IRQ delivery at an exact deadline is preserved. Before the
 * page is mapped the early clock advances as reads would. */
static void seedbox_charge(u64 ns)
{
	unsigned long flags;
	u64 now, last, bound, step;

	while (ns) {
		local_irq_save(flags);
		if (likely(page)) {
			now = READ_ONCE(page->now);
			last = READ_ONCE(page->last);
			if (now < last)
				now = last;
			bound = READ_ONCE(page->bound);
			step = now < bound ? bound - now : 0;
			if (step > ns)
				step = ns;
			if (step && (irqs_disabled_flags(flags) ||
				     !READ_ONCE(page->deferred_irq_pending))) {
				WRITE_ONCE(page->last, now + step);
				ns -= step;
				local_irq_restore(flags);
				continue;
			}
		}
		local_irq_restore(flags);
		(void)seedbox_read_step(1);
		--ns;
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

/* Charge syscall work through the bounded clock rendezvous. Load the charge
 * once: throttle/heal changes at a rendezvous affect the next syscall, while
 * the remaining charge survives in guest registers or stack. A progress call
 * charges the compute it stands for instead of one syscall, as user time.
 */
void seedbox_syscall_progress(struct pt_regs *regs, int scno)
{
	u64 ns;

	/* Time syscalls stand in for the vDSO, which the PV clock disables. They
	 * cost only the clock read they make, so frequent application time reads
	 * do not each advance the clock by a syscall. */
	if (scno == SEEDBOX_CLOCK_GETTIME_NR || scno == SEEDBOX_CLOCK_GETRES_NR ||
	    scno == SEEDBOX_GETTIMEOFDAY_NR)
		return;
	ns = page ? READ_ONCE(page->syscall_ns) : SYSCALL_NS;

	if (page && scno == SEEDBOX_PROGRESS_NR && regs->regs[0] == SEEDBOX_PROGRESS_MARK) {
		ns = progress_charge(ns);
		WRITE_ONCE(user_charge, current);
		seedbox_charge(ns);
		WRITE_ONCE(user_charge, NULL);
		return;
	}
	/* The translator's own syscalls (thread suspension, signal returns,
	 * futex waits) are bookkeeping, not application work. */
	if (regs->regs[7] == SEEDBOX_TRANSLATOR_MARK)
		ns /= SEEDBOX_TRANSLATOR_DIVISOR;
	seedbox_charge(ns);
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
