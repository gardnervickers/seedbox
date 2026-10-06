/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _ASM_X86_SEEDBOX_H
#define _ASM_X86_SEEDBOX_H

/* ABI shared with the Seedbox VMM. */
#define SEEDBOX_PORT_TIME_LO	0x520	/* read: latch virtual TSC, low 32 bits */
#define SEEDBOX_PORT_TIME_HI	0x524	/* read: high 32 bits of latched value */
#define SEEDBOX_PORT_TIME_READY	0x528	/* write: shared page now in use */
#define SEEDBOX_PORT_YIELD	0x52c	/* write: requested exit at a syscall */
#define SEEDBOX_TIME_PAGE_PHYS	0xd0000000UL

#ifndef __ASSEMBLY__
#include <linux/types.h>
#include <asm/io.h>

struct seedbox_time_page {
	u64 tsc;	/* virtual TSC published by the VMM (1 GHz) */
	u64 last;	/* last value returned to the guest */
	u64 exit_req;	/* VMM asks for an exit at the next syscall */
};

struct pt_regs;
extern struct seedbox_time_page *seedbox_page;
unsigned long long seedbox_rdtsc(void);
bool seedbox_emulate_user_tsc(struct pt_regs *regs);

/*
 * Offer the VMM a deterministic stopping point. The executed conditional
 * branches are the same whether or not the exit happens.
 */
static __always_inline void seedbox_syscall_hook(void)
{
	struct seedbox_time_page *p = READ_ONCE(seedbox_page);

	if (p && READ_ONCE(p->exit_req))
		outl(0, SEEDBOX_PORT_YIELD);
}
#endif

#endif
