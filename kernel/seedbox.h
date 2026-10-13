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
/* Nominal virtual time one syscall costs (ns); the unit a progress charge is counted in. */
#define SEEDBOX_SYSCALL_NS	1000ULL
/* Virtual time one guest clock read costs (ns). */
#define SEEDBOX_PV_READ_NS	100
/* A progress call: getpid with this first argument (guest/progress/budget.h). */
#define SEEDBOX_PROGRESS_NR	39
#define SEEDBOX_PROGRESS_MARK	0x5eedb0c5UL
/* rbx of a syscall the translator makes for itself
 * (guest/progress/dynamorio-x86-translator-syscalls.patch). */
#define SEEDBOX_TRANSLATOR_MARK	0x5eedd1a900000000UL
/* Translator syscalls cost this fraction of an application syscall. */
#define SEEDBOX_TRANSLATOR_DIVISOR	10

#ifndef __ASSEMBLY__
#include <linux/init.h>
#include <linux/types.h>
#include <linux/jump_label.h>
#include <asm/current.h>
#include <asm/io.h>
#include <asm/ptrace.h>

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
	 * Guest progress is not counted: virtual time moves only at exits.
	 * The guest exits when its clock would pass `bound` and at the
	 * syscall that makes `syscalls` reach `syscall_target`. The VMM sets
	 * pv_clock to 1 before boot; the kernel answers by setting it to 2.
	 */
	u64 pv_clock;
	u64 bound;
	u64 syscall_target;
	u64 entropy_gen;	/* VMM bumps after writing entropy at a time fork */
	u64 entropy[4];	/* 32 bytes the kernel mixes in before reseeding */
	u64 timer_deadline;	/* guest-written absolute 1 GHz deadline, or DISARMED */
	u64 capabilities;	/* VMM feature bits (ABI 4) */
	u64 syscall_ns;	/* VMM plan: per-syscall cost */
	u64 syscalls0;	/* VMM plan: syscall count used for syscall_target */
	u64 plan_time;	/* VMM plan: t0 at the last deterministic publication */
	u64 progress_ns;	/* nominal charge per progress call; 0 charges one syscall */
	u64 progress_jitter_ns;	/* bound on the seeded variation of that charge, each way; 0 is off */
	u64 progress_jitter_seed;	/* VMM-seeded stream the kernel draws that variation from */
};

struct pt_regs;
struct task_struct;
extern struct seedbox_time_page *seedbox_page;
/* The task whose progress call is at its exit, if any (seedbox_user_tick). */
extern struct task_struct *seedbox_user_charge;
unsigned long long seedbox_rdtsc(void);
void seedbox_cpu_relax(void);
bool seedbox_emulate_user_tsc(struct pt_regs *regs);
unsigned long seedbox_tsc_khz(void);
void __init seedbox_time_init(void);
/* Returns false when the timer port must be used. */
bool seedbox_set_next_event(unsigned long ticks);
void seedbox_timer_disarm(void);
/* Mix in fork entropy and reseed if the VMM published new (drivers/char/random.c). */
void seedbox_take_entropy(void);
/* The syscalls a progress call counts as: its charge in whole syscall costs. */
u64 seedbox_progress_syscalls(struct seedbox_time_page *p);
/* The syscalls a translator syscall counts as: one in every SEEDBOX_TRANSLATOR_DIVISOR. */
u64 seedbox_translator_syscalls(void);
/* The current tick interrupted a progress call's exit: user time. */
bool seedbox_user_tick(void);

/*
 * Offer the VMM a deterministic stopping point. The executed conditional
 * branches are the same whether or not the exit happens. A progress call
 * counts as the syscalls its compute charge comes to, not one.
 *
 * Time moves only at exits, so timer interrupts arrive in the kernel, right
 * after a yield. One that arrives at a progress call's yield samples the
 * user compute that call charged: mark it as user time for tick accounting.
 * The yield has no memory clobber; the barriers keep it inside the mark.
 */
static __always_inline void seedbox_syscall_hook(struct pt_regs *regs, int nr)
{
	struct seedbox_time_page *p = READ_ONCE(seedbox_page);

	if (p) {
		u64 n = p->syscalls;
		struct task_struct *user = NULL;

		if (nr == SEEDBOX_PROGRESS_NR && regs->di == SEEDBOX_PROGRESS_MARK) {
			n += seedbox_progress_syscalls(p);
			user = current;
		}
		/* The translator's own syscalls (thread suspension, signal
		 * returns, futex waits) are bookkeeping, not application work. */
		else if (regs->bx == SEEDBOX_TRANSLATOR_MARK)
			n += seedbox_translator_syscalls();
		else
			n += 1;
		WRITE_ONCE(p->syscalls, n);
		WRITE_ONCE(seedbox_user_charge, user);
		barrier();
		if (READ_ONCE(p->exit_req))
			outl(0, SEEDBOX_PORT_YIELD);
		else if (n >= READ_ONCE(p->syscall_target))
			outl(0, SEEDBOX_PORT_YIELD);
		barrier();
		WRITE_ONCE(seedbox_user_charge, NULL);
	}
}
#endif

#endif
