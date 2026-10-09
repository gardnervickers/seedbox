/* SPDX-License-Identifier: GPL-2.0 */
#ifndef __ASM_SEEDBOX_H
#define __ASM_SEEDBOX_H
#include <linux/types.h>
#include <linux/init.h>
#include <linux/compiler.h>
u64 notrace seedbox_read(void);
void __init seedbox_time_init(void);
void seedbox_idle(void);
void seedbox_syscall_progress(void);
#endif
