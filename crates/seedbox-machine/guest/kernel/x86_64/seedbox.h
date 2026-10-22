/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _ASM_X86_SEEDBOX_H
#define _ASM_X86_SEEDBOX_H
/* Single-CPU PV ABI 3, shared with the seedbox-machine stopped x86 worker. */
#define SEEDBOX_TIME_PHYS	0x0a000000UL
#define SEEDBOX_EVENT_PHYS	0x0a010000UL
#define SEEDBOX_ACK_PORT	0x500
#define SEEDBOX_YIELD_PORT	0x501
#define SEEDBOX_PIC_UNMASK_PORT	0x502
#define SEEDBOX_LIFECYCLE_PHYS	0x0b010000UL
#define SEEDBOX_LIFECYCLE_BYTES	0x4000UL
#define SEEDBOX_LIFECYCLE_IRQ	5
#define SEEDBOX_ABI		3
#define SEEDBOX_TSC_KHZ		1000000UL

#ifndef __ASSEMBLY__
#include <linux/compiler.h>
#include <linux/init.h>
#include <linux/types.h>

struct pt_regs;
u64 seedbox_read(void);
unsigned long long seedbox_rdtsc(void);
void seedbox_cpu_relax(void);
void seedbox_pic_update(u8 mask, u8 ack);
void seedbox_pic_unmask(u8 mask);
void seedbox_syscall_progress(void);
bool seedbox_emulate_user_tsc(struct pt_regs *regs);
unsigned long seedbox_tsc_khz(void);
void __init seedbox_time_init(void);
void __init seedbox_timer_init(void);
#endif
#endif
