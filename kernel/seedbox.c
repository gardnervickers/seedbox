// SPDX-License-Identifier: GPL-2.0
/*
 * Seedbox deterministic time.
 *
 * Every kernel TSC read (rdtsc/rdtsc_ordered) returns virtual time published
 * by the VMM, never the hardware counter. Before the shared page is mapped,
 * reads use two port reads. Repeated reads with no new publication advance
 * by one tick, so polling loops make progress deterministically. User-mode
 * RDTSC/RDTSCP trap through CR4.TSD and are emulated here.
 *
 * Once the page is active, interrupt masking (8259) and virtio-mmio interrupt
 * status go through the page instead of trapping port and MMIO accesses, and
 * the PIT is armed with a single port write. The VMM applies page state only
 * at deterministic points.
 */
#include <linux/init.h>
#include <linux/io.h>
#include <linux/kernel.h>
#include <linux/printk.h>
#include <asm/cpufeature.h>
#include <asm/i8259.h>
#include <asm/insn-eval.h>
#include <asm/seedbox.h>
#include <asm/tlbflush.h>

struct seedbox_time_page *seedbox_page;
static unsigned long long seedbox_early_last;

noinstr unsigned long long seedbox_rdtsc(void)
{
	struct seedbox_time_page *p = READ_ONCE(seedbox_page);
	unsigned long long v, last;

	if (likely(p)) {
		v = READ_ONCE(p->tsc);
		last = READ_ONCE(p->last);
		if (v <= last)
			v = last + 1;
		WRITE_ONCE(p->last, v);
		return v;
	}
	v = inl(SEEDBOX_PORT_TIME_LO);
	v |= (unsigned long long)inl(SEEDBOX_PORT_TIME_HI) << 32;
	if (v <= seedbox_early_last)
		v = seedbox_early_last + 1;
	seedbox_early_last = v;
	return v;
}

bool seedbox_emulate_user_tsc(struct pt_regs *regs)
{
	unsigned char buf[MAX_INSN_SIZE];
	unsigned long long v;
	int n, len;
	bool tscp;

	n = insn_fetch_from_user(regs, buf);
	if (n >= 2 && buf[0] == 0x0f && buf[1] == 0x31) {
		len = 2;
		tscp = false;
	} else if (n >= 3 && buf[0] == 0x0f && buf[1] == 0x01 && buf[2] == 0xf9) {
		len = 3;
		tscp = true;
	} else {
		return false;
	}
	v = seedbox_rdtsc();
	regs->ax = (u32)v;
	regs->dx = (u32)(v >> 32);
	if (tscp)
		regs->cx = 0;
	regs->ip += len;
	return true;
}

/* The virtual TSC runs at exactly 1 GHz: no calibration against the PIT. */
unsigned long seedbox_tsc_khz(void)
{
	setup_force_cpu_cap(X86_FEATURE_TSC_KNOWN_FREQ);
	return SEEDBOX_TSC_KHZ;
}

static int __init seedbox_time_init(void)
{
	struct seedbox_time_page *p;

	p = memremap(SEEDBOX_TIME_PAGE_PHYS, PAGE_SIZE, MEMREMAP_WB);
	if (!p)
		panic("seedbox: cannot map time page");
	WRITE_ONCE(p->last, seedbox_early_last);
	WRITE_ONCE(p->pic_imr, cached_irq_mask);
	outl(SEEDBOX_ABI, SEEDBOX_PORT_TIME_READY);
	WRITE_ONCE(seedbox_page, p);
	cr4_set_bits(X86_CR4_TSD);
	pr_info("seedbox: deterministic time page active\n");
	return 0;
}
early_initcall(seedbox_time_init);
