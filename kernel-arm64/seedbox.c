// SPDX-License-Identifier: GPL-2.0
/* Single-CPU PV ABI 1. Matches crates/seedbox-arm64; no architectural timer. */
#include <linux/clockchips.h>
#include <linux/clocksource.h>
#include <linux/init.h>
#include <linux/interrupt.h>
#include <linux/io.h>
#include <linux/irq.h>
#include <linux/of.h>
#include <linux/of_irq.h>
#include <linux/sched_clock.h>
#include <linux/sizes.h>
#include <asm/seedbox.h>
#include <asm/sysreg.h>

#define TIME_IPA 0x0a000000UL
#define EVENT_IPA 0x0a010000UL
struct seedbox_page { u64 now, last, bound, abi; };
static struct seedbox_page *page;
static void __iomem *event;
static u64 early_last;
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
	if (value > READ_ONCE(page->bound)) {
		/* The VMM reconciles last, expires the timer and republishes. */
		writeq(1, event + 24);
		goto again;
	}
	WRITE_ONCE(page->last, value);
out:
	local_irq_restore(flags);
	return value;
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
	if (!event || !page || READ_ONCE(page->abi) != 1)
		panic("seedbox: PV ABI 1 page unavailable");
	WRITE_ONCE(page->last, early_last);
	writeq(1, event + 32);
	node = of_find_compatible_node(NULL, NULL, "seedbox,pv-time-v1");
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
