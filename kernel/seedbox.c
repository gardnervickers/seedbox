// SPDX-License-Identifier: GPL-2.0
/*
 * Seedbox deterministic time.
 *
 * Every kernel TSC read (rdtsc/rdtsc_ordered) returns virtual time published
 * by the VMM, never the hardware counter. Before the shared page is mapped,
 * reads use two port reads. Repeated reads with no new publication advance
 * by one tick (SEEDBOX_PV_READ_NS under the PV clock), so polling loops make
 * progress deterministically. User-mode
 * RDTSC/RDTSCP trap through CR4.TSD and are emulated here.
 *
 * Once the page is active, interrupt masking (8259) and virtio-mmio interrupt
 * status go through the page instead of trapping port and MMIO accesses, and
 * the PIT is armed with a single port write. The VMM applies page state only
 * at deterministic points.
 *
 * In the VMM's PV clock timebase (no host perf counter), guest progress is
 * not measured: the guest exits at a clock bound and a syscall budget the VMM
 * publishes instead. Both checks sit behind a static key, so in the counter
 * timebase they compile to no-ops and retire no extra branches.
 */
#include <linux/build_bug.h>
#include <linux/stddef.h>
#include <linux/init.h>
#include <linux/io.h>
#include <linux/kernel.h>
#include <linux/mm.h>
#include <linux/pagewalk.h>
#include <linux/sched.h>
#include <linux/printk.h>
#include <asm/cpufeature.h>
#include <asm/fixmap.h>
#include <asm/i8259.h>
#include <asm/insn-eval.h>
#include <asm/seedbox.h>
#include <asm/tlbflush.h>
#include <asm/trap_pf.h>

struct seedbox_time_page *seedbox_page;
static unsigned long long seedbox_early_last;
DEFINE_STATIC_KEY_FALSE(seedbox_pv_clock);

noinstr unsigned long long seedbox_rdtsc(void)
{
	struct seedbox_time_page *p = READ_ONCE(seedbox_page);
	unsigned long long v, last;

	if (likely(p)) {
again:
		v = READ_ONCE(p->tsc);
		last = READ_ONCE(p->last);
		/*
		 * A read costs 1 ns, or SEEDBOX_PV_READ_NS under the PV clock,
		 * where compute is free and a loop polling the clock would
		 * otherwise need a read per nanosecond it waits. The static key
		 * retires no conditional branch in the counter timebase.
		 */
		if (static_branch_unlikely(&seedbox_pv_clock))
			last += SEEDBOX_PV_READ_NS - 1;
		if (v <= last)
			v = last + 1;
		/*
		 * PV clock: never show a time past the VMM's bound. The exit lets
		 * the VMM move time there (firing timers) and raise the bound.
		 */
		if (static_branch_unlikely(&seedbox_pv_clock) &&
		    v > READ_ONCE(p->bound)) {
			outl(1, SEEDBOX_PORT_YIELD);
			goto again;
		}
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

void __init seedbox_time_init(void)
{
	struct seedbox_time_page *p;

	/* Pin the u64-indexed ABI shared with the VMM. */
	BUILD_BUG_ON(offsetof(struct seedbox_time_page, pv_clock) != 6 * sizeof(u64));
	BUILD_BUG_ON(offsetof(struct seedbox_time_page, bound) != 7 * sizeof(u64));
	BUILD_BUG_ON(offsetof(struct seedbox_time_page, syscall_target) != 8 * sizeof(u64));
	BUILD_BUG_ON(offsetof(struct seedbox_time_page, entropy_gen) != 9 * sizeof(u64));
	BUILD_BUG_ON(offsetof(struct seedbox_time_page, entropy[0]) != 10 * sizeof(u64));
	BUILD_BUG_ON(offsetof(struct seedbox_time_page, entropy[1]) != 11 * sizeof(u64));
	BUILD_BUG_ON(offsetof(struct seedbox_time_page, entropy[2]) != 12 * sizeof(u64));
	BUILD_BUG_ON(offsetof(struct seedbox_time_page, entropy[3]) != 13 * sizeof(u64));
	BUILD_BUG_ON(offsetof(struct seedbox_time_page, crossings) != 14 * sizeof(u64));
	BUILD_BUG_ON(offsetof(struct seedbox_time_page, crossing_target) != 15 * sizeof(u64));
	BUILD_BUG_ON(offsetof(struct seedbox_time_page, crossing_walks) != 16 * sizeof(u64));

	/*
	 * setup_arch calls us after jump_label_init and early_ioremap_init,
	 * before tsc_early_init selects delay_tsc. A permanent, write-back
	 * fixmap needs no allocator and survives the boot page-table switch.
	 * In particular, the PIC's 8 * 2 us + 100 us initialization delays
	 * must poll this page, not make two port exits per nanosecond.
	 */
	set_fixmap(FIX_SEEDBOX_TIME, SEEDBOX_TIME_PAGE_PHYS);
	p = (struct seedbox_time_page *)fix_to_virt(FIX_SEEDBOX_TIME);
	WRITE_ONCE(p->last, seedbox_early_last);
	if (READ_ONCE(p->pv_clock)) {
		static_branch_enable(&seedbox_pv_clock);
		WRITE_ONCE(p->pv_clock, 2);	/* tell the VMM this kernel has it */
	}
	WRITE_ONCE(p->pic_imr, cached_irq_mask);
	outl(SEEDBOX_ABI, SEEDBOX_PORT_TIME_READY);
	WRITE_ONCE(seedbox_page, p);
	pr_info("seedbox: deterministic time page active\n");
}

/*
 * PV clock: crossings into user code pages, a progress event the guest
 * produces itself. With the key on, vm_get_page_prot gives every PTE of an
 * executable VMA NX, so an instruction fetch from a page faults here. We
 * clear NX on that page alone and count a crossing. At most SEEDBOX_XRING
 * pages of the current address space are executable at once: opening one
 * more closes the oldest, so a loop spanning more pages than that faults on
 * every pass. Every step follows the guest instruction stream.
 *
 * The ring belongs to one address space. A fault from another one first
 * closes every page of it, which also catches pages the ring dropped when
 * we last switched away from it. Single CPU and no preemption: nothing runs
 * between taking the mmap lock and returning.
 */
#ifndef SEEDBOX_XRING
#define SEEDBOX_XRING	4
#endif
static unsigned long seedbox_xring[SEEDBOX_XRING];
static unsigned int seedbox_xhead, seedbox_xlen;
static struct mm_struct *seedbox_xmm;	/* compared, never dereferenced */

static pte_t *seedbox_user_pte(struct mm_struct *mm, unsigned long addr,
			       spinlock_t **ptl)
{
	pgd_t *pgd = pgd_offset(mm, addr);
	p4d_t *p4d;
	pud_t *pud;
	pmd_t *pmd, pmdv;

	if (pgd_none(*pgd) || pgd_bad(*pgd))
		return NULL;
	p4d = p4d_offset(pgd, addr);
	if (p4d_none(*p4d) || p4d_bad(*p4d))
		return NULL;
	pud = pud_offset(p4d, addr);
	if (pud_none(*pud) || pud_leaf(*pud) || pud_bad(*pud))
		return NULL;
	pmd = pmd_offset(pud, addr);
	pmdv = pmdp_get_lockless(pmd);
	/* Huge text is never closed (file THP is off), so never opened here. */
	if (pmd_none(pmdv) || pmd_leaf(pmdv) || pmd_bad(pmdv))
		return NULL;
	return pte_offset_map_lock(mm, pmd, addr, ptl);
}

/* Open (clear NX) or close (set NX) a present PTE; true if it changed. */
static bool seedbox_set_nx(struct vm_area_struct *vma, unsigned long addr, bool nx)
{
	spinlock_t *ptl;
	pte_t *ptep, old;
	bool changed = false;

	ptep = seedbox_user_pte(vma->vm_mm, addr, &ptl);
	if (!ptep)
		return false;
	old = ptep_get(ptep);
	if (pte_present(old) && !!pte_exec(old) == nx) {
		old = ptep_modify_prot_start(vma, addr, ptep);
		ptep_modify_prot_commit(vma, addr, ptep, old,
					nx ? pte_set_flags(old, _PAGE_NX & __supported_pte_mask)
					   : pte_mkexec(old));
		changed = true;
	}
	pte_unmap_unlock(ptep, ptl);
	return changed;
}

static int seedbox_close_pte(pte_t *ptep, unsigned long addr,
			     unsigned long next, struct mm_walk *walk)
{
	pte_t old = ptep_get(ptep);

	if (pte_present(old) && pte_exec(old)) {
		old = ptep_modify_prot_start(walk->vma, addr, ptep);
		ptep_modify_prot_commit(walk->vma, addr, ptep, old,
					pte_set_flags(old, _PAGE_NX & __supported_pte_mask));
		*(unsigned long *)walk->private += 1;
	}
	return 0;
}

static const struct mm_walk_ops seedbox_close_ops = {
	.pte_entry = seedbox_close_pte,
	.walk_lock = PGWALK_RDLOCK,
};

static void seedbox_close_vma_page(struct mm_struct *mm, unsigned long addr)
{
	struct vm_area_struct *vma = vma_lookup(mm, addr);

	if (vma && (vma->vm_flags & VM_EXEC) && seedbox_set_nx(vma, addr, true))
		flush_tlb_one_user(addr);
}

/* Called only with seedbox_pv_clock on. True if the fault is handled. */
bool seedbox_exec_fault(unsigned long error_code, unsigned long address)
{
	const unsigned long want = X86_PF_USER | X86_PF_INSTR | X86_PF_PROT;
	struct seedbox_time_page *p = READ_ONCE(seedbox_page);
	struct mm_struct *mm = current->mm;
	struct vm_area_struct *vma;
	unsigned long page = address & PAGE_MASK;
	bool handled = false;

	if ((error_code & want) != want || !mm || !p)
		return false;
	mmap_read_lock(mm);
	vma = vma_lookup(mm, address);
	if (!vma || !(vma->vm_flags & VM_EXEC))
		goto out;
	if (mm != seedbox_xmm) {
		VMA_ITERATOR(vmi, mm, 0);
		struct vm_area_struct *v;
		unsigned long closed = 0;

		for_each_vma(vmi, v)
			if (v->vm_flags & VM_EXEC)
				walk_page_vma(v, &seedbox_close_ops, &closed);
		if (closed)
			flush_tlb_mm(mm);
		seedbox_xmm = mm;
		seedbox_xhead = 0;
		seedbox_xlen = 0;
		WRITE_ONCE(p->crossing_walks, p->crossing_walks + 1);
	}
	if (!seedbox_set_nx(vma, page, false))
		goto out;	/* not ours (stale TLB entry): the usual path */
	flush_tlb_one_user(page);
	if (seedbox_xlen == SEEDBOX_XRING)
		seedbox_close_vma_page(mm, seedbox_xring[seedbox_xhead]);
	else
		seedbox_xlen++;
	seedbox_xring[seedbox_xhead] = page;
	seedbox_xhead = (seedbox_xhead + 1) % SEEDBOX_XRING;
	WRITE_ONCE(p->crossings, p->crossings + 1);
	handled = true;
out:
	mmap_read_unlock(mm);
	return handled;
}

static int __init seedbox_user_tsc_init(void)
{
	/* cpu_init clears TSD after setup_arch; enable user traps afterwards. */
	cr4_set_bits(X86_CR4_TSD);
	return 0;
}
early_initcall(seedbox_user_tsc_init);
