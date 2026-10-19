/* **********************************************************
 * Copyright (c) 2026 Google, Inc.  All rights reserved.
 * **********************************************************/

/*
 * Redistribution and use in source and binary forms, with or without
 * modification, are permitted provided that the following conditions are met:
 *
 * * Redistributions of source code must retain the above copyright notice,
 *   this list of conditions and the following disclaimer.
 *
 * * Redistributions in binary form must reproduce the above copyright notice,
 *   this list of conditions and the following disclaimer in the documentation
 *   and/or other materials provided with the distribution.
 *
 * * Neither the name of Google, Inc. nor the names of its contributors may be
 *   used to endorse or promote products derived from this software without
 *   specific prior written permission.
 *
 * THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS "AS IS"
 * AND ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT LIMITED TO, THE
 * IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS FOR A PARTICULAR PURPOSE
 * ARE DISCLAIMED. IN NO EVENT SHALL GOOGLE, INC. OR CONTRIBUTORS BE LIABLE
 * FOR ANY DIRECT, INDIRECT, INCIDENTAL, SPECIAL, EXEMPLARY, OR CONSEQUENTIAL
 * DAMAGES (INCLUDING, BUT NOT LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS OR
 * SERVICES; LOSS OF USE, DATA, OR PROFITS; OR BUSINESS INTERRUPTION) HOWEVER
 * CAUSED AND ON ANY THEORY OF LIABILITY, WHETHER IN CONTRACT, STRICT
 * LIABILITY, OR TORT (INCLUDING NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY
 * OUT OF THE USE OF THIS SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH
 * DAMAGE.
 */

/* Application-only state oracle extracted from 356c1821045a4b5678d0813e132074324623df6e a64_prop_support.c. No DR internal ABI, fork runner, ptrace or calibrator. */
#define _GNU_SOURCE
#include <asm/hwcap.h>
#include <errno.h>
#include <stddef.h>
#include <stdlib.h>
#include <string.h>
#include <sys/auxv.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <unistd.h>
#include "state_model.h"
#include "signal_records.h"
#ifndef HWCAP_DIT
#    define HWCAP_DIT (1 << 24)
#endif
#ifndef HWCAP_FLAGM
#    define HWCAP_FLAGM (1 << 27)
#endif
#ifndef HWCAP_SB
#    define HWCAP_SB (1 << 29)
#endif
#ifndef HWCAP_PACA
#    define HWCAP_PACA (1 << 30)
#endif
#ifndef HWCAP_PACG
#    define HWCAP_PACG (1UL << 31)
#endif
#ifndef HWCAP2_FRINT
#    define HWCAP2_FRINT (1 << 8)
#endif
#ifndef HWCAP2_I8MM
#    define HWCAP2_I8MM (1 << 13)
#endif
#ifndef HWCAP2_BF16
#    define HWCAP2_BF16 (1 << 14)
#endif
#ifndef HWCAP2_AFP
#    define HWCAP2_AFP (1 << 20)
#endif

/* The generator's CPU features (declared in a64_dr_prop_gen.h). */
feat_t feat;
int gen_dczva_ok;
/* The calling thread's generator layout slot (a64_dr_prop_gen.h). */
__thread uint64_t a64g_code_base = A64G_CODE_BASE0;
__thread uint64_t a64g_data_base = A64G_DATA_BASE0;

static __thread a64p_worker_t *a64p_cur;

uint64_t a64p_fault_pstate_ignore = 0;
void (*a64p_on_masked_pstate)(int i, fault_rec_t *n, fault_rec_t *d);
void (*a64p_fault_hook)(a64p_worker_t *w, int n, ucontext_t *uc);

void
a64p_worker_init_slot(a64p_worker_t *w, int slot)
{
    if (slot < 0 || slot >= A64G_MAX_SLOTS) {
        fprintf(stderr, "ERROR: layout slot %d out of range\n", slot);
        exit(2);
    }
    memset(w, 0, sizeof(*w));
    w->alog = malloc(sizeof(arec_t) * MAXA);
    w->xres = w->vres = -1;
    w->code_mapped_mode = -1;
    w->slot = slot;
    w->code_base = a64g_slot_code_base(slot);
    w->data_base = a64g_slot_data_base(slot);
    /* HARN_ADDR's offset from DATA_BASE, for w's slot. */
    w->harn = (harn_t *)(uintptr_t)(w->data_base + (HARN_ADDR - DATA_BASE));
}

void
a64p_worker_init(a64p_worker_t *w)
{
    a64p_worker_init_slot(w, 0);
}

void
a64p_worker_bind(a64p_worker_t *w)
{
    a64p_cur = w;
    if (w != NULL) {
        a64g_code_base = w->code_base;
        a64g_data_base = w->data_base;
    }
}

/***************************************************************************
 * Host-side entry trampoline.  The block's own entry stub loads every
 * register from harn->in; its exit stub stores them to harn->out and returns
 * to label 1 below with the host SP from harn->host_sp.
 */
__asm__(".text\n"
        ".global run_code\n"
        ".type run_code, %function\n"
        ".p2align 2\n"
        "run_code:\n"
        "  hint #34\n"   /* bti c */
        "  mov x4, x2\n" /* run */
        "  stp x29, x30, [sp, #-176]!\n"
        "  mov x29, sp\n"
        "  stp x19, x20, [sp, #16]\n"
        "  stp x21, x22, [sp, #32]\n"
        "  stp x23, x24, [sp, #48]\n"
        "  stp x25, x26, [sp, #64]\n"
        "  stp x27, x28, [sp, #80]\n"
        "  stp d8, d9, [sp, #96]\n"
        "  stp d10, d11, [sp, #112]\n"
        "  stp d12, d13, [sp, #128]\n"
        "  stp d14, d15, [sp, #144]\n"
        "  mrs x2, fpcr\n"
        "  str x2, [sp, #160]\n"
        "  mrs x2, fpsr\n"
        "  str x2, [sp, #168]\n"
        "  mov x3, sp\n"
        "  str x3, [x1, #1600]\n"
        "  adr x3, 1f\n"
        "  str x3, [x1, #1608]\n"
        /* run_id = run, last before entering the block. */
        "  add x3, x1, #1616\n"
        "  stlr x4, [x3]\n"
        "  br x0\n"
        "1:\n"
        /* x1 is &harn->host_sp (exit stub): run_id = -1. */
        "  mov x2, #-1\n"
        "  add x3, x1, #16\n"
        "  stlr x2, [x3]\n"
        "  ldr x2, [sp, #160]\n"
        "  msr fpcr, x2\n"
        "  ldr x2, [sp, #168]\n"
        "  msr fpsr, x2\n"
        "  ldp x19, x20, [sp, #16]\n"
        "  ldp x21, x22, [sp, #32]\n"
        "  ldp x23, x24, [sp, #48]\n"
        "  ldp x25, x26, [sp, #64]\n"
        "  ldp x27, x28, [sp, #80]\n"
        "  ldp d8, d9, [sp, #96]\n"
        "  ldp d10, d11, [sp, #112]\n"
        "  ldp d12, d13, [sp, #128]\n"
        "  ldp d14, d15, [sp, #144]\n"
        "  ldp x29, x30, [sp], #176\n"
        "  ret\n"
        ".global run_code_end\n"
        "run_code_end:\n"
        ".size run_code, .-run_code\n");

/***************************************************************************
 * Signal frame parsing and hashing.
 */
uint64_t
a64p_hash_bytes(const void *p, size_t n, uint64_t h)
{
    const uint64_t *w = (const uint64_t *)p;
    for (size_t i = 0; i < n / 8; i++) {
        h ^= w[i];
        h *= 0x100000001b3ULL;
        h ^= h >> 29;
    }
    return h;
}

uint64_t
a64p_vhash(const __uint128_t *v, int vres)
{
    uint64_t h = 0xcbf29ce484222325ULL;
    for (int i = 0; i < 32; i++) {
        if (i == vres)
            continue;
        h = a64p_hash_bytes(&v[i], 16, h);
    }
    return h;
}

/***************************************************************************
 * Signal handlers.
 */
static void
sync_handler(int sig, siginfo_t *si, void *ucv)
{
    a64p_worker_t *w = a64p_cur;
    ucontext_t *uc = (ucontext_t *)ucv;
    uint64_t pc = uc->uc_mcontext.pc;
    if (w == NULL || !w->in_block ||
        (pc >= w->code_base && pc < w->code_hi &&
         !(pc >= w->blk_lo && pc < w->exit_pc))) {
        /* A genuine crash outside the generated block: re-raise with the default. */
        char buf[160];
        int len = snprintf(buf, sizeof(buf),
                           "a64_dr_prop: unexpected signal %d at pc 0x%llx addr 0x%llx "
                           "(in_block %d)\n",
                           sig, (unsigned long long)pc, (unsigned long long)si->si_addr,
                           w == NULL ? -1 : w->in_block);
        if (write(2, buf, len) < 0) {
            /* Nothing we can do. */
        }
        signal(sig, SIG_DFL);
        return;
    }
    int n = w->nf++;
    if (n < MAXF) {
        fault_rec_t *f = &w->flog[n];
        memset(f, 0, sizeof(*f));
        f->signo = sig;
        f->code = si->si_code;
        f->addr = (uint64_t)si->si_addr;
        f->pc = pc;
        f->sp = uc->uc_mcontext.sp;
        f->pstate = uc->uc_mcontext.pstate;
        f->fault_address = uc->uc_mcontext.fault_address;
        memcpy(f->x, uc->uc_mcontext.regs, sizeof(f->x));
        struct a64p_fpsimd_rec *fp =
            (struct a64p_fpsimd_rec *)a64p_find_rec(uc, FPSIMD_MAGIC_);
        if (fp != NULL) {
            f->has_fp = 1;
            f->fpsr = fp->fpsr;
            f->fpcr = fp->fpcr;
            memcpy(f->v, fp->v, sizeof(f->v));
        }
        struct a64p_esr_rec *er = (struct a64p_esr_rec *)a64p_find_rec(uc, ESR_MAGIC_);
        if (er != NULL) {
            f->has_esr = 1;
            f->esr = er->esr;
        }
    }
    if (a64p_fault_hook != NULL)
        a64p_fault_hook(w, n, uc);
    if (pc >= w->blk_lo && pc < w->exit_pc && n < MAXF_RUN)
        uc->uc_mcontext.pc = pc + 4;
    else
        uc->uc_mcontext.pc = w->exit_pc; /* wild branch or too many faults */
}

static void
async_handler(int sig, siginfo_t *si, void *ucv)
{
    a64p_worker_t *w = a64p_cur;
    ucontext_t *uc = (ucontext_t *)ucv;
    if (w == NULL || !w->async_on || w->na >= MAXA)
        return;
    arec_t *r = &w->alog[w->na];
    r->sig = sig;
    r->run = a64p_run_id(w);
    r->pc = uc->uc_mcontext.pc;
    r->sp = uc->uc_mcontext.sp;
    r->pstate = uc->uc_mcontext.pstate;
    memcpy(r->x, uc->uc_mcontext.regs, sizeof(r->x));
    struct a64p_fpsimd_rec *fp =
        (struct a64p_fpsimd_rec *)a64p_find_rec(uc, FPSIMD_MAGIC_);
    if (fp != NULL) {
        r->fpsr = fp->fpsr;
        r->fpcr = fp->fpcr;
        r->vhash = a64p_vhash(fp->v, w->vres);
        if (w->vres >= 0)
            r->vres_lo = (uint64_t)fp->v[w->vres];
    } else
        r->vhash = 0;
    r->dhash = a64p_hash_bytes((void *)D_ADDR, D_SIZE, 0xcbf29ce484222325ULL);
    r->applied = 0;
    if (w->in_block && r->pc >= w->blk_lo && r->pc <= w->exit_pc && w->xres >= 0) {
        /* Change state the block never reads: it must survive the sigreturn. */
        uint64_t delta = 0x10001ULL * (uint64_t)(w->na + 1);
        uc->uc_mcontext.regs[w->xres] += delta;
        if (fp != NULL && w->vres >= 0)
            *(uint64_t *)&fp->v[w->vres] += delta;
        r->applied = 1;
        w->delta_sum += delta;
    }
    w->na++;
}

void
a64p_install_handlers(a64p_worker_t *w, int altstack)
{
    struct sigaction sa;
    a64p_worker_bind(w);
    memset(&sa, 0, sizeof(sa));
    sa.sa_sigaction = sync_handler;
    sa.sa_flags = SA_SIGINFO | SA_NODEFER | (altstack ? SA_ONSTACK : 0);
    sigemptyset(&sa.sa_mask);
    if (altstack) {
        if (w->altstk.ss_sp == NULL) {
            w->altstk.ss_size = 256 * 1024;
            w->altstk.ss_sp = malloc(w->altstk.ss_size);
        }
        w->altstk.ss_flags = 0;
        sigaltstack(&w->altstk, NULL);
    }
    sigaction(SIGSEGV, &sa, NULL);
    sigaction(SIGBUS, &sa, NULL);
    sigaction(SIGILL, &sa, NULL);
    sigaction(SIGTRAP, &sa, NULL);
    sa.sa_sigaction = async_handler;
    sa.sa_flags = SA_SIGINFO | SA_RESTART | (altstack ? SA_ONSTACK : 0);
    sigaction(SIGALRM, &sa, NULL);
    sigaction(SIGUSR1, &sa, NULL);
    sigaction(SIGRTMIN + 2, &sa, NULL);
}

/***************************************************************************
 * Memory and code installation.
 */
int
a64p_init_features(void)
{
    unsigned long h1 = getauxval(AT_HWCAP), h2 = getauxval(AT_HWCAP2);
    feat.lse = (h1 & HWCAP_ATOMICS) != 0;
    feat.pauth = (h1 & HWCAP_PACA) != 0;
    feat.pacg = (h1 & HWCAP_PACG) != 0;
    feat.dit = (h1 & HWCAP_DIT) != 0;
    feat.fp16 = (h1 & HWCAP_FPHP) != 0 && (h1 & HWCAP_ASIMDHP) != 0;
    feat.jscvt = (h1 & HWCAP_JSCVT) != 0;
    feat.fcma = (h1 & HWCAP_FCMA) != 0;
    feat.lrcpc = (h1 & HWCAP_LRCPC) != 0;
    feat.dotprod = (h1 & HWCAP_ASIMDDP) != 0;
    feat.rdm = (h1 & HWCAP_ASIMDRDM) != 0;
    feat.flagm = (h1 & HWCAP_FLAGM) != 0;
    feat.flagm2 = (h2 & HWCAP2_FLAGM2) != 0;
    feat.frint = (h2 & HWCAP2_FRINT) != 0;
    feat.i8mm = (h2 & HWCAP2_I8MM) != 0;
    feat.bf16 = (h2 & HWCAP2_BF16) != 0;
    feat.aes = (h1 & HWCAP_AES) != 0;
    feat.crc32 = (h1 & HWCAP_CRC32) != 0;
    feat.sb = (h1 & HWCAP_SB) != 0;
    feat.afp = (h2 & HWCAP2_AFP) != 0;
    feat.sha2 = (h1 & HWCAP_SHA2) != 0;
    uint64_t dczid;
    __asm__ volatile("mrs %0, dczid_el0" : "=r"(dczid));
    int dczva_ok = !(dczid & 16) && (4ULL << (dczid & 15)) == 64;
    gen_dczva_ok = dczva_ok;
    return dczva_ok;
}

/* Every protection change matters to an oracle (the guard page is the fault
 * oracle; code protection decides how DR sees the code): never ignore one.
 */
static void
xmprotect(uint64_t addr, uint64_t size, int prot)
{
    if (mprotect((void *)addr, size, prot) != 0) {
        fprintf(stderr, "ERROR: mprotect(0x%llx, 0x%llx, %d) failed (page size %ld)\n",
                (unsigned long long)addr, (unsigned long long)size, prot,
                sysconf(_SC_PAGESIZE));
        exit(2);
    }
}

void
a64p_setup_maps(void)
{
    void *p = mmap((void *)DATA_BASE, DATA_MAP_SIZE, PROT_READ | PROT_WRITE,
                   MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED_NOREPLACE, -1, 0);
    if (p != (void *)DATA_BASE) {
        fprintf(stderr, "ERROR: cannot map data at 0x%llx\n",
                (unsigned long long)DATA_BASE);
        exit(2);
    }
    xmprotect(GUARD_ADDR, GUARD_SIZE, PROT_NONE);
    ((harn_t *)HARN_ADDR)->run_id = -1;
}

void
a64p_sync_icache(uint64_t start, uint64_t len, int honour_dic)
{
    uint64_t ctr;
    __asm__ volatile("mrs %0, ctr_el0" : "=r"(ctr));
    uint64_t dline = 4ULL << ((ctr >> 16) & 15), iline = 4ULL << (ctr & 15);
    if (!honour_dic || !(ctr & (1ULL << 28))) {
        for (uint64_t a = start & ~(dline - 1); a < start + len; a += dline)
            __asm__ volatile("dc cvau, %0" : : "r"(a) : "memory");
    }
    __asm__ volatile("dsb ish" : : : "memory");
    if (!honour_dic || !(ctr & (1ULL << 29))) {
        for (uint64_t a = start & ~(iline - 1); a < start + len; a += iline)
            __asm__ volatile("ic ivau, %0" : : "r"(a) : "memory");
        __asm__ volatile("dsb ish" : : : "memory");
    }
    __asm__ volatile("isb" : : : "memory");
}

void
a64p_install_code(a64p_worker_t *w, block_t *b, int mode)
{
    size_t len = (size_t)b->a.n * 4;
    if (mode == 0 || w->code_mapped_mode != mode) {
        munmap((void *)CODE_BASE, CODE_SIZE);
        int prot =
            mode == 1 ? PROT_READ | PROT_WRITE | PROT_EXEC : PROT_READ | PROT_WRITE;
        void *p = mmap((void *)CODE_BASE, CODE_SIZE, prot,
                       MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED_NOREPLACE, -1, 0);
        if (p != (void *)CODE_BASE) {
            fprintf(stderr, "ERROR: cannot map code at 0x%llx\n",
                    (unsigned long long)CODE_BASE);
            exit(2);
        }
        w->code_mapped_mode = mode;
        if (mode == 2 || mode == 3)
            xmprotect(CODE_BASE, CODE_SIZE, PROT_READ | PROT_EXEC);
    }
    if (mode == 2 || mode == 3)
        xmprotect(CODE_BASE, CODE_SIZE, PROT_READ | PROT_WRITE);
    memcpy((void *)CODE_BASE, b->a.w, len);
    if (mode == 0)
        xmprotect(CODE_BASE, CODE_SIZE, PROT_READ | PROT_EXEC);
    else if (mode == 2 || mode == 3)
        xmprotect(CODE_BASE, CODE_SIZE, PROT_READ | PROT_EXEC);
    if (mode == 0)
        __builtin___clear_cache((char *)CODE_BASE, (char *)CODE_BASE + len);
    else
        a64p_sync_icache(CODE_BASE, len, mode == 3);
    w->code_hi = CODE_BASE + len;
}

void
a64p_prepare_run(a64p_worker_t *w, block_t *b)
{
    harn_t *h = w->harn;
    memcpy((void *)D_ADDR, b->d, D_SIZE);
    memcpy((void *)STK_CMP_LO, b->stk, STK_CMP);
    h->in = b->in;
    memset(&h->out, 0xee, sizeof(h->out));
    w->blk_lo = pc_of(b->block_start);
    w->exit_pc = pc_of(b->exit_start);
    w->blk_hi = w->exit_pc;
    w->code_hi = CODE_BASE + (uint64_t)b->a.n * 4;
    w->nf = 0;
}

static void
finish_run(void)
{
    if (feat.dit)
        __asm__ volatile(".inst 0xd503405f" ::: "memory"); /* msr dit, #0 */
}

_Static_assert(offsetof(harn_t, host_sp) == 1600, "run_code uses this offset");
_Static_assert(offsetof(harn_t, run_id) == 1616, "run_code uses this offset");

void
a64p_run_block_id(a64p_worker_t *w, block_t *b, result_t *res, int run)
{
    harn_t *h = w->harn;
    a64p_prepare_run(w, b);
    w->in_block = 1;
    run_code(CODE_BASE, h, run);
    w->in_block = 0;
    finish_run();
    res->out = h->out;
    memcpy(res->d, (void *)D_ADDR, D_SIZE);
    memcpy(res->stk, (void *)STK_CMP_LO, STK_CMP);
    res->nf = w->nf;
    memcpy(res->f, w->flog, sizeof(res->f));
}

void
a64p_run_block(a64p_worker_t *w, block_t *b, result_t *res)
{
    a64p_run_block_id(w, b, res, 0);
}

int
a64p_make_block(block_t *b, const gen_opts_t *o, uint64_t seed)
{
    for (int attempt = 0; attempt < 8; attempt++) {
        /* Retries use a derived seed so the mapping stays deterministic. */
        if (gen_block(b, seed + attempt * 0x100000000ULL, o))
            return 1;
    }
    return 0;
}

/***************************************************************************
 * Comparison and reporting.
 */
int
a64p_cmp_fault(FILE *o, int i, fault_rec_t *n, fault_rec_t *d)
{
    int bad = 0;
#define FCMP(field, fmt, type)                                                        \
    do {                                                                              \
        if (n->field != d->field) {                                                   \
            fprintf(o, "    fault[%d] %-14s native " fmt "  dr " fmt "\n", i, #field, \
                    (type)n->field, (type)d->field);                                  \
            bad++;                                                                    \
        }                                                                             \
    } while (0)
    FCMP(signo, "%d", int);
    FCMP(code, "%d", int);
    FCMP(addr, "0x%llx", unsigned long long);
    FCMP(pc, "0x%llx", unsigned long long);
    FCMP(sp, "0x%llx", unsigned long long);
    /* Bits in a64p_fault_pstate_ignore are the test's named known deviations:
     * report each masked difference to it rather than dropping it.
     */
    if (((n->pstate ^ d->pstate) & ~a64p_fault_pstate_ignore) != 0) {
        fprintf(o, "    fault[%d] pstate         native 0x%llx  dr 0x%llx\n", i,
                (unsigned long long)n->pstate, (unsigned long long)d->pstate);
        bad++;
    } else if (n->pstate != d->pstate && a64p_on_masked_pstate != NULL)
        a64p_on_masked_pstate(i, n, d);
    /* For BRK the kernel leaves the previous fault's FAR and ESR in the frame. */
    if (n->signo != SIGTRAP) {
        FCMP(fault_address, "0x%llx", unsigned long long);
        FCMP(has_esr, "%d", int);
        FCMP(esr, "0x%llx", unsigned long long);
    }
    FCMP(has_fp, "%d", int);
    FCMP(fpsr, "0x%x", unsigned int);
    FCMP(fpcr, "0x%x", unsigned int);
    for (int r = 0; r < 31; r++) {
        if (n->x[r] != d->x[r]) {
            fprintf(o, "    fault[%d] x%-13d native 0x%llx  dr 0x%llx\n", i, r,
                    (unsigned long long)n->x[r], (unsigned long long)d->x[r]);
            bad++;
        }
    }
    for (int r = 0; r < 32; r++) {
        if (n->v[r] != d->v[r]) {
            fprintf(
                o, "    fault[%d] v%-13d native 0x%016llx%016llx  dr 0x%016llx%016llx\n",
                i, r, (unsigned long long)(n->v[r] >> 64), (unsigned long long)n->v[r],
                (unsigned long long)(d->v[r] >> 64), (unsigned long long)d->v[r]);
            bad++;
        }
    }
#undef FCMP
    return bad;
}

int
a64p_cmp_state(FILE *o, const char *what, cpu_state_t *n, cpu_state_t *d, int xres,
               uint64_t xadj, int vres, uint64_t vadj)
{
    int bad = 0;
    for (int r = 0; r < 31; r++) {
        uint64_t exp = n->x[r] + (r == xres ? xadj : 0);
        if (exp != d->x[r]) {
            fprintf(o, "    %s x%-3d native 0x%016llx  dr 0x%016llx\n", what, r,
                    (unsigned long long)exp, (unsigned long long)d->x[r]);
            bad++;
        }
    }
#define SCMP(f)                                                                  \
    do {                                                                         \
        if (n->f != d->f) {                                                      \
            fprintf(o, "    %s %-4s native 0x%016llx  dr 0x%016llx\n", what, #f, \
                    (unsigned long long)n->f, (unsigned long long)d->f);         \
            bad++;                                                               \
        }                                                                        \
    } while (0)
    SCMP(sp);
    SCMP(nzcv);
    SCMP(fpsr);
    SCMP(fpcr);
#undef SCMP
    for (int r = 0; r < 32; r++) {
        __uint128_t exp = n->v[r] + (r == vres ? (__uint128_t)vadj : 0);
        if (r == vres)
            exp = (n->v[r] & ~(__uint128_t)~0ULL) | (uint64_t)((uint64_t)n->v[r] + vadj);
        if (exp != d->v[r]) {
            fprintf(o, "    %s v%-3d native 0x%016llx%016llx  dr 0x%016llx%016llx\n",
                    what, r, (unsigned long long)(exp >> 64), (unsigned long long)exp,
                    (unsigned long long)(d->v[r] >> 64), (unsigned long long)d->v[r]);
            bad++;
        }
    }
    return bad;
}

int
a64p_cmp_mem(FILE *o, block_t *b, result_t *n, result_t *d)
{
    int bad = 0;
    for (uint64_t i = 0; i < D_SIZE; i++) {
        if (n->d[i] != d->d[i]) {
            if (bad < 16) {
                fprintf(o, "    mem D+0x%03llx (0x%llx) native 0x%02x  dr 0x%02x\n",
                        (unsigned long long)i, (unsigned long long)(D_ADDR + i), n->d[i],
                        d->d[i]);
            }
            bad++;
        }
    }
    for (uint64_t i = 0; i < STK_CMP; i++) {
        if (b->sinit_final[i] && n->stk[i] != d->stk[i]) {
            if (bad < 16) {
                fprintf(o, "    mem stack 0x%llx native 0x%02x  dr 0x%02x\n",
                        (unsigned long long)(STK_CMP_LO + i), n->stk[i], d->stk[i]);
            }
            bad++;
        }
    }
    if (bad > 16)
        fprintf(o, "    ... %d differing bytes in total\n", bad);
    return bad;
}

int
a64p_cmp_results(FILE *o, block_t *b, result_t *n, result_t *d)
{
    int bad = a64p_cmp_state(o, "final", &n->out, &d->out, -1, 0, -1, 0);
    bad += a64p_cmp_mem(o, b, n, d);
    if (n->nf != d->nf) {
        fprintf(o, "    fault count native %d  dr %d\n", n->nf, d->nf);
        bad++;
    }
    if (d->nf > MAXF) {
        fprintf(o, "    %d faults: more than the %d a run can record\n", d->nf, MAXF);
        bad++;
    }
    for (int i = 0; i < n->nf && i < d->nf && i < MAXF; i++)
        bad += a64p_cmp_fault(o, i, &n->f[i], &d->f[i]);
    return bad;
}

void
a64p_print_faults(FILE *o, const char *who, result_t *r)
{
    for (int i = 0; i < r->nf && i < MAXF; i++) {
        fprintf(o,
                "    %s fault[%d]: sig %d code %d addr 0x%llx pc 0x%llx esr 0x%llx(%d) "
                "far 0x%llx\n",
                who, i, r->f[i].signo, r->f[i].code, (unsigned long long)r->f[i].addr,
                (unsigned long long)r->f[i].pc, (unsigned long long)r->f[i].esr,
                r->f[i].has_esr, (unsigned long long)r->f[i].fault_address);
    }
}

int
a64p_check_planned(block_t *b, result_t *r)
{
    if (r->nf > MAXF) {
        fprintf(stderr, "  %d faults: more than the %d a run can record\n", r->nf, MAXF);
        return 0;
    }
    for (int i = 0; i < r->nf; i++) {
        uint64_t pc = r->f[i].pc;
        if (pc >= pc_of(b->block_start) && pc < pc_of(b->exit_start)) {
            int idx = (int)((pc - CODE_BASE) / 4);
            if (!b->a.fault_ok[idx]) {
                fprintf(stderr, "  unplanned fault %d (sig %d) at 0x%llx: %s\n", i,
                        r->f[i].signo, (unsigned long long)pc, b->a.txt[idx]);
                for (int k = idx - 8 < b->block_start ? b->block_start : idx - 8;
                     k <= idx; k++)
                    fprintf(stderr, "      %llx: %s\n", (unsigned long long)pc_of(k),
                            b->a.txt[k]);
                return 0;
            }
        } else if (!b->wild_branch) {
            fprintf(stderr, "  unplanned fault %d (sig %d) outside the block at 0x%llx\n",
                    i, r->f[i].signo, (unsigned long long)pc);
            return 0;
        }
    }
    return 1;
}

