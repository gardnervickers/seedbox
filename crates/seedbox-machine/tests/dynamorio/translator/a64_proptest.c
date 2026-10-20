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

/* Per-instruction property tests of DR's AArch64 IR, with the CPU as the
 * oracle.  Each case is one A64 word: the standalone decoder (drdecode)
 * describes it, and the word runs natively, alone, in a sandbox from several
 * random register/memory states.  What DR's IR says about the word must
 * match what the hardware did.  See README.md for the property list, the
 * sandbox and the allowlist.
 *
 * Usage: a64_proptest <mode> [options] <dis-a64*.txt...>
 *   modes: templates  every encoding class from the test vectors, as given
 *          mutants    random operand fields / bit flips of those classes
 *          random     uniformly random words
 *          regress    words that exposed DR bugs fixed with this test
 *          word       one word (-word W), e.g. to reproduce a failure
 *   -n N        cases (mutants/random; default 2000)
 *   -seed S     base seed (default 1); case i uses seed S + i
 *   -states K   random input states per case (default 4)
 *   -props a,b  only check these properties (default all)
 *   -word W     the word for "word" mode (with -seed: the case seed)
 *   -v          print allowlisted/known failures too
 *   -max-print M  failures printed per property (default 10)
 *   -strict     report known (unfixed) DR bugs as failures
 *   -trace      print every run's outcome (with "word")
 * Environment: A64_PROPTEST_N / A64_PROPTEST_SEED override -n / -seed;
 * A64_PROPTEST_STRICT=1 is -strict.
 *
 * Every failure prints one line:
 *   FAIL <property> word=0x... seed=0x... state=K "<DR disassembly>": details
 * and "a64_proptest word -word 0x... -seed 0x..." reruns that case.
 */

#ifndef _GNU_SOURCE
#    define _GNU_SOURCE
#endif
#include "dr_api.h"

#include <errno.h>
#include <setjmp.h>
#include <signal.h>
#include <stdarg.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/auxv.h>
#include <sys/mman.h>
#include <sys/time.h>
#include <ucontext.h>
#include <unistd.h>

#define GD GLOBAL_DCONTEXT

#ifndef HWCAP_DIT
#    define HWCAP_DIT (1 << 24)
#endif
#ifndef HWCAP2_AFP
#    define HWCAP2_AFP (1 << 20)
#endif

/***************************************************************************
 * Sandbox layout.  Fixed addresses keep a case reproducible from its seed.
 *
 * The code region is +-1MB of BRK landing pads around CODE_PC, where the
 * instruction under test goes; each pad is "brk #<slot>" so a literal load
 * reads position-dependent data.  Whatever the instruction does, the next
 * instruction to execute traps (BRK, or an instruction abort in the PROT_NONE
 * reservation that covers B/BL's +-128MB range), so the trap's PC is where
 * control went.
 *
 * Base registers point into a DATA_SIZE data buffer surrounded by PROT_NONE.
 */
#define CODE_PC 0x400000000000ULL
#define CODE_HALF (1ULL << 20)
#define CODE_RES_HALF (130ULL << 20)
#define DATA_BASE 0x500000000000ULL
#define DATA_SIZE (64 * 1024)
#define DATA_END (DATA_BASE + DATA_SIZE)
#define DATA_MID (DATA_BASE + DATA_SIZE / 2)
#define DATA_RES (1ULL << 20) /* PROT_NONE on each side */
#define PAGE 4096

#define NZCV_MASK 0xf0000000ULL
#define FPSR_CUMUL 0x0800009fU /* QC, IDC, IXC, UFC, OFC, DZC, IOC */
#define PSTATE_DIT (1ULL << 24)

/***************************************************************************
 * Random numbers (splitmix64): every state is a function of the case seed.
 */
typedef struct {
    uint64_t s;
} rng_t;

static uint64_t
rnd(rng_t *r)
{
    uint64_t z = (r->s += 0x9e3779b97f4a7c15ULL);
    z = (z ^ (z >> 30)) * 0xbf58476d1ce4e5b9ULL;
    z = (z ^ (z >> 27)) * 0x94d049bb133111ebULL;
    return z ^ (z >> 31);
}

static uint64_t
rnd_n(rng_t *r, uint64_t n)
{
    return rnd(r) % n;
}

static rng_t
rng_for(uint64_t seed, uint64_t stream)
{
    rng_t r = { seed * 0x2545f4914f6cdd1dULL + stream * 0x9e3779b97f4a7c15ULL };
    rnd(&r);
    return r;
}

/***************************************************************************
 * Architectural state and the outcome of one run.
 */
typedef struct {
    uint64_t x[31];
    uint64_t sp;
    uint64_t nzcv; /* PSTATE bits 31:28 */
    uint32_t fpcr, fpsr;
    __uint128_t v[32];
} state_t;

typedef struct {
    state_t st;    /* registers when control left the instruction */
    uint64_t pc;   /* where it left to (the trapping pc) */
    int sig, code; /* the signal that ended the run */
    uint64_t addr; /* its si_addr */
    int timeout;
    uint64_t pstate;
} outcome_t;

/* How a run ended. */
static bool
completed(const outcome_t *o)
{
    /* Control left the instruction: a landing pad or an instruction abort. */
    return !o->timeout && o->pc != CODE_PC;
}

static bool
undefined(const outcome_t *o)
{
    return !o->timeout && o->pc == CODE_PC && o->sig == SIGILL && o->code == ILL_ILLOPC;
}

static bool
data_fault(const outcome_t *o)
{
    return !o->timeout && o->pc == CODE_PC && (o->sig == SIGSEGV || o->sig == SIGBUS);
}

/***************************************************************************
 * Signal frame records (see arch/arm64/include/uapi/asm/sigcontext.h).
 */
struct rec_head {
    uint32_t magic, size;
};
#define FPSIMD_MAGIC_ 0x46508001
#define EXTRA_MAGIC_ 0x45585401
struct fpsimd_rec {
    struct rec_head h;
    uint32_t fpsr, fpcr;
    __uint128_t v[32];
};
struct extra_rec {
    struct rec_head h;
    uint64_t datap;
    uint32_t size;
};

static struct rec_head *
find_rec(ucontext_t *uc, uint32_t magic)
{
    uint8_t *p = (uint8_t *)uc->uc_mcontext.__reserved;
    uint8_t *end = p + sizeof(uc->uc_mcontext.__reserved);
    for (int n = 0; n < 64 && p + sizeof(struct rec_head) <= end; n++) {
        struct rec_head *h = (struct rec_head *)p;
        if (h->magic == 0)
            break;
        if (h->magic == magic)
            return h;
        if (h->magic == EXTRA_MAGIC_) {
            struct extra_rec *e = (struct extra_rec *)h;
            p = (uint8_t *)(uintptr_t)e->datap;
            end = p + e->size;
            continue;
        }
        if (h->size == 0)
            break;
        p += h->size;
    }
    return NULL;
}

/***************************************************************************
 * Running one word from one state.
 *
 * Entry is a signal return: the SIGUSR1 handler loads the input state into
 * the interrupted context and points its PC at the instruction.  The run ends
 * at the next synchronous signal (the landing pad's BRK, or a fault), whose
 * handler records the context and siglongjmp()s back.  A periodic SIGALRM
 * bounds runs that never trap (WFE and the like).
 */
enum { PHASE_IDLE, PHASE_ENTER, PHASE_RUN };
static volatile int g_phase;
static const state_t *g_in;
static outcome_t *g_out;
static volatile int g_ticks;
static sigjmp_buf g_jmp;
static uint8_t *g_data = (uint8_t *)DATA_BASE;
static uint8_t g_pristine[DATA_SIZE];
static uint64_t g_host_fpcr;
static bool g_has_dit;
static bool g_has_afp;
static uint32_t *g_code_page;
static uint64_t g_runs;
static int opt_trace; /* -trace: print every run */

static void
capture(ucontext_t *uc, int sig, siginfo_t *si)
{
    outcome_t *o = g_out;
    memcpy(o->st.x, uc->uc_mcontext.regs, sizeof(o->st.x));
    o->st.sp = uc->uc_mcontext.sp;
    o->pc = uc->uc_mcontext.pc;
    o->pstate = uc->uc_mcontext.pstate;
    o->st.nzcv = o->pstate & NZCV_MASK;
    struct fpsimd_rec *fp = (struct fpsimd_rec *)find_rec(uc, FPSIMD_MAGIC_);
    if (fp != NULL) {
        o->st.fpsr = fp->fpsr;
        o->st.fpcr = fp->fpcr;
        memcpy(o->st.v, fp->v, sizeof(o->st.v));
    }
    o->sig = sig;
    o->code = si == NULL ? 0 : si->si_code;
    o->addr = si == NULL ? 0 : (uint64_t)si->si_addr;
}

static void
enter_handler(int sig, siginfo_t *si, void *ucv)
{
    ucontext_t *uc = (ucontext_t *)ucv;
    if (g_phase != PHASE_ENTER)
        return;
    const state_t *in = g_in;
    memcpy(uc->uc_mcontext.regs, in->x, sizeof(in->x));
    uc->uc_mcontext.sp = in->sp;
    uc->uc_mcontext.pc = CODE_PC;
    uc->uc_mcontext.pstate = (uc->uc_mcontext.pstate & ~NZCV_MASK) | in->nzcv;
    struct fpsimd_rec *fp = (struct fpsimd_rec *)find_rec(uc, FPSIMD_MAGIC_);
    if (fp == NULL) {
        static const char msg[] = "a64_proptest: no FPSIMD record in signal frame\n";
        if (write(2, msg, sizeof(msg) - 1) < 0) {
            /* Nothing more to do. */
        }
        _exit(2);
    }
    fp->fpsr = in->fpsr;
    fp->fpcr = in->fpcr;
    memcpy(fp->v, in->v, sizeof(fp->v));
    g_ticks = 0;
    g_phase = PHASE_RUN;
}

static void
exit_handler(int sig, siginfo_t *si, void *ucv)
{
    if (g_phase != PHASE_RUN) {
        /* A genuine crash in the harness: die with the default action. */
        char buf[128];
        int len = snprintf(buf, sizeof(buf),
                           "a64_proptest: unexpected signal %d at pc 0x%llx\n", sig,
                           (unsigned long long)((ucontext_t *)ucv)->uc_mcontext.pc);
        if (write(2, buf, len) < 0) {
            /* Nothing more to do. */
        }
        signal(sig, SIG_DFL);
        return;
    }
    capture((ucontext_t *)ucv, sig, si);
    g_phase = PHASE_IDLE;
    siglongjmp(g_jmp, 1);
}

static void
alarm_handler(int sig, siginfo_t *si, void *ucv)
{
    if (g_phase != PHASE_RUN || ++g_ticks < 3)
        return;
    capture((ucontext_t *)ucv, sig, NULL);
    g_out->timeout = 1;
    g_phase = PHASE_IDLE;
    siglongjmp(g_jmp, 1);
}

static void
setup_sandbox(void)
{
    /* Reserve the branch range, then map the landing pads. */
    void *res =
        mmap((void *)(CODE_PC - CODE_RES_HALF), 2 * CODE_RES_HALF, PROT_NONE,
             MAP_PRIVATE | MAP_ANONYMOUS | MAP_NORESERVE | MAP_FIXED_NOREPLACE, -1, 0);
    uint32_t *code =
        mmap((void *)(CODE_PC - CODE_HALF), 2 * CODE_HALF, PROT_READ | PROT_WRITE,
             MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED, -1, 0);
    void *dres =
        mmap((void *)(DATA_BASE - DATA_RES), DATA_SIZE + 2 * DATA_RES, PROT_NONE,
             MAP_PRIVATE | MAP_ANONYMOUS | MAP_NORESERVE | MAP_FIXED_NOREPLACE, -1, 0);
    void *data = mmap((void *)DATA_BASE, DATA_SIZE, PROT_READ | PROT_WRITE,
                      MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED, -1, 0);
    if (res != (void *)(CODE_PC - CODE_RES_HALF) ||
        code != (uint32_t *)(CODE_PC - CODE_HALF) ||
        dres != (void *)(DATA_BASE - DATA_RES) || data != (void *)DATA_BASE) {
        fprintf(stderr, "ERROR: cannot map the sandbox at its fixed addresses\n");
        exit(2);
    }
    size_t n = 2 * CODE_HALF / 4;
    for (size_t i = 0; i < n; i++)
        code[i] = 0xd4200000 | (uint32_t)(i & 0xffff) << 5; /* brk #i */
    mprotect(code, 2 * CODE_HALF, PROT_READ | PROT_EXEC);
    __builtin___clear_cache((char *)code, (char *)code + 2 * CODE_HALF);
    g_code_page = (uint32_t *)(CODE_PC & ~(uint64_t)(PAGE - 1));

    static stack_t altstk;
    altstk.ss_size = 256 * 1024;
    altstk.ss_sp = malloc(altstk.ss_size);
    altstk.ss_flags = 0;
    sigaltstack(&altstk, NULL);
    struct sigaction sa;
    memset(&sa, 0, sizeof(sa));
    sa.sa_flags = SA_SIGINFO | SA_ONSTACK | SA_RESTART;
    sigfillset(&sa.sa_mask);
    sa.sa_sigaction = enter_handler;
    sigaction(SIGUSR1, &sa, NULL);
    sa.sa_sigaction = exit_handler;
    sigaction(SIGTRAP, &sa, NULL);
    sigaction(SIGSEGV, &sa, NULL);
    sigaction(SIGBUS, &sa, NULL);
    sigaction(SIGILL, &sa, NULL);
    sigaction(SIGFPE, &sa, NULL);
    sa.sa_sigaction = alarm_handler;
    sigaction(SIGALRM, &sa, NULL);
    struct itimerval it = { { 0, 20000 }, { 0, 20000 } };
    setitimer(ITIMER_REAL, &it, NULL);

    __asm__ volatile("mrs %0, fpcr" : "=r"(g_host_fpcr));
    g_has_dit = (getauxval(AT_HWCAP) & HWCAP_DIT) != 0;
    g_has_afp = (getauxval(AT_HWCAP2) & HWCAP2_AFP) != 0;
}

static void
install_word(uint32_t word)
{
    static uint32_t cur;
    static bool have;
    if (have && cur == word)
        return;
    mprotect(g_code_page, PAGE, PROT_READ | PROT_WRITE);
    *(volatile uint32_t *)CODE_PC = word;
    mprotect(g_code_page, PAGE, PROT_READ | PROT_EXEC);
    __builtin___clear_cache((char *)CODE_PC, (char *)CODE_PC + 4);
    cur = word;
    have = true;
}

/* Runs the installed word from *in; the data buffer starts as g_pristine and
 * its final contents are copied to mem (if non-NULL).
 */
static void
run_state(const state_t *in, outcome_t *out, uint8_t *mem)
{
    memcpy(g_data, g_pristine, DATA_SIZE);
    memset(out, 0, sizeof(*out));
    g_in = in;
    g_out = out;
    g_runs++;
    if (sigsetjmp(g_jmp, 1) == 0) {
        g_phase = PHASE_ENTER;
        raise(SIGUSR1);
        /* The handler enters the sandbox; we come back via siglongjmp. */
        fprintf(stderr, "ERROR: sandbox entry returned\n");
        abort();
    }
    /* The exit handler ran with the instruction's FP control state. */
    __asm__ volatile("msr fpcr, %0" : : "r"(g_host_fpcr));
    __asm__ volatile("msr fpsr, %0" : : "r"(0ULL));
    if (g_has_dit)
        __asm__ volatile(".inst 0xd503405f" ::: "memory"); /* msr dit, #0 */
    if (mem != NULL)
        memcpy(mem, g_data, DATA_SIZE);
    if (opt_trace) {
        printf("  run: sig %d code %d pc %llx addr %llx%s nzcv %llx->%llx fpcr %x->%x "
               "fpsr %x->%x\n",
               out->sig, out->code, (unsigned long long)out->pc,
               (unsigned long long)out->addr, out->timeout ? " TIMEOUT" : "",
               (unsigned long long)in->nzcv >> 28, (unsigned long long)out->st.nzcv >> 28,
               in->fpcr, out->st.fpcr, in->fpsr, out->st.fpsr);
        for (int i = 0; i < 31; i++) {
            if (in->x[i] != out->st.x[i] || opt_trace > 1)
                printf("    x%d %llx -> %llx\n", i, (unsigned long long)in->x[i],
                       (unsigned long long)out->st.x[i]);
        }
        if (in->sp != out->st.sp || opt_trace > 1)
            printf("    sp %llx -> %llx\n", (unsigned long long)in->sp,
                   (unsigned long long)out->st.sp);
        for (int i = 0; i < 32; i++) {
            if (in->v[i] != out->st.v[i] || opt_trace > 1)
                printf("    v%d %016llx%016llx -> %016llx%016llx\n", i,
                       (unsigned long long)(in->v[i] >> 64), (unsigned long long)in->v[i],
                       (unsigned long long)(out->st.v[i] >> 64),
                       (unsigned long long)out->st.v[i]);
        }
        for (int i = 0; i < DATA_SIZE; i++) {
            if (g_data[i] != g_pristine[i])
                printf("    mem %llx %02x -> %02x\n", (unsigned long long)DATA_BASE + i,
                       g_pristine[i], g_data[i]);
        }
    }
}

/***************************************************************************
 * Input states.
 */
static uint64_t
gen_gpr(rng_t *r)
{
    static const uint64_t special[] = { 0,
                                        1,
                                        ~0ULL,
                                        0x7fffffffffffffffULL,
                                        0x8000000000000000ULL,
                                        0xffffffffULL,
                                        0x80000000ULL,
                                        0x7fffffffULL,
                                        0x100000000ULL,
                                        0xffffffff80000000ULL };
    uint64_t k = rnd_n(r, 20);
    if (k < 8) {
        /* A pointer into the middle of the data buffer. */
        int64_t off = (int64_t)rnd_n(r, 8192) - 4096;
        if (rnd_n(r, 2) == 0)
            off &= ~15LL;
        return DATA_MID + off;
    }
    if (k < 11)
        return rnd_n(r, 2) ? rnd_n(r, 64) : (uint64_t)-(int64_t)rnd_n(r, 64);
    if (k < 17)
        return rnd(r);
    return special[rnd_n(r, sizeof(special) / sizeof(special[0]))];
}

static uint64_t
gen_fp_lane(rng_t *r, int bits)
{
    /* sign, exponent-all-ones, quiet bit, 1.0 per width */
    uint64_t sign = 1ULL << (bits - 1);
    uint64_t one =
        bits == 64 ? 0x3ff0000000000000ULL : (bits == 32 ? 0x3f800000ULL : 0x3c00ULL);
    uint64_t inf =
        bits == 64 ? 0x7ff0000000000000ULL : (bits == 32 ? 0x7f800000ULL : 0x7c00ULL);
    uint64_t qbit = bits == 64 ? (1ULL << 51) : (bits == 32 ? (1ULL << 22) : (1ULL << 9));
    uint64_t mask = bits == 64 ? ~0ULL : (1ULL << bits) - 1;
    switch (rnd_n(r, 10)) {
    case 0: return 0;
    case 1: return sign;
    case 2: return one | (rnd_n(r, 2) ? sign : 0);
    case 3: return inf | (rnd_n(r, 2) ? sign : 0);
    case 4: return inf | qbit | (rnd(r) & (qbit - 1)); /* qNaN */
    case 5: return inf | ((rnd(r) & (qbit - 1)) | 1);  /* sNaN */
    case 6: return rnd(r) & (inf - 1) & ~(inf ^ (inf & (inf - 1))) & (qbit * 2 - 1);
    case 7: return (one + (rnd_n(r, 64) << (bits == 16 ? 10 : 20))) & mask;
    default: return rnd(r) & mask;
    }
}

static __uint128_t
gen_vreg(rng_t *r)
{
    uint64_t k = rnd_n(r, 4);
    if (k == 0)
        return ((__uint128_t)rnd(r) << 64) | rnd(r);
    int bits = k == 1 ? 16 : (k == 2 ? 32 : 64);
    __uint128_t v = 0;
    for (int i = 0; i < 128 / bits; i++)
        v |= (__uint128_t)gen_fp_lane(r, bits) << (i * bits);
    return v;
}

static uint32_t
gen_fpcr(rng_t *r)
{
    if (rnd_n(r, 4) != 0)
        return 0;
    /* AHP DN FZ RMode FZ16, and FEAT_AFP's AH and FIZ.  Not FEAT_AFP's NEP,
     * which makes scalar FP instructions merge into the destination's upper
     * bits: DR's IR describes NEP == 0, the Linux default.
     */
    /* AH and FIZ are RES0 without FEAT_AFP: the signal return cannot load
     * those requested bits, so they must not be attributed to the tested word.
     * Keep the same RNG draw and all supported controls.
     */
    return (uint32_t)rnd(r) & (g_has_afp ? 0x07c80003U : 0x07c80000U);
}

static void
gen_state(rng_t *r, state_t *s)
{
    for (int i = 0; i < 31; i++)
        s->x[i] = gen_gpr(r);
    if (rnd_n(r, 10) != 0)
        s->sp = (DATA_MID + (int64_t)rnd_n(r, 4096) - 2048) & ~15ULL;
    else
        s->sp = gen_gpr(r);
    s->nzcv = (rnd(r) & 0xf) << 28;
    s->fpcr = gen_fpcr(r);
    s->fpsr = rnd_n(r, 4) == 0 ? ((uint32_t)rnd(r) & FPSR_CUMUL) : 0;
    for (int i = 0; i < 32; i++)
        s->v[i] = gen_vreg(r);
}

static void
gen_pristine(uint64_t seed)
{
    rng_t r = rng_for(seed, 0xda7a);
    for (int i = 0; i < DATA_SIZE; i += 8) {
        uint64_t v = rnd(&r);
        memcpy(g_pristine + i, &v, 8);
    }
}

static void
to_mcontext(const state_t *s, dr_mcontext_t *mc)
{
    memset(mc, 0, sizeof(*mc));
    mc->size = sizeof(*mc);
    mc->flags = DR_MC_ALL;
    memcpy(&mc->r0, s->x, sizeof(s->x));
    mc->sp = s->sp;
    mc->pc = (byte *)CODE_PC;
    mc->nzcv = (uint)s->nzcv;
    mc->fpcr = s->fpcr;
    mc->fpsr = s->fpsr;
    for (int i = 0; i < 32; i++)
        memcpy(&mc->simd[i], &s->v[i], 16);
}

/***************************************************************************
 * What DR says about a word.
 */
enum { RC_NONE, RC_GPR, RC_V, RC_NZCV, RC_FPCR, RC_FPSR, RC_OTHER };

/* Maps a DR register to (class, index), returning its size in bytes. */
static int
map_reg(reg_id_t reg, int *cls, int *idx)
{
    *idx = 0;
    if (reg >= DR_REG_X0 && reg <= DR_REG_X30) {
        *cls = RC_GPR;
        *idx = reg - DR_REG_X0;
        return 8;
    }
    if (reg >= DR_REG_W0 && reg <= DR_REG_W30) {
        *cls = RC_GPR;
        *idx = reg - DR_REG_W0;
        return 4;
    }
    if (reg == DR_REG_XSP || reg == DR_REG_WSP) {
        *cls = RC_GPR;
        *idx = 31;
        return reg == DR_REG_XSP ? 8 : 4;
    }
    if (reg == DR_REG_XZR || reg == DR_REG_WZR || reg == DR_REG_NULL) {
        *cls = RC_NONE;
        return 0;
    }
    static const struct {
        reg_id_t first;
        int size;
    } vecs[] = { { DR_REG_Q0, 16 }, { DR_REG_D0, 8 }, { DR_REG_S0, 4 },
                 { DR_REG_H0, 2 },  { DR_REG_B0, 1 }, { DR_REG_Z0, 16 } };
    for (size_t i = 0; i < sizeof(vecs) / sizeof(vecs[0]); i++) {
        if (reg >= vecs[i].first && reg <= vecs[i].first + 31) {
            *cls = RC_V;
            *idx = reg - vecs[i].first;
            return vecs[i].size;
        }
    }
    *cls = reg == DR_REG_NZCV
        ? RC_NZCV
        : (reg == DR_REG_FPCR ? RC_FPCR : (reg == DR_REG_FPSR ? RC_FPSR : RC_OTHER));
    return 8;
}

#define MAX_MEM 4

typedef struct {
    bool valid; /* decoded to a real opcode (not OP_xx/OP_INVALID) */
    int opc;
    char dis[192];
    instr_t instr;
    /* Read/write widths in bytes (0: not accessed); gpr index 31 is SP. */
    uint8_t xr[32], xw[32], vr[32], vw[32];
    bool nzcv_r, nzcv_w, fpcr_r, fpcr_w, fpsr_r, fpsr_w;
    bool reads_mem, writes_mem;
    bool cti, call, cbr, ubr, mbr;
    int nmem; /* memory operands that instr_compute_address_ex() reports */
} drinfo_t;

static void
note_opnd(drinfo_t *d, opnd_t op, bool dst)
{
    for (int i = 0; i < opnd_num_regs_used(op); i++) {
        reg_id_t reg = opnd_get_reg_used(op, i);
        /* Only a destination register operand is written; base and index
         * registers of a destination memory operand are read.
         */
        bool write = dst && opnd_is_reg(op);
        int cls, idx;
        int sz = map_reg(reg, &cls, &idx);
        uint8_t *slot = NULL;
        switch (cls) {
        case RC_GPR: slot = write ? &d->xw[idx] : &d->xr[idx]; break;
        case RC_V: slot = write ? &d->vw[idx] : &d->vr[idx]; break;
        case RC_NZCV: *(write ? &d->nzcv_w : &d->nzcv_r) = true; break;
        case RC_FPCR: *(write ? &d->fpcr_w : &d->fpcr_r) = true; break;
        case RC_FPSR: *(write ? &d->fpsr_w : &d->fpsr_r) = true; break;
        default: break;
        }
        if (slot != NULL && *slot < sz)
            *slot = (uint8_t)sz;
    }
}

static void
dr_describe(uint32_t word, drinfo_t *d)
{
    memset(d, 0, sizeof(*d));
    instr_init(GD, &d->instr);
    byte *next = decode_from_copy(GD, (byte *)&word, (byte *)CODE_PC, &d->instr);
    d->opc = instr_get_opcode(&d->instr);
    d->valid =
        next != NULL && d->opc != OP_xx && d->opc != OP_INVALID && d->opc != OP_UNDECODED;
    instr_disassemble_to_buffer(GD, &d->instr, d->dis, sizeof(d->dis));
    if (!d->valid)
        return;
    for (int i = 0; i < instr_num_srcs(&d->instr); i++)
        note_opnd(d, instr_get_src(&d->instr, i), false);
    for (int i = 0; i < instr_num_dsts(&d->instr); i++)
        note_opnd(d, instr_get_dst(&d->instr, i), true);
    uint fl = instr_get_arith_flags(&d->instr, DR_QUERY_INCLUDE_ALL);
    if (fl & EFLAGS_READ_NZCV)
        d->nzcv_r = true;
    if (fl & EFLAGS_WRITE_NZCV)
        d->nzcv_w = true;
    d->reads_mem = instr_reads_memory(&d->instr);
    d->writes_mem = instr_writes_memory(&d->instr);
    d->cti = instr_is_cti(&d->instr);
    d->call = instr_is_call(&d->instr);
    d->cbr = instr_is_cbr(&d->instr);
    d->ubr = instr_is_ubr(&d->instr);
    d->mbr = instr_is_mbr(&d->instr);
}

static void
dr_free(drinfo_t *d)
{
    instr_free(GD, &d->instr);
}

typedef struct {
    uint64_t addr;
    uint64_t size; /* 0: unknown (e.g. OPSZ_sys for DC ZVA) */
    bool write;
    opnd_t op;
} memref_t;

/* DR's memory accesses for state s. */
static int
dr_memrefs(drinfo_t *d, const state_t *s, memref_t *m)
{
    dr_mcontext_t mc;
    to_mcontext(s, &mc);
    int n = 0;
    for (uint i = 0; n < MAX_MEM; i++) {
        app_pc addr;
        bool write;
        uint pos;
        if (!instr_compute_address_ex_pos(&d->instr, &mc, i, &addr, &write, &pos))
            break;
        m[n].addr = (uint64_t)addr;
        m[n].write = write;
        m[n].op = write ? instr_get_dst(&d->instr, pos) : instr_get_src(&d->instr, pos);
        /* OPSZ_sys (DC ZVA and other cache operations): the size is a
         * property of the CPU, not the encoding.
         */
        m[n].size = opnd_get_size(m[n].op) == OPSZ_sys
            ? 0
            : opnd_size_in_bytes(opnd_get_size(m[n].op));
        n++;
    }
    return n;
}

/***************************************************************************
 * Failure reporting, allowlist and known issues.
 */
typedef enum {
    P_VALIDITY_UNDEF, /* DR decodes it, the CPU says UNDEFINED */
    P_VALIDITY_EXEC,  /* DR says invalid, the CPU executes it */
    P_REG_WRITE,      /* CPU changed a register DR does not list as written */
    P_WRITE_WIDTH,    /* bits above DR's destination size not zeroed */
    P_REG_READ,       /* output depends on a register DR says is not read */
    P_FPCR_READ,      /* output depends on FPCR, DR says it is not read */
    P_FPSR_WRITE,     /* CPU set FPSR cumulative bits, DR does not list FPSR */
    P_MEM_WRITE,      /* CPU wrote bytes outside DR's write ranges */
    P_MEM_FAULT,      /* faulting address outside DR's ranges */
    P_MEM_SIZE,       /* guard-page placement contradicts DR's size */
    P_WRITEBACK,      /* base register update != DR's operands */
    P_BRANCH,         /* control flow vs DR's cti/target/condition */
    P_PCREL,          /* ADR/ADRP/literal values vs DR's pc-relative operands */
    P_CONSTANT,       /* instr_is_mov_constant() vs the value produced */
    P_REENCODE,       /* DR's re-encoding behaves differently */
    P_NOP,            /* instr_is_nop() but state changed */
    P_PHANTOM_WRITE,  /* DR lists a destination the CPU passes through */
    P_MEM_READ,       /* result depends on memory outside DR's read ranges */
    NUM_PROPS
} prop_t;

static const char *const prop_names[NUM_PROPS] = {
    "validity-undef", "validity-exec", "reg-write",  "write-width",
    "reg-read",       "fpcr-read",     "fpsr-write", "mem-write",
    "mem-fault",      "mem-size",      "writeback",  "branch",
    "pcrel",          "constant",      "reencode",   "nop",
    "phantom-write",  "mem-read",
};

/* Expected differences: not DR bugs (allowed) or DR bugs not yet fixed
 * (known).  A failure matches an entry when the property matches (or the
 * entry is for any property), (word & mask) == value, DR's opcode is opc
 * (unless OP_INVALID), and match (if set) returns true.
 */
#define ANY_PROP NUM_PROPS

typedef struct {
    prop_t prop;
    uint32_t mask, value;
    int opc;
    bool (*match)(uint32_t word, drinfo_t *d);
    bool known; /* a DR bug rather than expected noise */
    const char *reason;
} expect_t;

static int
gpr_index(opnd_t op)
{
    int cls, idx;
    if (!opnd_is_reg(op))
        return -1;
    map_reg(opnd_get_reg(op), &cls, &idx);
    return cls == RC_GPR ? idx : -1;
}

/* Whether DR's operands show base-register writeback: the base is the last
 * destination and the second-last source, followed by the increment.
 */
static int
writeback_base(drinfo_t *d)
{
    instr_t *instr = &d->instr;
    int ns = instr_num_srcs(instr), nd = instr_num_dsts(instr);
    if (ns < 2 || nd < 1)
        return -1;
    opnd_t base = instr_get_src(instr, ns - 2), last = instr_get_dst(instr, nd - 1);
    if (!opnd_is_reg(base) || !opnd_is_reg(last) ||
        opnd_get_reg(base) != opnd_get_reg(last))
        return -1;
    return gpr_index(base);
}

/* CONSTRAINED UNPREDICTABLE: writeback to a base that is also transferred. */
static bool
cu_writeback_overlap(uint32_t word, drinfo_t *d)
{
    int b = writeback_base(d);
    if (b < 0 || b == 31)
        return false;
    instr_t *instr = &d->instr;
    for (int i = 0; i < instr_num_srcs(instr) - 2; i++) {
        if (gpr_index(instr_get_src(instr, i)) == b)
            return true;
    }
    for (int i = 0; i < instr_num_dsts(instr) - 1; i++) {
        if (gpr_index(instr_get_dst(instr, i)) == b)
            return true;
    }
    return false;
}

/* CONSTRAINED UNPREDICTABLE: a load pair (or exclusive pair) to one register. */
static bool
cu_load_pair_same(uint32_t word, drinfo_t *d)
{
    instr_t *instr = &d->instr;
    if (!instr_reads_memory(instr) || instr_num_dsts(instr) < 2)
        return false;
    opnd_t a = instr_get_dst(instr, 0), b = instr_get_dst(instr, 1);
    if (!opnd_is_reg(a) || !opnd_is_reg(b))
        return false;
    if (opnd_get_reg(a) == opnd_get_reg(b))
        return true; /* includes XZR, XZR */
    int ca, ia, cb, ib;
    map_reg(opnd_get_reg(a), &ca, &ia);
    map_reg(opnd_get_reg(b), &cb, &ib);
    return ca == cb && ia == ib && ca != RC_NONE;
}

/* CONSTRAINED UNPREDICTABLE: a store-exclusive status register that is also
 * the data or base register.
 */
static bool
cu_stx_status(uint32_t word, drinfo_t *d)
{
    if (!instr_is_exclusive_store(&d->instr))
        return false;
    int st = -1;
    for (int i = 0; i < instr_num_dsts(&d->instr); i++) {
        if (gpr_index(instr_get_dst(&d->instr, i)) >= 0)
            st = gpr_index(instr_get_dst(&d->instr, i));
    }
    if (st < 0)
        return false;
    for (int i = 0; i < instr_num_srcs(&d->instr); i++) {
        opnd_t op = instr_get_src(&d->instr, i);
        if (gpr_index(op) == st)
            return true;
        if (opnd_is_base_disp(op) && opnd_get_base(op) != DR_REG_XSP) {
            int cls, idx;
            map_reg(opnd_get_base(op), &cls, &idx);
            if (cls == RC_GPR && idx == st)
                return true;
        }
    }
    return false;
}

/* MSR (immediate) to a PSTATE field EL0 cannot write (DAIF, SPSel, PAN...);
 * not CFINV, XAFLAG or AXFLAG (op1 = 0, op2 <= 2), which are always allowed.
 */
static bool
msr_imm_privileged(uint32_t word, drinfo_t *d)
{
    return !(((word >> 16) & 7) == 0 && ((word >> 5) & 7) <= 2);
}

/* System register and system instruction accesses (op0 != 0). */
static bool
sys_op0_nonzero(uint32_t word, drinfo_t *d)
{
    return ((word >> 19) & 3) != 0;
}

/* Exception-generating and exception-return instructions EL0 cannot use. */
static bool
el0_undefined(uint32_t word, drinfo_t *d)
{
    switch (d->opc) {
    case OP_dcps1:
    case OP_dcps2:
    case OP_dcps3:
    case OP_drps:
    case OP_eret:
    case OP_eretaa:
    case OP_eretab:
    case OP_hlt:
    case OP_hvc:
    case OP_smc: return true;
    default: return false;
    }
}

/* LDRAA/LDRAB: the address is the authenticated base, which DR does not model. */
static bool
pac_load(uint32_t word, drinfo_t *d)
{
    return d->opc == OP_ldraa || d->opc == OP_ldrab;
}

static const expect_t expected[] = {
    /* System register and system instruction accesses (op0 != 0) trap or not
     * depending on EL0 access controls the kernel sets; DR decodes them all.
     */
    { P_VALIDITY_UNDEF, 0xffc00000, 0xd5000000, OP_INVALID, sys_op0_nonzero, false,
      "EL0 access to a system register/instruction is trapped by the OS" },
    { P_VALIDITY_UNDEF, 0, 0, OP_INVALID, el0_undefined, false,
      "exception generation/return (HVC, SMC, HLT, DCPS, ERET, DRPS) at EL0" },
    /* MSR (immediate) to PSTATE fields not writable at EL0 (DAIF, SPSel...). */
    { P_VALIDITY_UNDEF, 0xfff8f01f, 0xd500401f, OP_INVALID, msr_imm_privileged, false,
      "MSR (immediate) PSTATE field not accessible at EL0" },
    /* UDF is permanently undefined by design. */
    { P_VALIDITY_UNDEF, 0xffff0000, 0x00000000, OP_udf, NULL, false,
      "UDF is permanently undefined" },
    /* CONSTRAINED UNPREDICTABLE encodings: DR decodes them like the
     * architecture's disassembly does; the CPU may UNDEF or do anything
     * the architecture allows.
     */
    { P_MEM_FAULT, 0, 0, OP_INVALID, pac_load, false,
      "LDRAA/LDRAB: a failed authentication changes the address DR computes" },
    { ANY_PROP, 0, 0, OP_INVALID, cu_writeback_overlap, false,
      "CONSTRAINED UNPREDICTABLE: writeback base is also a transfer register" },
    { ANY_PROP, 0, 0, OP_INVALID, cu_load_pair_same, false,
      "CONSTRAINED UNPREDICTABLE: load pair to the same register" },
    { ANY_PROP, 0, 0, OP_INVALID, cu_stx_status, false,
      "CONSTRAINED UNPREDICTABLE: store-exclusive status overlaps data/base" },
};
#define NUM_EXPECTED (sizeof(expected) / sizeof(expected[0]))

/* Known DR bugs that are not fixed yet.  Each entry names an encoding
 * pattern in the codec_*.txt syntax (0/1 fixed, anything else free) and DR's
 * opcode name, so a new encoding showing the same symptom is still a failure.
 * -strict (or A64_PROPTEST_STRICT=1) reports them as failures.
 */
enum {
    KI_FPCR,
    KI_FPSR,
    NUM_KI,
};

static const char *const known_issue_reasons[NUM_KI] = {
    "FPCR is not an implicit source of FP instructions in DR's IR",
    "FPSR is not an implicit destination of FP instructions in DR's IR",
};

typedef struct {
    int issue;
    uint32_t props; /* bit per prop_t */
    const char *pattern;
    const char *opname;
    uint32_t mask, value; /* from pattern, at startup */
} known_t;

#define PB(p) (1U << (p))

static known_t known[] = {
    { KI_FPCR, PB(P_FPCR_READ), NULL, NULL },
    { KI_FPSR, PB(P_FPSR_WRITE), NULL, NULL },
};
#define NUM_KNOWN (sizeof(known) / sizeof(known[0]))

static void
init_known(void)
{
    for (size_t i = 0; i < NUM_KNOWN; i++) {
        known[i].mask = known[i].value = 0;
        for (int b = 0; known[i].pattern != NULL && b < 32; b++) {
            char c = known[i].pattern[b];
            if (c == '0' || c == '1') {
                known[i].mask |= 1U << (31 - b);
                known[i].value |= (uint32_t)(c == '1') << (31 - b);
            }
        }
    }
}

static int opt_verbose;
static int opt_max_print = 10;
static bool opt_props[NUM_PROPS];
static uint64_t g_case_seed;
static uint32_t g_case_word;

static uint64_t fail_count[NUM_PROPS], allowed_count[NUM_PROPS], known_count[NUM_PROPS];
static uint64_t pass_count[NUM_PROPS];
static uint64_t expect_hits[NUM_EXPECTED];
static uint64_t known_hits[NUM_KI];
static bool opt_strict;
static uint64_t g_cases, g_skipped_unsafe, g_undef_agree, g_nofeature, g_nondet;
static uint64_t g_executed, g_unclassified;

/* Failures (not allowed/known) per property and opcode, for the summary. */
static uint32_t *g_opc_fails[NUM_PROPS];
static uint32_t *g_opc_first[NUM_PROPS];

/* Per-case: report each property at most once. */
static bool g_reported[NUM_PROPS];

static void
report(prop_t p, drinfo_t *d, int state, const char *fmt, ...)
{
    if (g_reported[p])
        return;
    g_reported[p] = true;
    const expect_t *e = NULL;
    for (size_t i = 0; i < NUM_EXPECTED; i++) {
        if ((expected[i].prop == p || expected[i].prop == ANY_PROP) &&
            (g_case_word & expected[i].mask) == expected[i].value &&
            (expected[i].opc == OP_INVALID || expected[i].opc == d->opc) &&
            (expected[i].match == NULL || expected[i].match(g_case_word, d))) {
            e = &expected[i];
            expect_hits[i]++;
            break;
        }
    }
    const known_t *k = NULL;
    for (size_t i = 0; e == NULL && !opt_strict && i < NUM_KNOWN; i++) {
        if ((known[i].props & PB(p)) != 0 &&
            (g_case_word & known[i].mask) == known[i].value &&
            (known[i].opname == NULL ||
             strcmp(known[i].opname, decode_opcode_name(d->opc)) == 0)) {
            k = &known[i];
            known_hits[k->issue]++;
            break;
        }
    }
    const char *tag = "FAIL";
    if ((e != NULL && e->known) || k != NULL) {
        known_count[p]++;
        tag = "KNOWN";
    } else if (e != NULL) {
        allowed_count[p]++;
        tag = "ALLOWED";
    } else {
        fail_count[p]++;
        if (d->opc >= 0 && d->opc < OP_LAST) {
            if (g_opc_fails[p][d->opc]++ == 0)
                g_opc_first[p][d->opc] = g_case_word;
        }
    }
    bool print = e == NULL && k == NULL ? fail_count[p] <= (uint64_t)opt_max_print
                                        : opt_verbose > 0;
    if (!print)
        return;
    char msg[512];
    va_list ap;
    va_start(ap, fmt);
    vsnprintf(msg, sizeof(msg), fmt, ap);
    va_end(ap);
    printf("%s %s word=0x%08x seed=0x%llx state=%d \"%s\": %s\n", tag, prop_names[p],
           g_case_word, (unsigned long long)g_case_seed, state, d->dis, msg);
    fflush(stdout);
}

/***************************************************************************
 * Words the sandbox must not run: they would escape it or disturb the host.
 */
static const char *
unsafe_reason(uint32_t w)
{
    if ((w & 0xffe0001f) == 0xd4000001)
        return "svc";
    if ((w & 0xfff00000) == 0xd5100000) {
        /* MSR (register): only NZCV, FPCR and FPSR are host-safe. */
        uint32_t sr = (w >> 5) & 0xffff;
        if (sr != 0xda10 && sr != 0xda20 && sr != 0xda21)
            return "msr";
    }
    /* MSR (immediate) SVCRSM/SVCRZA/SVCRSMZA: smstart/smstop. */
    if ((w & 0xfff8f01f) == 0xd500401f && ((w >> 16) & 7) == 3 && ((w >> 5) & 7) == 3)
        return "smstart";
    /* Branches to themselves would spin until the watchdog fires. */
    if ((w & 0x7c000000) == 0x14000000 && (w & 0x03ffffff) == 0)
        return "self-branch";
    if ((w & 0xff000000) == 0x54000000 && (w & 0x00ffffe0) == 0)
        return "self-branch";
    if ((w & 0x7e000000) == 0x34000000 && (w & 0x00ffffe0) == 0)
        return "self-branch";
    if ((w & 0x7e000000) == 0x36000000 && (w & 0x0007ffe0) == 0)
        return "self-branch";
    return NULL;
}

/* SVE and SME encodings: neither runs here (no SVE, not in streaming mode). */
static bool
sve_or_sme_space(uint32_t w)
{
    uint32_t op0 = (w >> 25) & 0xf;
    return op0 == 0x2 || ((w & 0x80000000) != 0 && op0 == 0x0);
}

/***************************************************************************
 * Encoding classes from the test vectors.
 *
 * A class is a template word and the mask of bits that can each be flipped
 * without changing DR's opcode (its operand fields, as DR sees them).
 * Whether a class runs here (its template is not UNDEFINED) tells which
 * opcodes' features this CPU has.
 */
typedef struct {
    uint32_t word, mask;
    int opc;
    int8_t runs; /* -1 unknown, 0 UNDEFINED, 1 executes */
    int8_t feat; /* this CPU has its ISA feature: -1 unknown, 0 no, 1 yes */
} class_t;

static class_t *g_classes;
static int g_nclasses;

static int
class_cmp(const void *a, const void *b)
{
    const class_t *x = a, *y = b;
    uint32_t kx = x->word & ~x->mask, ky = y->word & ~y->mask;
    if (x->opc != y->opc)
        return x->opc < y->opc ? -1 : 1;
    if (x->mask != y->mask)
        return x->mask < y->mask ? -1 : 1;
    if (kx != ky)
        return kx < ky ? -1 : 1;
    return 0;
}

static int
dr_opcode(uint32_t w)
{
    instr_t in;
    instr_init(GD, &in);
    decode_from_copy(GD, (byte *)&w, (byte *)CODE_PC, &in);
    int opc = instr_get_opcode(&in);
    instr_free(GD, &in);
    return opc;
}

static void
load_vectors(int nfiles, char **files)
{
    int cap = 1024;
    g_classes = malloc(cap * sizeof(class_t));
    for (int f = 0; f < nfiles; f++) {
        /* SVE needs a CPU this harness does not target. */
        if (strstr(files[f], "-sve") != NULL)
            continue;
        FILE *fp = fopen(files[f], "r");
        if (fp == NULL) {
            fprintf(stderr, "ERROR: cannot open %s\n", files[f]);
            exit(2);
        }
        char line[1024];
        while (fgets(line, sizeof(line), fp) != NULL) {
            char *end;
            if (line[0] == '#' || line[0] == '\n')
                continue;
            unsigned long w = strtoul(line, &end, 16);
            if (end != line + 8 || *end != ' ')
                continue;
            uint32_t word = (uint32_t)w;
            int opc = dr_opcode(word);
            if (opc == OP_xx || opc == OP_INVALID)
                continue;
            uint32_t mask = 0;
            for (int b = 0; b < 32; b++) {
                if (dr_opcode(word ^ (1U << b)) == opc)
                    mask |= 1U << b;
            }
            if (g_nclasses == cap) {
                cap *= 2;
                g_classes = realloc(g_classes, cap * sizeof(class_t));
            }
            g_classes[g_nclasses++] = (class_t) { word, mask, opc, -1, -1 };
        }
        fclose(fp);
    }
    /* One class per (opcode, mask, fixed bits). */
    qsort(g_classes, g_nclasses, sizeof(class_t), class_cmp);
    int n = 0;
    for (int i = 0; i < g_nclasses; i++) {
        if (n > 0 && class_cmp(&g_classes[n - 1], &g_classes[i]) == 0)
            continue;
        g_classes[n++] = g_classes[i];
    }
    g_nclasses = n;
}

/* Whether this CPU has the ISA feature DR says the instruction needs: -1 if
 * DR's feature has no Linux hwcap (or none for EL0 use), 0 no, 1 yes.
 */
static int
dr_feature_present(uint32_t word, instr_t *instr)
{
    static const struct {
        const char *name;
        int hwcap; /* 1: AT_HWCAP, 2: AT_HWCAP2, 0: always, -1: never */
        uint64_t bit;
    } map[] = {
        { "BASE", 0, 0 },
        { "BF16", 2, 1ULL << 14 },
        { "BTI", 0, 0 },
        { "DPB", 1, 1ULL << 16 },
        { "DPB2", 2, 1ULL << 0 },
        { "DotProd", 1, 1ULL << 20 },
        { "FHM", 1, 1ULL << 23 },
        { "FP16", 1, 1ULL << 9 },
        { "FRINTTS", 2, 1ULL << 8 },
        { "FlagM", 1, 1ULL << 27 },
        { "FlagM2", 2, 1ULL << 7 },
        { "I8MM", 2, 1ULL << 13 },
        { "JSCVT", 1, 1ULL << 13 },
        { "LRCPC", 1, 1ULL << 15 },
        { "LRCPC2", 1, 1ULL << 26 },
        { "LSE", 1, 1ULL << 8 },
        { "MTE", 2, 1ULL << 18 },
        { "MTE2", 2, 1ULL << 18 },
        { "PAUTH", 1, 1ULL << 30 },
        { "RDM", 1, 1ULL << 12 },
        { "SHA3", 1, 1ULL << 17 },
        { "SHA512", 1, 1ULL << 21 },
        { "SM3", 1, 1ULL << 18 },
        { "SM4", 1, 1ULL << 19 },
        { "WFxT", 2, 1ULL << 31 },
        { "SVE", -1, 0 },
        { "SVE2", -1, 0 },
        { "SVEAES", -1, 0 },
        { "SVEBitPerm", -1, 0 },
        { "SVESHA3", -1, 0 },
        { "SVESM4", -1, 0 },
        { "F64MM", -1, 0 },
        { "SB", 1, 1ULL << 29 },
    };
    if (sve_or_sme_space(word))
        return 0;
    const char *name =
        instr_get_isa_feature_name(instr_get_isa_feature((byte *)CODE_PC, instr));
    for (size_t i = 0; i < sizeof(map) / sizeof(map[0]); i++) {
        if (strcmp(name, map[i].name) != 0)
            continue;
        if (map[i].hwcap <= 0)
            return map[i].hwcap == 0;
        uint64_t caps = getauxval(map[i].hwcap == 1 ? AT_HWCAP : AT_HWCAP2);
        return (caps & map[i].bit) != 0;
    }
    return -1;
}

/* Runs each class's template once from a benign state. */
static void
probe_classes(void)
{
    state_t s;
    rng_t r = rng_for(0x9e0be, 0);
    gen_state(&r, &s);
    for (int i = 0; i < 31; i++)
        s.x[i] = DATA_MID + 64 * i;
    s.sp = DATA_MID;
    outcome_t o;
    for (int i = 0; i < g_nclasses; i++) {
        class_t *c = &g_classes[i];
        instr_t in;
        instr_init(GD, &in);
        decode_from_copy(GD, (byte *)&c->word, (byte *)CODE_PC, &in);
        c->feat = (int8_t)dr_feature_present(c->word, &in);
        instr_free(GD, &in);
        if (unsafe_reason(c->word) != NULL || sve_or_sme_space(c->word)) {
            c->runs = 0;
            c->feat = 0;
            continue;
        }
        install_word(c->word);
        run_state(&s, &o, NULL);
        c->runs = undefined(&o) ? 0 : 1;
    }
}

/* Classes this CPU should run: DR's ISA feature is present, or, for features
 * without a hwcap, the template ran.
 */
static bool
class_usable(const class_t *c)
{
    return c->feat == 1 || (c->feat < 0 && c->runs == 1);
}

/* Whether a word DR decodes should run here: -1 unknown, 0 its feature is
 * absent, 1 present.
 */
static int
feature_present(uint32_t w, drinfo_t *d)
{
    int fp = dr_feature_present(w, &d->instr);
    if (fp >= 0)
        return fp;
    int res = -1;
    for (int i = 0; i < g_nclasses; i++) {
        class_t *c = &g_classes[i];
        if (c->opc != d->opc || (w & ~c->mask) != (c->word & ~c->mask))
            continue;
        if (c->runs == 1)
            return 1;
        res = 0;
    }
    return res;
}

/***************************************************************************
 * Property checks.
 */
static uint64_t
gpr_val(const state_t *s, int i)
{
    return i == 31 ? s->sp : s->x[i];
}

static uint64_t *
gpr_ptr(state_t *s, int i)
{
    return i == 31 ? &s->sp : &s->x[i];
}

static const char *
gpr_name(int i)
{
    static char buf[8][8];
    static int k;
    char *b = buf[k++ & 7];
    if (i == 31)
        snprintf(b, 8, "sp");
    else
        snprintf(b, 8, "x%d", i);
    return b;
}

static __uint128_t
width_mask(int bytes)
{
    return bytes >= 16 ? ~(__uint128_t)0 : (((__uint128_t)1 << (bytes * 8)) - 1);
}

static bool
same_outcome(const outcome_t *a, const outcome_t *b, const uint8_t *ma, const uint8_t *mb)
{
    if (a->timeout || b->timeout)
        return a->timeout == b->timeout;
    if (a->sig != b->sig || a->pc != b->pc || a->code != b->code)
        return false;
    if (a->pc == CODE_PC && a->addr != b->addr)
        return false;
    if (memcmp(&a->st, &b->st, sizeof(a->st)) != 0)
        return false;
    return ma == NULL || mb == NULL || memcmp(ma, mb, DATA_SIZE) == 0;
}

static void
describe_diff(const outcome_t *a, const outcome_t *b, const uint8_t *ma,
              const uint8_t *mb, char *buf, size_t sz)
{
    size_t n = 0;
#define ADD(...)                                         \
    do {                                                 \
        if (n < sz)                                      \
            n += snprintf(buf + n, sz - n, __VA_ARGS__); \
    } while (0)
    if (a->sig != b->sig || a->pc != b->pc || a->addr != b->addr ||
        a->timeout != b->timeout)
        ADD("end sig%d@%llx(%llx) vs sig%d@%llx(%llx) ", a->sig,
            (unsigned long long)a->pc, (unsigned long long)a->addr, b->sig,
            (unsigned long long)b->pc, (unsigned long long)b->addr);
    for (int i = 0; i < 32; i++) {
        if (gpr_val(&a->st, i) != gpr_val(&b->st, i))
            ADD("%s %llx vs %llx ", gpr_name(i), (unsigned long long)gpr_val(&a->st, i),
                (unsigned long long)gpr_val(&b->st, i));
    }
    for (int i = 0; i < 32; i++) {
        if (a->st.v[i] != b->st.v[i])
            ADD("v%d %016llx%016llx vs %016llx%016llx ", i,
                (unsigned long long)(a->st.v[i] >> 64), (unsigned long long)a->st.v[i],
                (unsigned long long)(b->st.v[i] >> 64), (unsigned long long)b->st.v[i]);
    }
    if (a->st.nzcv != b->st.nzcv)
        ADD("nzcv %llx vs %llx ", (unsigned long long)a->st.nzcv,
            (unsigned long long)b->st.nzcv);
    if (a->st.fpsr != b->st.fpsr)
        ADD("fpsr %x vs %x ", a->st.fpsr, b->st.fpsr);
    if (ma != NULL && mb != NULL) {
        for (int i = 0; i < DATA_SIZE; i++) {
            if (ma[i] != mb[i]) {
                ADD("mem[%llx] %02x vs %02x ", (unsigned long long)(DATA_BASE + i), ma[i],
                    mb[i]);
                break;
            }
        }
    }
#undef ADD
}

/* Registers the CPU changed must be DR destinations, and destinations narrower
 * than the register must leave the bits above them zero.
 */
static void
check_writes(drinfo_t *d, int k, const state_t *in, const outcome_t *o)
{
    if (!completed(o))
        return;
    const state_t *out = &o->st;
    for (int i = 0; i < 32; i++) {
        uint64_t a = gpr_val(in, i), b = gpr_val(out, i);
        if (a != b && d->xw[i] == 0) {
            report(P_REG_WRITE, d, k, "%s changed %llx -> %llx but is not a destination",
                   gpr_name(i), (unsigned long long)a, (unsigned long long)b);
        }
        if (d->xw[i] == 4 && (b >> 32) != 0) {
            report(P_WRITE_WIDTH, d, k, "%s = %llx: destination is 32-bit", gpr_name(i),
                   (unsigned long long)b);
        }
    }
    for (int i = 0; i < 32; i++) {
        __uint128_t a = in->v[i], b = out->v[i];
        if (a != b && d->vw[i] == 0) {
            report(P_REG_WRITE, d, k, "v%d changed but is not a destination", i);
        }
        if (d->vw[i] != 0 && d->vw[i] < 16 && (b & ~width_mask(d->vw[i])) != 0) {
            report(P_WRITE_WIDTH, d, k,
                   "v%d = %016llx%016llx: destination is %d bytes, upper bits not zeroed",
                   i, (unsigned long long)(b >> 64), (unsigned long long)b, d->vw[i]);
        }
    }
    if (in->nzcv != out->nzcv && !d->nzcv_w) {
        report(P_REG_WRITE, d, k, "nzcv changed %llx -> %llx but DR says not written",
               (unsigned long long)in->nzcv, (unsigned long long)out->nzcv);
    }
    if (in->fpcr != out->fpcr && !d->fpcr_w) {
        report(P_REG_WRITE, d, k, "fpcr changed %x -> %x but DR says not written",
               in->fpcr, out->fpcr);
    }
    if ((out->fpsr & ~in->fpsr & FPSR_CUMUL) != 0 && !d->fpsr_w) {
        report(P_FPSR_WRITE, d, k, "fpsr %x -> %x", in->fpsr, out->fpsr);
    }
    if (out->fpsr != in->fpsr && (out->fpsr & ~FPSR_CUMUL) != (in->fpsr & ~FPSR_CUMUL) &&
        !d->fpsr_w) {
        report(P_REG_WRITE, d, k, "fpsr non-cumulative bits changed %x -> %x", in->fpsr,
               out->fpsr);
    }
    pass_count[P_REG_WRITE]++;
}

/* The control flow the CPU took vs DR's cti classification and target. */
static void
check_branch(drinfo_t *d, int k, const state_t *in, const outcome_t *o)
{
    if (!completed(o))
        return;
    uint64_t next = o->pc;
    if (!d->cti) {
        if (next != CODE_PC + 4)
            report(P_BRANCH, d, k, "not a cti per DR but control went to %llx",
                   (unsigned long long)next);
        return;
    }
    if (d->call && o->st.x[30] != CODE_PC + 4) {
        report(P_BRANCH, d, k, "call per DR but x30 = %llx",
               (unsigned long long)o->st.x[30]);
    }
    opnd_t t = instr_get_target(&d->instr);
    uint64_t target;
    if (opnd_is_pc(t))
        target = (uint64_t)opnd_get_pc(t);
    else if (opnd_is_reg(t)) {
        int cls, idx;
        map_reg(opnd_get_reg(t), &cls, &idx);
        if (cls != RC_GPR)
            return;
        /* Pointer-authenticated branches: the target is the authenticated
         * value, which DR does not model.
         */
        int opc = d->opc;
        if (opc == OP_braa || opc == OP_brab || opc == OP_blraa || opc == OP_blrab ||
            opc == OP_braaz || opc == OP_brabz || opc == OP_blraaz || opc == OP_blrabz ||
            opc == OP_retaa || opc == OP_retab)
            return;
        target = gpr_val(in, idx);
    } else
        return;
    /* Linux enables top-byte-ignore for the lower (bit 55 clear) address range
     * only, and branches there clear the target's top byte
     * (AArch64.BranchAddr()); DR reports the register value.
     */
    if ((target & (1ULL << 55)) == 0)
        target &= 0x00ffffffffffffffULL;
    bool taken = next != CODE_PC + 4;
    if (taken && next != target) {
        report(P_BRANCH, d, k, "went to %llx, DR target %llx", (unsigned long long)next,
               (unsigned long long)target);
    }
    if (target == CODE_PC + 4)
        return; /* Taken and not taken look the same. */
    if (d->ubr || d->mbr || d->call) {
        if (!taken)
            report(P_BRANCH, d, k, "unconditional per DR but fell through");
        return;
    }
    if (!d->cbr)
        return;
    /* Conditional: evaluate the condition from DR's IR. */
    bool want;
    dr_mcontext_t mc;
    to_mcontext(in, &mc);
    int opc = d->opc;
    if (opc == OP_cbz || opc == OP_cbnz || opc == OP_tbz || opc == OP_tbnz) {
        opnd_t r = instr_get_src(&d->instr, 1);
        if (!opnd_is_reg(r))
            return;
        uint64_t v = reg_get_value(reg_to_pointer_sized(opnd_get_reg(r)), &mc);
        if (reg_get_size(opnd_get_reg(r)) == OPSZ_4)
            v &= 0xffffffffULL;
        if (opc == OP_cbz || opc == OP_cbnz)
            want = (v == 0) == (opc == OP_cbz);
        else {
            opnd_t b = instr_get_src(&d->instr, 2);
            if (!opnd_is_immed_int(b))
                return;
            bool bit = (v >> (opnd_get_immed_int(b) & 63)) & 1;
            want = bit == (opc == OP_tbnz);
        }
    } else {
        dr_pred_trigger_t trig = instr_predicate_triggered(&d->instr, &mc);
        if (trig != DR_PRED_TRIGGER_MATCH && trig != DR_PRED_TRIGGER_MISMATCH) {
            report(P_BRANCH, d, k, "instr_predicate_triggered() = %d", trig);
            return;
        }
        want = trig == DR_PRED_TRIGGER_MATCH;
    }
    if (want != taken) {
        report(P_BRANCH, d, k, "nzcv=%llx: DR says %s, CPU %s",
               (unsigned long long)(in->nzcv >> 28), want ? "taken" : "not taken",
               taken ? "took it" : "fell through");
    }
    pass_count[P_BRANCH]++;
}

/* ADR/ADRP results and literal loads vs DR's pc-relative operands. */
static void
check_pcrel(drinfo_t *d, int k, const state_t *in, const outcome_t *o)
{
    if (!completed(o))
        return;
    if (d->opc == OP_adr || d->opc == OP_adrp) {
        opnd_t src = instr_get_src(&d->instr, 0);
        opnd_t dst = instr_get_dst(&d->instr, 0);
        uint64_t want = opnd_is_rel_addr(src) ? (uint64_t)opnd_get_addr(src)
                                              : (uint64_t)opnd_get_pc(src);
        int cls, idx;
        map_reg(opnd_get_reg(dst), &cls, &idx);
        if (cls == RC_GPR && o->st.x[idx] != want) {
            report(P_PCREL, d, k, "%s = %llx, DR says %llx", gpr_name(idx),
                   (unsigned long long)o->st.x[idx], (unsigned long long)want);
        }
        pass_count[P_PCREL]++;
        return;
    }
    /* Literal loads: the first destination's bytes must be the memory at DR's
     * address (the landing pads hold position-dependent words).
     */
    for (int i = 0; i < instr_num_srcs(&d->instr); i++) {
        opnd_t src = instr_get_src(&d->instr, i);
        if (!opnd_is_rel_addr(src) || instr_num_dsts(&d->instr) == 0)
            continue;
        uint64_t a = (uint64_t)opnd_get_addr(src);
        size_t sz = opnd_size_in_bytes(opnd_get_size(src));
        opnd_t dst = instr_get_dst(&d->instr, 0);
        if (!opnd_is_reg(dst) || a < CODE_PC - CODE_HALF || a + sz > CODE_PC + CODE_HALF)
            return;
        int cls, idx;
        int rsz = map_reg(opnd_get_reg(dst), &cls, &idx);
        if (sz > (size_t)rsz)
            sz = rsz;
        uint8_t got[16];
        if (cls == RC_GPR)
            memcpy(got, &o->st.x[idx], 8);
        else if (cls == RC_V)
            memcpy(got, &o->st.v[idx], 16);
        else
            return;
        /* LDRSW sign-extends: compare the loaded bytes only. */
        if (memcmp(got, (void *)a, sz) != 0) {
            report(P_PCREL, d, k,
                   "loaded value does not match memory at DR's address %llx",
                   (unsigned long long)a);
        }
        pass_count[P_PCREL]++;
        return;
    }
}

/* instr_is_mov_constant() vs the value the CPU produced. */
static void
check_constant(drinfo_t *d, int k, const outcome_t *o)
{
    ptr_int_t val;
    if (!completed(o) || !instr_is_mov_constant(&d->instr, &val))
        return;
    opnd_t dst = instr_get_dst(&d->instr, 0);
    int cls, idx;
    int sz = map_reg(opnd_get_reg(dst), &cls, &idx);
    if (cls != RC_GPR)
        return;
    uint64_t got = gpr_val(&o->st, idx), want = (uint64_t)val;
    if (sz == 4) {
        want &= 0xffffffffULL;
    }
    if (got != want) {
        report(P_CONSTANT, d, k, "instr_is_mov_constant() = %llx, CPU wrote %llx",
               (unsigned long long)want, (unsigned long long)got);
    }
    pass_count[P_CONSTANT]++;
}

static void
check_nop(drinfo_t *d, int k, const state_t *in, const outcome_t *o, const uint8_t *mem)
{
    if (!instr_is_nop(&d->instr))
        return;
    if (!completed(o) || o->pc != CODE_PC + 4 || memcmp(&o->st, in, sizeof(*in)) != 0 ||
        memcmp(mem, g_pristine, DATA_SIZE) != 0)
        report(P_NOP, d, k, "instr_is_nop() but the state changed");
    pass_count[P_NOP]++;
}

static bool
in_ranges(uint64_t a, const memref_t *m, int n, bool writes_only)
{
    a &= 0x00ffffffffffffffULL; /* TBI: tags are ignored */
    for (int i = 0; i < n; i++) {
        if (writes_only && !m[i].write)
            continue;
        uint64_t lo = m[i].addr & 0x00ffffffffffffffULL;
        uint64_t sz = m[i].size;
        if (sz == 0) {
            /* Unknown size (e.g. DC ZVA's OPSZ_sys): the 2KB block holding
             * the address, the largest DC ZVA block.
             */
            lo &= ~2047ULL;
            sz = 2048;
        }
        if (a >= lo && a - lo < sz)
            return true;
    }
    return false;
}

/* Memory the CPU wrote, or the address it faulted on, vs DR's ranges. */
static void
check_memory(drinfo_t *d, int k, const state_t *in, const outcome_t *o,
             const uint8_t *mem)
{
    memref_t m[MAX_MEM];
    int n = dr_memrefs(d, in, m);
    if (completed(o)) {
        for (int i = 0; i < DATA_SIZE; i++) {
            if (mem[i] == g_pristine[i])
                continue;
            if (!in_ranges(DATA_BASE + i, m, n, true)) {
                report(P_MEM_WRITE, d, k,
                       "wrote %llx, outside DR's write ranges (%d refs)",
                       (unsigned long long)(DATA_BASE + i), n);
            }
            break;
        }
        pass_count[P_MEM_WRITE]++;
    } else if (data_fault(o)) {
        /* An alignment fault reports the access address; a translation or
         * permission fault the first faulting byte.  Either lies in a range.
         */
        /* A misaligned SP base raises an SP alignment fault, for which the
         * fault address is not reported.
         */
        bool sp_align = false;
        for (int i = 0; i < n; i++) {
            if (opnd_is_base_disp(m[i].op) && opnd_get_base(m[i].op) == DR_REG_XSP &&
                (in->sp & 15) != 0 && o->sig == SIGBUS)
                sp_align = true;
        }
        if (!sp_align && !in_ranges(o->addr, m, n, false)) {
            report(P_MEM_FAULT, d, k,
                   "sig %d code %d at %llx, outside DR's ranges (%d refs%s)", o->sig,
                   o->code, (unsigned long long)o->addr, n, n > 0 ? "" : ", none");
        }
        pass_count[P_MEM_FAULT]++;
    }
}

/* Base register updates (pre/post-index) vs DR's operands.  DR lists a
 * writeback base as the last destination, and as the second-last source
 * followed by the increment (an immediate or a register).
 */
static void
check_writeback(drinfo_t *d, int k, const state_t *in, const outcome_t *o)
{
    if (!completed(o))
        return;
    instr_t *instr = &d->instr;
    int ns = instr_num_srcs(instr), nd = instr_num_dsts(instr);
    if (ns < 2 || nd < 1)
        return;
    opnd_t base = instr_get_src(instr, ns - 2), inc = instr_get_src(instr, ns - 1);
    opnd_t last = instr_get_dst(instr, nd - 1);
    if (!opnd_is_reg(base) || !opnd_is_reg(last) ||
        opnd_get_reg(base) != opnd_get_reg(last))
        return;
    bool has_mem = false;
    for (int i = 0; i < ns; i++)
        has_mem |= opnd_is_base_disp(instr_get_src(instr, i));
    for (int i = 0; i < nd; i++)
        has_mem |= opnd_is_base_disp(instr_get_dst(instr, i));
    if (!has_mem)
        return;
    int cls, idx;
    map_reg(opnd_get_reg(base), &cls, &idx);
    if (cls != RC_GPR)
        return;
    uint64_t delta;
    if (opnd_is_immed_int(inc))
        delta = (uint64_t)opnd_get_immed_int(inc);
    else if (opnd_is_reg(inc)) {
        int c2, i2;
        map_reg(opnd_get_reg(inc), &c2, &i2);
        if (c2 == RC_NONE)
            delta = 0;
        else if (c2 == RC_GPR)
            delta = gpr_val(in, i2);
        else
            return;
    } else
        return;
    uint64_t want = gpr_val(in, idx) + delta;
    if (gpr_val(&o->st, idx) != want) {
        report(P_WRITEBACK, d, k, "%s = %llx, DR's operands give %llx + %llx",
               gpr_name(idx), (unsigned long long)gpr_val(&o->st, idx),
               (unsigned long long)gpr_val(in, idx), (unsigned long long)delta);
    }
    pass_count[P_WRITEBACK]++;
}

/* A state with every register DR says is not read (and the bits above DR's
 * read width of those it reads) replaced.
 */
static void
perturb(drinfo_t *d, const state_t *in, state_t *out, rng_t *r)
{
    *out = *in;
    for (int i = 0; i < 32; i++) {
        uint64_t *p = gpr_ptr(out, i);
        uint64_t nv = gen_gpr(r);
        if (i == 31 && rnd_n(r, 4) != 0)
            nv = (DATA_MID + (int64_t)rnd_n(r, 4096) - 2048) & ~15ULL;
        if (d->xr[i] == 0)
            *p = nv;
        else if (d->xr[i] == 4)
            *p = (*p & 0xffffffffULL) | (nv & ~0xffffffffULL);
    }
    for (int i = 0; i < 32; i++) {
        __uint128_t nv = gen_vreg(r);
        __uint128_t keep = d->vr[i] == 0 ? 0 : width_mask(d->vr[i]);
        out->v[i] = (out->v[i] & keep) | (nv & ~keep);
    }
    if (!d->nzcv_r)
        out->nzcv = (rnd(r) & 0xf) << 28;
}

/* Outputs of the perturbed run must match the base run, except that a
 * perturbed register may also pass through unchanged.
 */
static bool
reads_ok(const state_t *in1, const outcome_t *o0, const outcome_t *o1, const uint8_t *m0,
         const uint8_t *m1)
{
    if (o0->timeout || o1->timeout)
        return o0->timeout == o1->timeout;
    if (o0->sig != o1->sig || o0->pc != o1->pc || o0->code != o1->code)
        return false;
    if (o0->pc == CODE_PC && o0->addr != o1->addr)
        return false;
    for (int i = 0; i < 32; i++) {
        uint64_t b = gpr_val(&o1->st, i);
        if (b != gpr_val(&o0->st, i) && b != gpr_val(in1, i))
            return false;
    }
    for (int i = 0; i < 32; i++) {
        if (o1->st.v[i] != o0->st.v[i] && o1->st.v[i] != in1->v[i])
            return false;
    }
    if (o1->st.nzcv != o0->st.nzcv && o1->st.nzcv != in1->nzcv)
        return false;
    if (o1->st.fpsr != o0->st.fpsr)
        return false;
    return memcmp(m0, m1, DATA_SIZE) == 0;
}

static uint8_t *g_mem[4];
/* Phantom-write candidates (bit i: GPR i, bit 32 + i: V i), across checks. */
static uint64_t g_phantom;
static bool g_phantom_nzcv;
static int g_phantom_checks;
static bool g_read_failed;

static void
check_reads(drinfo_t *d, int k, const state_t *in, const outcome_t *o0, uint64_t seed)
{
    if (o0->timeout)
        return;
    rng_t r = rng_for(seed, 0x9ead + k);
    state_t s1;
    outcome_t o1;
    perturb(d, in, &s1, &r);
    run_state(&s1, &o1, g_mem[1]);
    if (completed(o0) && completed(&o1)) {
        /* A destination DR says is not read, given different inputs in the
         * two runs, that comes out unchanged both times may not be written.
         * Candidates are reported at the end of the case, if seen in every
         * such check and DR's read claims held up.
         */
        uint64_t cand = 0;
        for (int i = 0; i < 64; i++) {
            bool is_v = i >= 32;
            int j = is_v ? i - 32 : i;
            if ((is_v ? d->vw[j] : d->xw[j]) == 0 || (is_v ? d->vr[j] : d->xr[j]) != 0)
                continue;
            bool same0 =
                is_v ? o0->st.v[j] == in->v[j] : gpr_val(&o0->st, j) == gpr_val(in, j);
            bool same1 =
                is_v ? o1.st.v[j] == s1.v[j] : gpr_val(&o1.st, j) == gpr_val(&s1, j);
            bool differ = is_v ? in->v[j] != s1.v[j] : gpr_val(in, j) != gpr_val(&s1, j);
            if (same0 && same1 && differ)
                cand |= 1ULL << i;
        }
        bool nzcv_cand = !d->nzcv_r && d->nzcv_w && o0->st.nzcv == in->nzcv &&
            o1.st.nzcv == s1.nzcv && in->nzcv != s1.nzcv;
        g_phantom &= cand;
        g_phantom_nzcv &= nzcv_cand;
        g_phantom_checks++;
    }
    if (reads_ok(&s1, o0, &o1, g_mem[0], g_mem[1])) {
        pass_count[P_REG_READ]++;
        return;
    }
    /* Find the register(s) responsible: apply one register's perturbation
     * at a time.
     */
    char buf[256];
    size_t n = 0;
    for (int only = 0; only <= 64; only++) {
        state_t s2 = *in;
        if (only < 32)
            *gpr_ptr(&s2, only) = gpr_val(&s1, only);
        else if (only < 64)
            s2.v[only - 32] = s1.v[only - 32];
        else
            s2.nzcv = s1.nzcv;
        if (memcmp(&s2, in, sizeof(s2)) == 0)
            continue;
        outcome_t o2;
        run_state(&s2, &o2, g_mem[2]);
        if (reads_ok(&s2, o0, &o2, g_mem[0], g_mem[2]))
            continue;
        const char *what = only < 32 ? gpr_name(only) : NULL;
        char vb[8];
        if (only >= 32 && only < 64) {
            snprintf(vb, sizeof(vb), "v%d", only - 32);
            what = vb;
        } else if (only == 64)
            what = "nzcv";
        int width = only < 32 ? d->xr[only] : (only < 64 ? d->vr[only - 32] : 0);
        if (n < sizeof(buf))
            n += snprintf(buf + n, sizeof(buf) - n, "%s%s", n > 0 ? ", " : "", what);
        if (width != 0 && n < sizeof(buf))
            n += snprintf(buf + n, sizeof(buf) - n, " above %d bytes", width);
    }
    g_read_failed = true;
    if (n == 0) {
        /* Only the combination matters: report the full perturbation. */
        describe_diff(o0, &o1, g_mem[0], g_mem[1], buf, sizeof(buf));
        report(P_REG_READ, d, k, "output depends on registers DR says are not read: %s",
               buf);
    } else
        report(P_REG_READ, d, k, "output depends on unread register bits: %s", buf);
}

static void
check_fpcr_read(drinfo_t *d, int k, const state_t *in, const outcome_t *o0, uint64_t seed)
{
    if (d->fpcr_r || o0->timeout)
        return;
    rng_t r = rng_for(seed, 0xf9c4 + k);
    state_t s1 = *in;
    /* Rounding mode, flush-to-zero and default NaN. */
    while (s1.fpcr == in->fpcr)
        s1.fpcr = (uint32_t)rnd(&r) & 0x03c00000U;
    outcome_t o1;
    run_state(&s1, &o1, g_mem[1]);
    /* FPSR's input-denormal/underflow bits follow FZ: compare the rest. */
    o1.st.fpcr = o0->st.fpcr;
    if (!reads_ok(&s1, o0, &o1, g_mem[0], g_mem[1]))
        report(P_FPCR_READ, d, k, "fpcr %x -> %x changes the result", in->fpcr, s1.fpcr);
    else
        pass_count[P_FPCR_READ]++;
}

/* Re-randomizes every data byte outside DR's read ranges: the registers the
 * instruction produces, and whether and where it faults, must not change.
 */
static void
check_mem_read(drinfo_t *d, int k, const state_t *in, const outcome_t *o0, uint64_t seed)
{
    if (!completed(o0))
        return;
    memref_t m[MAX_MEM];
    int n = dr_memrefs(d, in, m);
    static uint8_t saved[DATA_SIZE];
    memcpy(saved, g_pristine, DATA_SIZE);
    rng_t r = rng_for(seed, 0x3e3d + k);
    bool changed = false;
    for (int i = 0; i < DATA_SIZE; i += 8) {
        uint64_t v = rnd(&r);
        for (int b = 0; b < 8; b++) {
            uint64_t a = DATA_BASE + i + b;
            bool keep = false;
            for (int j = 0; j < n; j++) {
                uint64_t lo = m[j].addr & 0x00ffffffffffffffULL;
                if (m[j].size == 0 || (!m[j].write && a >= lo && a - lo < m[j].size) ||
                    /* read-modify-write operands appear as a source too */
                    (m[j].write && a >= lo && a - lo < m[j].size && d->reads_mem))
                    keep = true;
            }
            if (!keep) {
                g_pristine[i + b] = (uint8_t)(v >> (8 * b));
                changed = true;
            }
        }
    }
    if (changed) {
        outcome_t o1;
        run_state(in, &o1, NULL);
        if (o1.timeout != o0->timeout || o1.sig != o0->sig || o1.pc != o0->pc ||
            memcmp(&o1.st, &o0->st, sizeof(o1.st)) != 0) {
            char buf[256];
            describe_diff(o0, &o1, NULL, NULL, buf, sizeof(buf));
            report(P_MEM_READ, d, k,
                   "result depends on memory outside DR's read ranges "
                   "(%d refs, reads_memory=%d): %s",
                   n, d->reads_mem, buf);
        } else
            pass_count[P_MEM_READ]++;
    }
    memcpy(g_pristine, saved, DATA_SIZE);
}

/* Places each memory range DR computes so it ends at the top guard page (and
 * then one byte further), and starts at the bottom one (and one byte lower),
 * by moving its base register.  The placements that fit must not fault; the
 * ones that overlap the guard by a byte must.
 */
static void
check_mem_size(drinfo_t *d, int k, const state_t *in, uint64_t seed)
{
    memref_t m[MAX_MEM];
    int n = dr_memrefs(d, in, m);
    if (n != 1 || m[0].size == 0 || m[0].size > DATA_SIZE / 2 ||
        !opnd_is_base_disp(m[0].op))
        return;
    int cls, idx;
    map_reg(opnd_get_base(m[0].op), &cls, &idx);
    if (cls != RC_GPR)
        return;
    /* The index register must not be the base. */
    if (opnd_get_index(m[0].op) != DR_REG_NULL) {
        int c2, i2;
        map_reg(opnd_get_index(m[0].op), &c2, &i2);
        if (c2 == RC_GPR && i2 == idx)
            return;
    }
    uint64_t base = gpr_val(in, idx);
    uint64_t addr = m[0].addr;
    uint64_t size = m[0].size;
    /* Shift to try past the end: 1 byte, or a whole element when unaligned
     * accesses fault (SP alignment, exclusives, atomics).
     */
    struct {
        uint64_t start;
        bool fault;
    } place[4] = {
        { DATA_END - size, false },
        { DATA_BASE, false },
        { DATA_END - size + 1, true },
        { DATA_BASE - 1, true },
    };
    outcome_t fit[2];
    for (int p = 0; p < 4; p++) {
        state_t s = *in;
        *gpr_ptr(&s, idx) = base + (place[p].start - addr);
        outcome_t o;
        run_state(&s, &o, NULL);
        if (!completed(&o) && !data_fault(&o))
            return; /* e.g. a pointer authentication failure: inconclusive */
        if (p < 2) {
            fit[p] = o;
            if (data_fault(&o) && o.sig == SIGBUS)
                return; /* alignment-checked: the 1-byte shifts say nothing */
            if (data_fault(&o)) {
                report(P_MEM_SIZE, d, k,
                       "range [%llx,+%llu) fits but the CPU faulted at %llx (DR size too "
                       "small?)",
                       (unsigned long long)place[p].start, (unsigned long long)size,
                       (unsigned long long)o.addr);
                return;
            }
        } else {
            if (data_fault(&o) && o.sig == SIGBUS)
                continue; /* the shift made it misaligned */
            if (!data_fault(&o)) {
                report(
                    P_MEM_SIZE, d, k,
                    "range [%llx,+%llu) overlaps a guard page by 1 byte but the CPU did "
                    "not fault (DR size too large?)",
                    (unsigned long long)place[p].start, (unsigned long long)size);
                return;
            }
        }
    }
    pass_count[P_MEM_SIZE]++;
}

/* DR's re-encoding of the instruction must behave identically. */
static void
check_reencode(drinfo_t *d, uint32_t word, const state_t *in, const outcome_t *o0)
{
    uint32_t w2;
    byte *end = instr_encode_to_copy(GD, &d->instr, (byte *)&w2, (byte *)CODE_PC);
    if (end == NULL || w2 == word)
        return;
    if (unsafe_reason(w2) != NULL)
        return;
    install_word(w2);
    outcome_t o2;
    run_state(in, &o2, g_mem[2]);
    install_word(word);
    if (same_outcome(o0, &o2, g_mem[0], g_mem[2])) {
        pass_count[P_REENCODE]++;
        return;
    }
    char buf[256];
    describe_diff(o0, &o2, g_mem[0], g_mem[2], buf, sizeof(buf));
    report(P_REENCODE, d, 0, "re-encoded as 0x%08x, which behaves differently: %s", w2,
           buf);
}

/***************************************************************************
 * One case: a word and a seed.
 */
static int opt_states = 4;

static void
check_word(uint32_t word, uint64_t seed)
{
    g_case_word = word;
    g_case_seed = seed;
    memset(g_reported, 0, sizeof(g_reported));
    for (int p = 0; p < NUM_PROPS; p++) {
        if (!opt_props[p])
            g_reported[p] = true; /* disabled: never report */
    }
    g_cases++;
    if (unsafe_reason(word) != NULL) {
        g_skipped_unsafe++;
        return;
    }
    drinfo_t d;
    dr_describe(word, &d);
    install_word(word);
    g_phantom = ~0ULL;
    g_phantom_nzcv = true;
    g_phantom_checks = 0;
    g_read_failed = false;
    gen_pristine(seed);

    state_t s0;
    rng_t r = rng_for(seed, 0);
    gen_state(&r, &s0);
    outcome_t o0;
    run_state(&s0, &o0, g_mem[0]);

    /* Validity. */
    if (d.valid && undefined(&o0)) {
        int fp = feature_present(word, &d);
        if (fp == 0)
            g_nofeature++;
        else if (fp < 0)
            g_unclassified++;
        else
            report(P_VALIDITY_UNDEF, &d, 0, "DR decodes it but the CPU raises SIGILL");
        dr_free(&d);
        return;
    }
    if (!d.valid) {
        if (undefined(&o0))
            g_undef_agree++;
        else if (!o0.timeout && !sve_or_sme_space(word)) {
            report(P_VALIDITY_EXEC, &d, 0, "DR cannot decode it but the CPU %s",
                   completed(&o0) ? "executes it" : "does not raise SIGILL");
        }
        dr_free(&d);
        return;
    }
    /* Other faults (e.g. SIGILL from a failed pointer authentication with
     * FEAT_FPAC) are outcomes the properties below compare.
     */
    g_executed++;

    /* Nondeterministic instructions (counters, RNDR...) are only checked for
     * validity.
     */
    outcome_t o0b;
    run_state(&s0, &o0b, g_mem[3]);
    if (!same_outcome(&o0, &o0b, g_mem[0], g_mem[3])) {
        g_nondet++;
        dr_free(&d);
        return;
    }

    for (int k = 0; k < opt_states; k++) {
        state_t s;
        outcome_t o;
        if (k == 0) {
            s = s0;
            o = o0;
        } else {
            gen_state(&r, &s);
            /* Every other state aims DR's memory operands into the buffer, so
             * memory instructions also complete.
             */
            if ((k & 1) == 1) {
                memref_t m[MAX_MEM];
                int n = dr_memrefs(&d, &s, m);
                if (n > 0 && opnd_is_base_disp(m[0].op)) {
                    int cls, idx;
                    map_reg(opnd_get_base(m[0].op), &cls, &idx);
                    if (cls == RC_GPR) {
                        uint64_t want = (DATA_MID + rnd_n(&r, 1024)) & ~63ULL;
                        *gpr_ptr(&s, idx) += want - m[0].addr;
                    }
                }
            }
            run_state(&s, &o, g_mem[0]);
        }
        if (opt_props[P_REG_WRITE] || opt_props[P_WRITE_WIDTH] || opt_props[P_FPSR_WRITE])
            check_writes(&d, k, &s, &o);
        check_branch(&d, k, &s, &o);
        check_pcrel(&d, k, &s, &o);
        check_constant(&d, k, &o);
        check_nop(&d, k, &s, &o, g_mem[0]);
        check_memory(&d, k, &s, &o, g_mem[0]);
        check_writeback(&d, k, &s, &o);
        if ((opt_props[P_REG_READ] || opt_props[P_PHANTOM_WRITE]) && k < 2)
            check_reads(&d, k, &s, &o, seed);
        if (opt_props[P_FPCR_READ] && k < 2)
            check_fpcr_read(&d, k, &s, &o, seed);
        if (opt_props[P_MEM_SIZE] && k == 1)
            check_mem_size(&d, k, &s, seed);
        if (opt_props[P_MEM_READ] && (k & 1) == 1)
            check_mem_read(&d, k, &s, &o, seed);
        if (opt_props[P_REENCODE] && k == 0)
            check_reencode(&d, word, &s, &o);
    }
    if (g_phantom_checks >= 2 && !g_read_failed) {
        for (int i = 0; i < 64; i++) {
            if ((g_phantom >> i & 1) != 0) {
                report(P_PHANTOM_WRITE, &d, 0,
                       "%s%d is a destination but the CPU left it unchanged in %d runs "
                       "with different inputs",
                       i >= 32 ? "v" : "x", i & 31, 2 * g_phantom_checks);
            }
        }
        if (g_phantom_nzcv) {
            report(P_PHANTOM_WRITE, &d, 0,
                   "nzcv is a destination but the CPU left it unchanged in %d runs",
                   2 * g_phantom_checks);
        }
        pass_count[P_PHANTOM_WRITE]++;
    }
    dr_free(&d);
}

/***************************************************************************
 * Driver.
 */

/* Words that exposed DR bugs fixed alongside this test, and B.cond, whose
 * conditions depend on instr_predicate_triggered().  Each runs from several
 * seeds in the "regress" mode.
 */
static const struct {
    uint32_t word;
    const char *what;
} regressions[] = {
    { 0x889f7c20, "stllr w0, [x1]: decoded as a load" },
    { 0xc8933b30, "stllr x16, [x25]: decoded as a load" },
    { 0x089f7c20, "stllrb w0, [x1]: decoded as a load" },
    { 0x489f7c20, "stllrh w0, [x1]: decoded as a load" },
    { 0xd50330ff, "sb: not decoded" },
    { 0xd503323f, "dsb oshnxs: not decoded" },
    { 0xd5033e3f, "dsb synxs: not decoded" },
    { 0x5f1ffc20, "fcvtzs h0, h1, #1: not decoded" },
    { 0x7f1ffc20, "fcvtzu h0, h1, #1: not decoded" },
    { 0x5f1fe420, "scvtf h0, h1, #1: not decoded" },
    { 0x7f1fe420, "ucvtf h0, h1, #1: not decoded" },
    { 0x5e0f04a1, "mov b1, v5.b[7]: decoded and re-encoded as vector DUP" },
    { 0x5e1804a1, "mov d1, v5.d[1]: decoded as vector DUP" },
    /* Destinations that are also read, and 128-bit registers given as D. */
    { 0x2f720820, "mla v0.4h, v1.4h, v2.h[7]: Vd not a source, Vm given as D" },
    { 0x0f8288cb, "mul v11.2s, v6.2s, v2.s[2]: Vm given as D" },
    { 0x0fa51883, "fmla v3.2s, v4.2s, v5.s[3]: Vm given as D" },
    { 0x0f522820, "smlal v0.4s, v1.4h, v2.h[5]: Vd not a source and given as D" },
    { 0x0f6a3b38, "sqdmlal v24.4s, v25.4h, v10.h[6]: Vm given as D" },
    { 0x2fa9a907, "umull v7.2d, v8.2s, v9.s[3]: Vm given as D" },
    { 0x0e212841, "xtn v1.8b, v2.8h: Vn given as D" },
    { 0x4e212841, "xtn2 v1.16b, v2.8h: Vd not a source" },
    { 0x0e614883, "sqxtn v3.4h, v4.4s: Vn given as D" },
    { 0x2e6138c5, "shll v5.4s, v6.4h, #16: Vd given as D" },
    { 0x0e190507, "dup v7.8b, v8.b[12]: Vn given as D" },
    { 0x0e032020, "tbl v0.8b, {v1.16b, v2.16b}, v3.8b: only the first table register" },
    { 0x4e0363e0, "tbl v0.16b, {v31.16b-v2.16b}, v3.16b: table registers wrap" },
    { 0x0e0553c4, "tbx v4.8b, {v30.16b-v0.16b}, v5.8b: Vd not a source" },
    { 0x0d409041, "ld1 {v1.s}[1], [x2]: Vt not a source" },
    { 0x4dff687e, "ld4 {v30.h-v1.h}[5], [x3], #8: Vt not sources" },
    { 0x4e0c1c62, "mov v2.s[1], w3: Vd not a source" },
    { 0x6e0a74a4, "mov v4.h[2], v5.h[7]: Vd not a source" },
    { 0x9eaf00e6, "fmov v6.d[1], x7: Vd not a source" },
    { 0x6e6a1d28, "bsl v8.16b, v9.16b, v10.16b: Vd not a source" },
    { 0x6f01340b, "bic v11.4s, #0x20, lsl #8: Vd not a source" },
    { 0x4f02940c, "orr v12.8h, #0x40: Vd not a source" },
    { 0x6f4355cd, "sli v13.2d, v14.2d, #3: Vd not a source" },
    { 0x7f77160f, "usra d15, d16, #9: Vd not a source" },
    { 0x6ea03a51, "usqadd v17.4s, v18.4s: Vd not a source" },
    { 0x4ea06a93, "sadalp v19.2d, v20.4s: Vd not a source" },
    { 0x5e1742d5, "sha256h q21, q22, v23.4s: Vd not a source" },
    { 0x4f1c8f9b, "rshrn2 v27.8h, v28.4s, #4: Vd not a source" },
    { 0x4e616bdd, "fcvtn2 v29.4s, v30.2d: Vd not a source" },
    { 0x54000040, "b.eq: instr_predicate_triggered() missing" },
    { 0x54000048, "b.hi" },
    { 0x5400004a, "b.ge" },
    { 0x5400004c, "b.gt" },
    { 0x5400004d, "b.le" },
    { 0x5400004f, "b.nv" },
};
#define NUM_REGRESSIONS (sizeof(regressions) / sizeof(regressions[0]))
static uint32_t
mutate(rng_t *r)
{
    class_t *c;
    do {
        c = &g_classes[rnd_n(r, g_nclasses)];
    } while (!class_usable(c));
    uint32_t w = c->word;
    uint64_t k = rnd_n(r, 20);
    if (k < 12) {
        w = (w & ~c->mask) | ((uint32_t)rnd(r) & c->mask);
    } else if (k < 17) {
        w = (w & ~c->mask) | ((uint32_t)rnd(r) & c->mask);
        int flips = 1 + (int)rnd_n(r, 2);
        for (int i = 0; i < flips; i++)
            w ^= 1U << rnd_n(r, 32);
    } else
        w ^= 1U << rnd_n(r, 32);
    return w;
}

static void
usage(void)
{
    fprintf(stderr,
            "usage: a64_proptest templates|mutants|random|regress|word [-n N] [-seed S] "
            "[-states K] [-props a,b] [-word W] [-v] [-max-print M] dis-a64*.txt...\n");
    exit(2);
}

int
main(int argc, char **argv)
{
    if (argc < 2)
        usage();
    const char *mode = argv[1];
    uint64_t n = 2000, seed = 1;
    uint32_t word = 0;
    bool have_word = false;
    const char *props = NULL;
    char *files[32];
    int nfiles = 0;
    for (int i = 2; i < argc; i++) {
        if (strcmp(argv[i], "-n") == 0 && i + 1 < argc)
            n = strtoull(argv[++i], NULL, 0);
        else if (strcmp(argv[i], "-seed") == 0 && i + 1 < argc)
            seed = strtoull(argv[++i], NULL, 0);
        else if (strcmp(argv[i], "-states") == 0 && i + 1 < argc)
            opt_states = atoi(argv[++i]);
        else if (strcmp(argv[i], "-props") == 0 && i + 1 < argc)
            props = argv[++i];
        else if (strcmp(argv[i], "-word") == 0 && i + 1 < argc) {
            word = (uint32_t)strtoul(argv[++i], NULL, 16);
            have_word = true;
        } else if (strcmp(argv[i], "-v") == 0)
            opt_verbose++;
        else if (strcmp(argv[i], "-trace") == 0)
            opt_trace++;
        else if (strcmp(argv[i], "-strict") == 0)
            opt_strict = true;
        else if (strcmp(argv[i], "-max-print") == 0 && i + 1 < argc)
            opt_max_print = atoi(argv[++i]);
        else if (argv[i][0] != '-' && nfiles < 32)
            files[nfiles++] = argv[i];
        else
            usage();
    }
    if (getenv("A64_PROPTEST_N") != NULL)
        n = strtoull(getenv("A64_PROPTEST_N"), NULL, 0);
    if (getenv("A64_PROPTEST_STRICT") != NULL && getenv("A64_PROPTEST_STRICT")[0] == '1')
        opt_strict = true;
    if (getenv("A64_PROPTEST_SEED") != NULL)
        seed = strtoull(getenv("A64_PROPTEST_SEED"), NULL, 0);
    if (opt_states < 1)
        opt_states = 1;
    for (int p = 0; p < NUM_PROPS; p++)
        opt_props[p] = props == NULL;
    if (props != NULL) {
        char *list = strdup(props);
        for (char *tok = strtok(list, ","); tok != NULL; tok = strtok(NULL, ",")) {
            int p;
            for (p = 0; p < NUM_PROPS; p++) {
                if (strcmp(tok, prop_names[p]) == 0) {
                    opt_props[p] = true;
                    break;
                }
            }
            if (p == NUM_PROPS) {
                fprintf(stderr, "unknown property %s\n", tok);
                exit(2);
            }
        }
        free(list);
    }

    dr_set_isa_mode(GD, DR_ISA_ARM_A64, NULL);
    init_known();
    setup_sandbox();
    for (int i = 0; i < 4; i++)
        g_mem[i] = malloc(DATA_SIZE);
    for (int p = 0; p < NUM_PROPS; p++) {
        g_opc_fails[p] = calloc(OP_LAST, sizeof(uint32_t));
        g_opc_first[p] = calloc(OP_LAST, sizeof(uint32_t));
    }
    load_vectors(nfiles, files);
    int trace = opt_trace;
    opt_trace = 0;
    probe_classes();
    opt_trace = trace;
    int runnable = 0;
    for (int i = 0; i < g_nclasses; i++)
        runnable += class_usable(&g_classes[i]);
    printf("a64_proptest %s: %d encoding classes, %d for this CPU; seed %llu\n", mode,
           g_nclasses, runnable, (unsigned long long)seed);
    printf("FPCR controls: AFP=%d mask=%08x\n", g_has_afp,
           g_has_afp ? 0x07c80003U : 0x07c80000U);

    if (strcmp(mode, "templates") == 0) {
        for (int i = 0; i < g_nclasses; i++) {
            if (class_usable(&g_classes[i]))
                check_word(g_classes[i].word, seed + i);
        }
    } else if (strcmp(mode, "mutants") == 0) {
        if (runnable == 0) {
            fprintf(stderr, "no runnable encoding classes (pass dis-a64*.txt)\n");
            return 2;
        }
        for (uint64_t i = 0; i < n; i++) {
            rng_t r = rng_for(seed + i, 0x6e7);
            check_word(mutate(&r), seed + i);
        }
    } else if (strcmp(mode, "random") == 0) {
        for (uint64_t i = 0; i < n; i++) {
            rng_t r = rng_for(seed + i, 0x7a4d);
            check_word((uint32_t)rnd(&r), seed + i);
        }
    } else if (strcmp(mode, "regress") == 0) {
        for (size_t i = 0; i < NUM_REGRESSIONS; i++) {
            for (uint64_t k = 0; k < 16; k++)
                check_word(regressions[i].word, seed + 16 * i + k);
        }
    } else if (strcmp(mode, "word") == 0) {
        if (!have_word)
            usage();
        opt_max_print = 1000;
        opt_verbose++;
        check_word(word, seed);
    } else
        usage();

    uint64_t total_fail = 0;
    printf("cases %llu: executed %llu, undefined (DR agrees) %llu, feature absent %llu, "
           "unclassified %llu, nondeterministic %llu, unsafe %llu; runs %llu\n",
           (unsigned long long)g_cases, (unsigned long long)g_executed,
           (unsigned long long)g_undef_agree, (unsigned long long)g_nofeature,
           (unsigned long long)g_unclassified, (unsigned long long)g_nondet,
           (unsigned long long)g_skipped_unsafe, (unsigned long long)g_runs);
    for (int p = 0; p < NUM_PROPS; p++) {
        if (!opt_props[p])
            continue;
        printf("  %-15s checked %8llu  fail %6llu  known %6llu  allowed %6llu\n",
               prop_names[p], (unsigned long long)pass_count[p],
               (unsigned long long)fail_count[p], (unsigned long long)known_count[p],
               (unsigned long long)allowed_count[p]);
        total_fail += fail_count[p];
    }
    for (int p = 0; p < NUM_PROPS; p++) {
        for (int opc = 0; opc < OP_LAST; opc++) {
            if (g_opc_fails[p][opc] > 0) {
                printf("  failing: %-15s %-12s x%u (e.g. 0x%08x)\n", prop_names[p],
                       decode_opcode_name(opc), g_opc_fails[p][opc], g_opc_first[p][opc]);
            }
        }
    }
    for (size_t i = 0; i < NUM_EXPECTED; i++) {
        if (expect_hits[i] > 0) {
            printf("  %s %s x%llu: %s\n", expected[i].known ? "known" : "allowed",
                   expected[i].prop == ANY_PROP ? "*" : prop_names[expected[i].prop],
                   (unsigned long long)expect_hits[i], expected[i].reason);
        }
    }
    for (int i = 0; i < NUM_KI; i++) {
        if (known_hits[i] > 0) {
            printf("  known issue x%llu: %s\n", (unsigned long long)known_hits[i],
                   known_issue_reasons[i]);
        }
    }
    printf("%s\n", total_fail == 0 ? "PASS" : "FAILED");
    return total_fail == 0 ? 0 : 1;
}
