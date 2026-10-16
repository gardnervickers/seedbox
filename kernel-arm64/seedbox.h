/* SPDX-License-Identifier: GPL-2.0 */
#ifndef __ASM_SEEDBOX_H
#define __ASM_SEEDBOX_H
#include <linux/types.h>
#include <linux/init.h>
#include <linux/compiler.h>
u64 notrace seedbox_read(void);
void __init seedbox_time_init(void);
void seedbox_idle(void);
void notrace seedbox_cpu_relax(void);
struct pt_regs;
void seedbox_syscall_progress(struct pt_regs *regs, int scno);
/* Branch entropy from the PV page; seedbox_take_entropy lives in random.c. */
u64 seedbox_entropy_gen(void);
void seedbox_read_entropy(u8 entropy[32]);
void seedbox_take_entropy(void);
#endif
