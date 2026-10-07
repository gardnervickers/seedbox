/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _ASM_X86_SEEDBOX_H
#define _ASM_X86_SEEDBOX_H

/* ABI shared with the Seedbox VMM. */
#define SEEDBOX_PORT_TIME_LO	0x520	/* read: latch virtual TSC, low 32 bits */
#define SEEDBOX_PORT_TIME_HI	0x524	/* read: high 32 bits of latched value */
#define SEEDBOX_PORT_TIME_READY	0x528	/* write: shared page in use (value: ABI version) */
#define SEEDBOX_PORT_YIELD	0x52c	/* write: requested exit at a syscall */
#define SEEDBOX_PORT_TIMER	0x530	/* write: arm PIT channel 0 for N ticks */
#define SEEDBOX_TIME_PAGE_PHYS	0xd0000000UL
#define SEEDBOX_ABI		3
#define SEEDBOX_TSC_KHZ		1000000

#ifndef __ASSEMBLY__
#include <linux/types.h>
#include <asm/io.h>

/*
 * The VMM reads guest-written fields only while the guest is stopped at a
 * deterministic point, so plain stores need no exit.
 */
struct seedbox_time_page {
	u64 tsc;	/* virtual TSC published by the VMM (1 GHz) */
	u64 last;	/* last value returned to the guest */
	u64 exit_req;	/* VMM asks for an exit at the next syscall */
	u64 syscalls;	/* syscalls entered */
	u64 pic_imr;	/* 8259 masks, master | slave << 8 */
	u64 net_isr;	/* virtio-mmio interrupt status: VMM sets, guest clears */
	u64 entropy_gen;	/* VMM bumps after writing entropy at a time fork */
	u64 entropy[4];	/* 32 bytes the kernel mixes in before reseeding */
};

struct pt_regs;
extern struct seedbox_time_page *seedbox_page;
unsigned long long seedbox_rdtsc(void);
bool seedbox_emulate_user_tsc(struct pt_regs *regs);
unsigned long seedbox_tsc_khz(void);

/*
 * Offer the VMM a deterministic stopping point. The executed conditional
 * branches are the same whether or not the exit happens.
 */
static __always_inline void seedbox_syscall_hook(void)
{
	struct seedbox_time_page *p = READ_ONCE(seedbox_page);

	if (p) {
		WRITE_ONCE(p->syscalls, p->syscalls + 1);
		if (READ_ONCE(p->exit_req))
			outl(0, SEEDBOX_PORT_YIELD);
	}
}
#endif

#endif
