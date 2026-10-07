/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _ASM_X86_SEEDBOX_CLOCK_H
#define _ASM_X86_SEEDBOX_CLOCK_H

/*
 * The deterministic clock fields of the Seedbox time page. Shared by the
 * kernel and the vDSO, which maps the page writable into every process so
 * user clock reads need no RDTSC trap.
 */
#include <linux/types.h>
#include <linux/compiler.h>

struct seedbox_clock {
	u64 tsc;	/* virtual TSC published by the VMM (1 GHz) */
	u64 last;	/* last value returned to the guest */
};

/*
 * Return max(published, last + 1) and record it. Reads with no new
 * publication advance by one tick, so polling loops make progress.
 *
 * The guest has one vCPU and the VMM touches the page only while it is
 * stopped, so a cmpxchg without LOCK is atomic here: an interrupt between
 * the loads and the store makes it retry rather than move `last` backwards.
 * Interrupts arrive at deterministic points, so retries are deterministic.
 */
static __always_inline u64 seedbox_clock_read(struct seedbox_clock *c)
{
	u64 last = READ_ONCE(c->last);
	u64 v;
	bool ok;

	do {
		v = READ_ONCE(c->tsc);
		if (v <= last)
			v = last + 1;
		asm volatile("cmpxchgq %[v], %[p]\n\tsete %[ok]"
			     : [ok] "=q" (ok), [p] "+m" (c->last), "+a" (last)
			     : [v] "r" (v)
			     : "memory", "cc");
	} while (!ok);
	return v;
}

#endif
