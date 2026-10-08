/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _ASM_X86_SEEDBOX_H
#define _ASM_X86_SEEDBOX_H

/* ABI shared with the Seedbox VMM. */
#define SEEDBOX_PORT_TIME_LO	0x520	/* read: latch virtual TSC, low 32 bits */
#define SEEDBOX_PORT_TIME_HI	0x524	/* read: high 32 bits of latched value */
#define SEEDBOX_PORT_TIME_READY	0x528	/* write: shared page in use (value: ABI version) */
#define SEEDBOX_PORT_YIELD	0x52c	/* write: requested exit (0: syscall, 1: clock bound) */
#define SEEDBOX_PORT_TIMER	0x530	/* write: arm PIT channel 0 for N ticks */
#define SEEDBOX_TIME_PAGE_PHYS	0xd0000000UL
#define SEEDBOX_ABI		4
#define SEEDBOX_CAP_PAGE_TIMER	1ULL
#define SEEDBOX_TIMER_DISARMED	(~0ULL)
#define SEEDBOX_TSC_KHZ		1000000
/* PV clock: virtual time one guest clock read costs (ns; 1 in the counter timebase). */
#define SEEDBOX_PV_READ_NS	100

#ifndef __ASSEMBLY__
#include <linux/init.h>
#include <linux/types.h>
#include <linux/jump_label.h>
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
	/*
	 * PV clock timebase only (pv_clock set by the VMM before boot). Guest
	 * progress is not counted: virtual time moves only at exits. The
	 * guest exits when its clock would pass `bound` and at the syscall
	 * that makes `syscalls` reach `syscall_target`. The kernel answers a
	 * nonzero pv_clock by setting it to 2.
	 */
	u64 pv_clock;
	u64 bound;
	u64 syscall_target;
	u64 entropy_gen;	/* VMM bumps after writing entropy at a time fork */
	u64 entropy[4];	/* 32 bytes the kernel mixes in before reseeding */
	u64 timer_deadline;	/* guest-written absolute 1 GHz deadline, or DISARMED */
	u64 capabilities;	/* VMM feature bits (ABI 4) */
	u64 syscall_ns;	/* VMM plan: per-syscall cost, zero in counter mode */
	u64 syscalls0;	/* VMM plan: syscall count used for syscall_target */
	u64 plan_time;	/* VMM plan: t0 (tsc can lag after a watchdog split) */
};

/* Enabled at boot in the PV clock timebase; a no-op (no branch) otherwise. */
DECLARE_STATIC_KEY_FALSE(seedbox_pv_clock);

struct pt_regs;
extern struct seedbox_time_page *seedbox_page;
unsigned long long seedbox_rdtsc(void);
bool seedbox_emulate_user_tsc(struct pt_regs *regs);
unsigned long seedbox_tsc_khz(void);
void __init seedbox_time_init(void);
/* Returns false when the timer port must be used. */
bool seedbox_set_next_event(unsigned long ticks);
void seedbox_timer_disarm(void);
/* Mix in fork entropy and reseed if the VMM published new (drivers/char/random.c). */
void seedbox_take_entropy(void);

/*
 * Offer the VMM a deterministic stopping point. The executed conditional
 * branches are the same whether or not the exit happens.
 */
static __always_inline void seedbox_syscall_hook(void)
{
	struct seedbox_time_page *p = READ_ONCE(seedbox_page);

	if (p) {
		u64 n = p->syscalls + 1;

		WRITE_ONCE(p->syscalls, n);
		if (READ_ONCE(p->exit_req))
			outl(0, SEEDBOX_PORT_YIELD);
		else if (static_branch_unlikely(&seedbox_pv_clock) &&
			 n >= READ_ONCE(p->syscall_target))
			outl(0, SEEDBOX_PORT_YIELD);
	}
}
#endif

#endif
