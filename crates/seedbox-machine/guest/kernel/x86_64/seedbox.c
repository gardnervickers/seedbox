// SPDX-License-Identifier: GPL-2.0
/*
 * Single-CPU PV ABI 3 for x86-64. Matches the seedbox-machine stopped worker:
 * the shared time page and deadline registers, byte OUT for ACK/YIELD, and
 * the 8259 PIC as the only interrupt controller. No LAPIC, HPET, PIT or RTC is
 * used. Kernel TSC reads return PV time; application RDTSC/RDTSCP trap through
 * CR4.TSD and are emulated here.
 */
#include <linux/build_bug.h>
#include <linux/clockchips.h>
#include <linux/clocksource.h>
#include <linux/init.h>
#include <linux/interrupt.h>
#include <linux/ioport.h>
#include <linux/kernel.h>
#include <linux/platform_device.h>
#include <linux/pm.h>
#include <linux/preempt.h>
#include <linux/sched.h>
#include <linux/stddef.h>
#include <asm/cpufeature.h>
#include <asm/fixmap.h>
#include <asm/insn-eval.h>
#include <asm/irqflags.h>
#include <asm/seedbox.h>
#include <asm/time.h>
#include <asm/tlbflush.h>
#include <asm/x86_init.h>

struct seedbox_page {
	u64 now, last, bound, abi, deferred_irq_pending, exclusive_epoch;
	u64 syscall_ns;
	u64 pic_update;
	u64 virtio_irq[3];
};

enum {
	EVENT_DEADLINE = 0,
	EVENT_READY = 32,
	EVENT_POWER_OFF = 40,
};

static struct seedbox_page *page;
static volatile u64 *event;
static u64 early_last;
static struct clock_event_device ce;

static __always_inline void event_write(unsigned int offset, u64 value)
{
	/* Uncached fixmap: every store is a stopped worker rendezvous. */
	WRITE_ONCE(event[offset / 8], value);
}

static __always_inline void event_signal(u16 port)
{
	/* Order time-page accesses around the stopped worker rendezvous. */
	asm volatile("outb %b0, %w1" : : "a"((u8)1), "Nd"(port) : "memory");
}

/* IRQs are disabled by i8259A_lock on this single CPU. One aligned store
 * publishes the complete mask and accumulated specific EOIs. The stopped
 * worker consumes and clears this word before admitting input or injecting.
 * No device interrupt status is acknowledged here. */
void seedbox_pic_update(u8 mask, u8 ack)
{
	struct seedbox_page *p = READ_ONCE(page);
	u64 pending = READ_ONCE(p->pic_update);

	WRITE_ONCE(p->pic_update, (1ULL << 16) | mask |
		   (pending & 0xff00) | ((u64)ack << 8));
}

void seedbox_pic_unmask(u8 mask)
{
	seedbox_pic_update(mask, 0);
	/* Always rendezvous before the caller can enable CPU interrupts. */
	event_signal(SEEDBOX_PIC_UNMASK_PORT);
}

/* The fixed console/disk/NIC MMIO windows have corresponding page words.
 * Called only from the IRQ-disabled virtio handler. READ/WRITE_ONCE and the
 * compiler barrier publish ACK before queue callbacks (which can rendezvous).
 * The worker consumes it before new input/notify, and republishes device status
 * before entry. Host-only interruptions do neither. The PIC unmask OUT remains
 * unconditional, so no ACK can wait beyond the handler's return. */
u32 seedbox_virtio_interrupt(unsigned long phys)
{
	unsigned int slot = (phys - SEEDBOX_LIFECYCLE_PHYS) / 0x10000;
	u64 *word = &READ_ONCE(page)->virtio_irq[slot];
	u64 value = READ_ONCE(*word);
	u32 status = (u32)value & ~(u32)(value >> 32);

	WRITE_ONCE(*word, value | ((u64)status << 32));
	barrier();
	return status;
}

u64 noinstr seedbox_read(void)
{
	struct seedbox_page *p;
	unsigned long flags;
	u64 value, last;

	/* Serialize reads against IRQ clock reads on the one guest CPU. */
	flags = arch_local_irq_save();
	p = READ_ONCE(page);
	if (unlikely(!p)) {
		value = ++early_last;
		goto out;
	}
again:
	value = READ_ONCE(p->now);
	last = READ_ONCE(p->last);
	if (value <= last)
		value = last + 1;
	if (unlikely(!value))
		__builtin_trap();
	if (value > READ_ONCE(p->bound) ||
	    (!arch_irqs_disabled_flags(flags) &&
	     READ_ONCE(p->deferred_irq_pending))) {
		/*
		 * Yield to the stopped worker at the published bound. A caller
		 * that permits IRQs takes a due interrupt right at this
		 * rendezvous; masked callers keep it pending until they unmask,
		 * where the worker's interrupt-window exit delivers it exactly.
		 */
		if (!arch_irqs_disabled_flags(flags)) {
			arch_local_irq_restore(flags);
			event_signal(SEEDBOX_YIELD_PORT);
			arch_local_irq_disable();
		} else {
			event_signal(SEEDBOX_YIELD_PORT);
		}
		goto again;
	}
	WRITE_ONCE(p->last, value);
out:
	arch_local_irq_restore(flags);
	return value;
}

unsigned long long noinstr seedbox_rdtsc(void)
{
	return seedbox_read();
}

/*
 * cpu_relax/rep_nop can run with locks held and interrupts disabled. Reuse the
 * bounded clock read: no allocation, scheduling, locking or interrupt enabling.
 * Before the page is mapped this is a no-op.
 */
void noinstr seedbox_cpu_relax(void)
{
	if (READ_ONCE(page))
		(void)seedbox_read();
}

/* A progress call: getpid with this first argument (guest/progress/progress.c). */
#define SEEDBOX_PROGRESS_NR 39
#define SEEDBOX_PROGRESS_MARK 0x5eedb0c5UL
/* Time syscalls (gettimeofday, time, clock_gettime, clock_getres). */
#define SEEDBOX_GETTIMEOFDAY_NR 96
#define SEEDBOX_TIME_NR 201
#define SEEDBOX_CLOCK_GETTIME_NR 228
#define SEEDBOX_CLOCK_GETRES_NR 229

/* Charge work through the bounded clock rendezvous. The remaining charge
 * survives a rendezvous in guest registers or stack. */
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
		flags = arch_local_irq_save();
		p = READ_ONCE(page);
		if (p && (arch_irqs_disabled_flags(flags) ||
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
		arch_local_irq_restore(flags);
	}
}

/* The task whose progress call is charging its translated compute, if any.
 * Time moves only at rendezvous in the kernel, so every tick lands there; a
 * tick that interrupts this charge samples user work. One guest CPU, and no
 * preemption inside the charge. */
static struct task_struct *user_charge;

/* Account the current tick as user time (kernel/time/timer.c): it interrupted
 * a progress charge directly, not a softirq or a nested interrupt. */
bool seedbox_user_tick(void)
{
	return READ_ONCE(user_charge) == current && irq_count() == HARDIRQ_OFFSET;
}

/* Charge one syscall. Load its price once; a price change at a rendezvous
 * affects the next syscall only. A progress call stands for the application's
 * own compute: ticks during its charge are user time, at the same charge.
 * Translator syscalls carry no mark on x86 and cost a whole syscall. */
void seedbox_syscall_progress(struct pt_regs *regs, int nr)
{
	struct seedbox_page *p = READ_ONCE(page);
	u64 ns = p ? READ_ONCE(p->syscall_ns) : 1000;

	/* The PV clocksource leaves the vDSO without a clock, so time syscalls
	 * stand in for vDSO reads and cost only the clock read they make. */
	if (nr == SEEDBOX_GETTIMEOFDAY_NR || nr == SEEDBOX_TIME_NR ||
	    nr == SEEDBOX_CLOCK_GETTIME_NR || nr == SEEDBOX_CLOCK_GETRES_NR)
		return;
	if (nr == SEEDBOX_PROGRESS_NR && regs->di == SEEDBOX_PROGRESS_MARK) {
		WRITE_ONCE(user_charge, current);
		seedbox_charge(ns);
		WRITE_ONCE(user_charge, NULL);
		return;
	}
	seedbox_charge(ns);
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
	v = seedbox_read();
	regs->ax = (u32)v;
	regs->dx = (u32)(v >> 32);
	if (tscp)
		regs->cx = 0;
	regs->ip += len;
	return true;
}

/* The virtual TSC runs at exactly 1 GHz: no calibration against hardware. */
unsigned long seedbox_tsc_khz(void)
{
	setup_force_cpu_cap(X86_FEATURE_TSC_KNOWN_FREQ);
	setup_force_cpu_cap(X86_FEATURE_TSC_RELIABLE);
	return SEEDBOX_TSC_KHZ;
}

static void seedbox_power_off(void)
{
	event_write(EVENT_POWER_OFF, 1);
	for (;;)
		native_halt();
}

static void seedbox_get_wallclock(struct timespec64 *now)
{
	/* Like the arm64 PV machine: no RTC, so wall time starts at the epoch. */
	now->tv_sec = 0;
	now->tv_nsec = 0;
}

static int seedbox_set_wallclock(const struct timespec64 *now)
{
	return -ENODEV;
}

void __init seedbox_time_init(void)
{
	struct seedbox_page *p;

	BUILD_BUG_ON(offsetof(struct seedbox_page, syscall_ns) != 6 * sizeof(u64));
	BUILD_BUG_ON(offsetof(struct seedbox_page, pic_update) != 7 * sizeof(u64));
	BUILD_BUG_ON(offsetof(struct seedbox_page, virtio_irq) != 8 * sizeof(u64));
	/*
	 * setup_arch calls us before tsc_early_init selects TSC delays. Permanent
	 * fixmaps need no allocator and survive the boot page-table switch.
	 */
	set_fixmap(FIX_SEEDBOX_TIME, SEEDBOX_TIME_PHYS);
	set_fixmap_nocache(FIX_SEEDBOX_EVENT, SEEDBOX_EVENT_PHYS);
	p = (struct seedbox_page *)fix_to_virt(FIX_SEEDBOX_TIME);
	event = (volatile u64 *)fix_to_virt(FIX_SEEDBOX_EVENT);
	if (READ_ONCE(p->abi) != SEEDBOX_ABI)
		panic("seedbox: PV ABI 3 page unavailable");
	WRITE_ONCE(p->last, early_last);
	WRITE_ONCE(page, p);
	event_write(EVENT_READY, SEEDBOX_ABI);
	x86_platform.get_wallclock = seedbox_get_wallclock;
	x86_platform.set_wallclock = seedbox_set_wallclock;
	pm_power_off = seedbox_power_off;
	pr_info("seedbox: PV ABI 3 time page active\n");
}

static u64 cs_read(struct clocksource *cs)
{
	return seedbox_read();
}

static struct clocksource cs = {
	.name = "seedbox-pv",
	.rating = 500,
	.read = cs_read,
	.mask = CLOCKSOURCE_MASK(64),
	.flags = CLOCK_SOURCE_IS_CONTINUOUS,
	/* vdso_clock_mode stays NONE: vDSO clock calls use syscalls. */
};

static int next_event(unsigned long delta, struct clock_event_device *dev)
{
	u64 now = seedbox_read();

	if (U64_MAX - now < delta)
		panic("seedbox: deadline overflow");
	event_write(EVENT_DEADLINE, now + delta);
	return 0;
}

static int shutdown_event(struct clock_event_device *dev)
{
	event_write(EVENT_DEADLINE, 0);
	event_signal(SEEDBOX_ACK_PORT);
	return 0;
}

static int oneshot_event(struct clock_event_device *dev)
{
	return 0;
}

static irqreturn_t timer_irq(int irq, void *dev)
{
	/* Deassert before invoking the clock core; it may arm another deadline. */
	event_signal(SEEDBOX_ACK_PORT);
	ce.event_handler(&ce);
	return IRQ_HANDLED;
}

/* Replaces hpet_time_init: PV clocksource and oneshot clockevent on IRQ 0. */
void __init seedbox_timer_init(void)
{
	int err;

	if (!READ_ONCE(page))
		panic("seedbox: timer before PV initialization");
	err = clocksource_register_hz(&cs, NSEC_PER_SEC);
	if (err)
		panic("seedbox: clocksource registration %d", err);
	ce.name = "seedbox-pv";
	ce.features = CLOCK_EVT_FEAT_ONESHOT;
	ce.rating = 500;
	ce.set_next_event = next_event;
	ce.set_state_shutdown = shutdown_event;
	ce.set_state_oneshot = oneshot_event;
	ce.cpumask = cpumask_of(0);
	ce.irq = 0;
	err = request_irq(0, timer_irq, IRQF_TIMER | IRQF_NOBALANCING,
			  "seedbox-pv", &ce);
	if (err)
		panic("seedbox: request_irq %d", err);
	global_clock_event = &ce;
	clockevents_config_and_register(&ce, NSEC_PER_SEC, 1, 0x7fffffff);
	pr_info("seedbox: PV clock and IRQ 0 ready\n");
}

/* There is no device tree on x86: describe the fixed lifecycle port here. */
static int __init seedbox_devices_init(void)
{
	struct resource resources[] = {
		DEFINE_RES_MEM(SEEDBOX_LIFECYCLE_PHYS, SEEDBOX_LIFECYCLE_BYTES),
		DEFINE_RES_IRQ(SEEDBOX_LIFECYCLE_IRQ),
	};
	struct platform_device *device;

	device = platform_device_register_simple("virtio-mmio", 0, resources,
						 ARRAY_SIZE(resources));
	return PTR_ERR_OR_ZERO(device);
}
arch_initcall(seedbox_devices_init);

static int __init seedbox_user_tsc_init(void)
{
	/* cpu_init clears TSD after setup_arch; enable user traps afterwards. */
	cr4_set_bits(X86_CR4_TSD);
	return 0;
}
early_initcall(seedbox_user_tsc_init);
