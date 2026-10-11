/* SPDX-License-Identifier: GPL-2.0 */
/* Included only in the fixture kernel, in seedbox.c's translation unit. */
#include <asm/processor.h>
static int __init seedbox_poll_test(void)
{
	unsigned long flags;
	u64 before, elapsed, epoch;
	unsigned int i;

	local_irq_save(flags);
	before = READ_ONCE(page->last);
	epoch = READ_ONCE(page->exclusive_epoch);
	/* Negative control: host YIELD hints alone must not advance the PV page,
	 * even if the host interrupts execution or clears physical reservations. */
	for (i = 0; i < 200000; ++i)
		asm volatile("yield" ::: "memory");
	if (READ_ONCE(page->last) != before)
		panic("seedbox: raw polling advanced virtual time");
	if (READ_ONCE(page->exclusive_epoch) != epoch)
		panic("seedbox: raw polling changed the exclusive epoch");
	/* Exercise the patched kernel helper, with no explicit clock reads in
	 * the loop. Twenty milliseconds crosses the kernel's 10 ms tick. */
	for (i = 0; i < 200000; ++i)
		cpu_relax();
	elapsed = READ_ONCE(page->last) - before;
	if (elapsed < 20000000 || !READ_ONCE(page->deferred_irq_pending))
		panic("seedbox: masked kernel polling made no bounded progress");
	if (!irqs_disabled())
		panic("seedbox: masked kernel polling enabled IRQs");
	local_irq_restore(flags);
	/* An unmasked rendezvous must deliver the retained tick. */
	(void)seedbox_read();
	if (READ_ONCE(page->deferred_irq_pending))
		panic("seedbox: kernel polling tick was not delivered");
	/* Keep the fixture marker visible with the default loglevel=4. */
	pr_err("seedbox: kernel polling PASS (masked progress, deferred tick, negative control)\n");
	return 0;
}
late_initcall(seedbox_poll_test);
