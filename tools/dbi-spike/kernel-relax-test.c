/* Experiment-only: append to a disposable copy of kernel/seedbox.c.
 * Build again with -DSEEDBOX_RELAX_RAW to exercise the unhooked PAUSE control.
 * Never included by the production kernel build.
 */
#include <linux/hrtimer.h>
#include <linux/spinlock.h>

static DEFINE_SPINLOCK(relax_test_lock);
static bool relax_test_done;
static volatile unsigned long relax_test_spins;

static enum hrtimer_restart relax_test_expired(struct hrtimer *timer)
{
	WRITE_ONCE(relax_test_done, true);
	return HRTIMER_NORESTART;
}

static int __init relax_progress_test(void)
{
	struct hrtimer timer;
	unsigned long flags;
	unsigned long long before, after;
	unsigned int i;

	if (!static_branch_unlikely(&seedbox_pv_clock))
		return 0;
	pr_emerg("RELAX_TEST waiting with lock held\n");
	hrtimer_init(&timer, CLOCK_MONOTONIC, HRTIMER_MODE_REL_HARD);
	timer.function = relax_test_expired;
	hrtimer_start(&timer, ns_to_ktime(1000000), HRTIMER_MODE_REL_HARD);
	spin_lock(&relax_test_lock);
	while (!READ_ONCE(relax_test_done)) {
		relax_test_spins++;
#ifdef SEEDBOX_RELAX_RAW
		asm volatile("rep; nop" ::: "memory");
#else
		cpu_relax();
#endif
	}
	spin_unlock(&relax_test_lock);
	hrtimer_cancel(&timer);

	/* A progress hook must preserve IF while holding an irqsave lock. */
	spin_lock_irqsave(&relax_test_lock, flags);
	before = seedbox_rdtsc();
	for (i = 0; i < 1000; ++i)
		cpu_relax();
	after = seedbox_rdtsc();
	if (!irqs_disabled() || after - before < 100000)
		panic("RELAX_TEST IRQ state or progress failed");
	spin_unlock_irqrestore(&relax_test_lock, flags);
	pr_emerg("RELAX_TEST pass spins=%lu irq_off_ns=%llu\n",
		 relax_test_spins, after - before);
	return 0;
}
late_initcall(relax_progress_test);
