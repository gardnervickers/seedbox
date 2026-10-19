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

/* Property test of AArch64 exclusive-access sequences, natively and under DR
 * with and without -ldstex2cas.  client.ldstex covers fixed shapes (stolen
 * registers, faults, CLREX, clean calls); this test adds:
 *
 * - Strict architectural cases, each repeated: a store-exclusive that fails
 *   writes nothing; CLREX, a completed store-exclusive and an exception return
 *   (a system call, a signal) all clear the monitor, so a following
 *   store-exclusive fails; the status register is written as 0 or 1 in a W
 *   register (its upper half zero); only the bytes of the access are written,
 *   whatever the upper bits of the source registers; a faulting exclusive
 *   load or store reports its own pc and address with every register intact,
 *   and a fault after completed store-exclusives reports their statuses.
 *   Each case runs in one block and split across blocks.
 * - Algorithm cases: seeded rounds of 2-8 threads running planned operations
 *   (fetch-add at 8/16/32/64 bits and on 2x32/2x64 pairs, swaps of unique
 *   operation IDs, and a spinlock guarding a plain counter and a log) on
 *   shared words, in half of the rounds interrupted by asynchronous signals.  Every
 * history must be linearizable: the old values a counter returns are exactly
 * init..init+n-1, swaps form one chain of IDs, the lock log holds every ID once, and no
 * byte outside the targets moves. Retry counts are not compared: spurious failure is
 * permitted.
 * - An explicitly scheduled A->B->A: thread A load-exclusives a word, thread
 *   B writes another value and then the original back, and A's
 *   store-exclusive must fail (another observer wrote the location).  Under
 *   -ldstex2cas the compare-and-swap succeeds instead: the known ABA
 *   deviation documented in api/docs/ldstex.dox, which this test requires to
 *   be reproducible ("ldstex2cas-aba").  -no_ldstex2cas must match native.
 *
 * Every failure names its phase, iteration and diagnostic time. Seedbox runs
 * all 1200 rounds; the owning process guard provides containment. No timed
 * cutoff reduces the population and no watchdog thread waits on host time.
 *
 * Usage: a64_prop_exclusive native|cas|nocas [seed [rounds [ops]]]
 */

#ifndef _GNU_SOURCE
#    define _GNU_SOURCE
#endif
#include <pthread.h>
#include <signal.h>
#include <stdarg.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <time.h>
#include <ucontext.h>
#include <unistd.h>

typedef uint8_t u8;
typedef uint16_t u16;
typedef uint32_t u32;
typedef uint64_t u64;

#define STATUS_PATTERN 0xdeadbeefcafef00dULL
#define REPEAT 64 /* repetitions of each strict case */

static enum { MODE_NATIVE, MODE_CAS, MODE_NOCAS } mode;
static int failures;

/* Where the test is, for failure messages, the watchdog and unexpected
 * signals: a flake must say which case it hit and in what state.
 */
static const char *volatile g_phase = "setup";
static volatile int g_iter;
static double g_start;

static double
now(void)
{
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return ts.tv_sec + ts.tv_nsec / 1e9;
}

static void
fail(const char *fmt, ...) __attribute__((format(printf, 1, 2)));

static void
fail(const char *fmt, ...)
{
    if (++failures > 40)
        return;
    va_list ap;
    va_start(ap, fmt);
    printf("FAIL [%s #%d, %.1fs]: ", g_phase, g_iter, now() - g_start);
    vprintf(fmt, ap);
    printf("\n");
    va_end(ap);
    fflush(stdout);
}

/***************************************************************************
 * Random numbers (splitmix64).
 */
static u64
rnd(u64 *s)
{
    u64 z = (*s += 0x9e3779b97f4a7c15ULL);
    z = (z ^ (z >> 30)) * 0xbf58476d1ce4e5b9ULL;
    z = (z ^ (z >> 27)) * 0x94d049bb133111ebULL;
    return z ^ (z >> 31);
}

/***************************************************************************
 * Strict single-thread cases written as inline assembly.  "x28" (DR's stolen
 * register) is kept out of the sequences: its mangling is client.ldstex's.
 */

/* ldxr; clrex; stxr.  Returns the full 64-bit status register. */
static u64
clrex_same_block(u32 *a, u32 nv, u32 *loaded)
{
    u64 st = STATUS_PATTERN;
    u32 v;
    __asm__ volatile("ldxr %w[v], [%[a]]\n\t"
                     "clrex\n\t"
                     "stxr %w[st], %w[nv], [%[a]]\n\t"
                     : [v] "=&r"(v), [st] "+&r"(st)
                     : [a] "r"(a), [nv] "r"(nv)
                     : "memory", "x28");
    *loaded = v;
    return st;
}

/* The same split into three blocks (the conditional branches are taken). */
static u64
clrex_split(u32 *a, u32 nv, u32 *loaded)
{
    u64 st = STATUS_PATTERN;
    u32 v;
    __asm__ volatile("ldxr %w[v], [%[a]]\n\t"
                     "cbnz %[a], 1f\n\t"
                     "1: clrex\n\t"
                     "cbnz %[a], 2f\n\t"
                     "2: stxr %w[st], %w[nv], [%[a]]\n\t"
                     : [v] "=&r"(v), [st] "+&r"(st)
                     : [a] "r"(a), [nv] "r"(nv)
                     : "memory", "x28");
    *loaded = v;
    return st;
}

/* ldxr; stxr; stxr: after a successful store-exclusive the monitor is open,
 * so the second one must fail.  st1/st2 are the full status registers.
 */
static void
reuse_same_block(u64 *a, u64 nv1, u64 nv2, u64 *st1, u64 *st2)
{
    u64 s1 = STATUS_PATTERN, s2 = STATUS_PATTERN, v;
    __asm__ volatile("ldxr %[v], [%[a]]\n\t"
                     "stxr %w[s1], %[n1], [%[a]]\n\t"
                     "stxr %w[s2], %[n2], [%[a]]\n\t"
                     : [v] "=&r"(v), [s1] "+&r"(s1), [s2] "+&r"(s2)
                     : [a] "r"(a), [n1] "r"(nv1), [n2] "r"(nv2)
                     : "memory", "x28");
    *st1 = s1;
    *st2 = s2;
}

static void
reuse_split(u64 *a, u64 nv1, u64 nv2, u64 *st1, u64 *st2)
{
    u64 s1 = STATUS_PATTERN, s2 = STATUS_PATTERN, v;
    __asm__ volatile("ldxr %[v], [%[a]]\n\t"
                     "cbnz %[a], 1f\n\t"
                     "1: stxr %w[s1], %[n1], [%[a]]\n\t"
                     "cbnz %[a], 2f\n\t"
                     "2: stxr %w[s2], %[n2], [%[a]]\n\t"
                     : [v] "=&r"(v), [s1] "+&r"(s1), [s2] "+&r"(s2)
                     : [a] "r"(a), [n1] "r"(nv1), [n2] "r"(nv2)
                     : "memory", "x28");
    *st1 = s1;
    *st2 = s2;
}

/* ldxr; svc (getpid); stxr: the exception return clears the monitor. */
static u64
svc_between(u32 *a, u32 nv)
{
    u64 st = STATUS_PATTERN;
    u32 v;
    __asm__ volatile("ldxr %w[v], [%[a]]\n\t"
                     "mov x8, #172\n\t"
                     "svc #0\n\t"
                     "stxr %w[st], %w[nv], [%[a]]\n\t"
                     : [v] "=&r"(v), [st] "+&r"(st)
                     : [a] "r"(a), [nv] "r"(nv)
                     : "memory", "x0", "x8", "x28");
    return st;
}

/* The same with a system call DR intercepts (rt_sigprocmask(SIG_BLOCK, NULL,
 * NULL, 8)), which leaves the code cache instead of running inline.
 */
static u64
svc_intercepted_between(u32 *a, u32 nv)
{
    u64 st = STATUS_PATTERN;
    u32 v;
    __asm__ volatile("ldxr %w[v], [%[a]]\n\t"
                     "mov x0, #0\n\t"
                     "mov x1, #0\n\t"
                     "mov x2, #0\n\t"
                     "mov x3, #8\n\t"
                     "mov x8, #135\n\t"
                     "svc #0\n\t"
                     "stxr %w[st], %w[nv], [%[a]]\n\t"
                     : [v] "=&r"(v), [st] "+&r"(st)
                     : [a] "r"(a), [nv] "r"(nv)
                     : "memory", "x0", "x1", "x2", "x3", "x8", "x28");
    return st;
}

/* ldxr; nothing; stxr: the control for svc_between and signal_between. */
static u64
nothing_between(u32 *a, u32 nv)
{
    u64 st = STATUS_PATTERN;
    u32 v;
    __asm__ volatile("ldxr %w[v], [%[a]]\n\t"
                     "stxr %w[st], %w[nv], [%[a]]\n\t"
                     : [v] "=&r"(v), [st] "+&r"(st)
                     : [a] "r"(a), [nv] "r"(nv)
                     : "memory", "x28");
    return st;
}

/* Store-exclusive at each size with junk above the stored bits: only the
 * access's bytes may change.  Each retries until it succeeds (bounded).
 */
#define DEF_STORE(name, T, LD, ST, R, DIRTY)                                     \
    static int name(T *a, T nv, u64 *st_out)                                     \
    {                                                                            \
        for (int tries = 0; tries < 1000; tries++) {                             \
            u64 st = STATUS_PATTERN, v, n = nv;                                  \
            __asm__ volatile(DIRTY LD " %" R "[v], [%[a]]\n\t" ST " %w[st], %" R \
                                      "[n], [%[a]]\n\t"                          \
                             : [v] "=&r"(v), [st] "+&r"(st), [n] "+&r"(n)        \
                             : [a] "r"(a)                                        \
                             : "memory", "x28");                                 \
            *st_out = st;                                                        \
            if (st == 0)                                                         \
                return tries;                                                    \
        }                                                                        \
        return -1;                                                               \
    }
DEF_STORE(store8, u8, "ldxrb", "stxrb", "w", "orr %w[n], %w[n], #0xffffff00\n\t")
DEF_STORE(store16, u16, "ldxrh", "stxrh", "w", "orr %w[n], %w[n], #0xffff0000\n\t")
DEF_STORE(store32, u32, "ldxr", "stxr", "w", "orr %x[n], %x[n], #0xffffffff00000000\n\t")
DEF_STORE(store64, u64, "ldxr", "stxr", "x", "")

/* 2x32 pair with junk in the upper halves of both sources. */
static int
store_pair32(u32 *a, u32 lo, u32 hi, u64 *st_out)
{
    for (int tries = 0; tries < 1000; tries++) {
        u64 st = STATUS_PATTERN, v1, v2, n1 = lo, n2 = hi;
        __asm__ volatile("orr %x[n1], %x[n1], #0xffffffff00000000\n\t"
                         "orr %x[n2], %x[n2], #0xffff000000000000\n\t"
                         "ldxp %w[v1], %w[v2], [%[a]]\n\t"
                         "stxp %w[st], %w[n1], %w[n2], [%[a]]\n\t"
                         : [v1] "=&r"(v1), [v2] "=&r"(v2), [st] "+&r"(st), [n1] "+&r"(n1),
                           [n2] "+&r"(n2)
                         : [a] "r"(a)
                         : "memory", "x28");
        *st_out = st;
        if (st == 0)
            return tries;
    }
    return -1;
}

static int
store_pair64(u64 *a, u64 lo, u64 hi, u64 *st_out)
{
    for (int tries = 0; tries < 1000; tries++) {
        u64 st = STATUS_PATTERN, v1, v2;
        __asm__ volatile("ldxp %[v1], %[v2], [%[a]]\n\t"
                         "stxp %w[st], %[n1], %[n2], [%[a]]\n\t"
                         : [v1] "=&r"(v1), [v2] "=&r"(v2), [st] "+&r"(st)
                         : [a] "r"(a), [n1] "r"(lo), [n2] "r"(hi)
                         : "memory", "x28");
        *st_out = st;
        if (st == 0)
            return tries;
    }
    return -1;
}

/***************************************************************************
 * Cases that need every register or a signal: plain assembly.
 *
 * Each function sets x1 to STATUS_PATTERN and xN (N = 2..30, except the
 * registers the sequence writes) to STATUS_PATTERN + N, runs the sequence
 * and returns x1 (or x3 for loads).  The signal handler checks the frame at
 * the labeled instruction and resumes after it, setting x1/x3 to HANDLED.
 */
#define HANDLED 0x77

#define SAVE_AND_FILL                                                          \
    "stp x29, x30, [sp, #-96]!\n\t"                                            \
    "stp x19, x20, [sp, #16]\n\t"                                              \
    "stp x21, x22, [sp, #32]\n\t"                                              \
    "stp x23, x24, [sp, #48]\n\t"                                              \
    "stp x25, x26, [sp, #64]\n\t"                                              \
    "stp x27, x28, [sp, #80]\n\t"                                              \
    "movz x1, #0xf00d\n\tmovk x1, #0xcafe, lsl #16\n\t"                        \
    "movk x1, #0xbeef, lsl #32\n\tmovk x1, #0xdead, lsl #48\n\t"               \
    "add x2, x1, #2\n\tadd x3, x1, #3\n\tadd x4, x1, #4\n\tadd x5, x1, #5\n\t" \
    "add x6, x1, #6\n\tadd x7, x1, #7\n\tadd x8, x1, #8\n\tadd x9, x1, #9\n\t" \
    "add x10, x1, #10\n\tadd x11, x1, #11\n\tadd x12, x1, #12\n\t"             \
    "add x13, x1, #13\n\tadd x14, x1, #14\n\tadd x15, x1, #15\n\t"             \
    "add x16, x1, #16\n\tadd x17, x1, #17\n\tadd x18, x1, #18\n\t"             \
    "add x19, x1, #19\n\tadd x20, x1, #20\n\tadd x21, x1, #21\n\t"             \
    "add x22, x1, #22\n\tadd x23, x1, #23\n\tadd x24, x1, #24\n\t"             \
    "add x25, x1, #25\n\tadd x26, x1, #26\n\tadd x27, x1, #27\n\t"             \
    "add x28, x1, #28\n\tadd x29, x1, #29\n\tadd x30, x1, #30\n\t"

#define RESTORE_AND_RET           \
    "ldp x19, x20, [sp, #16]\n\t" \
    "ldp x21, x22, [sp, #32]\n\t" \
    "ldp x23, x24, [sp, #48]\n\t" \
    "ldp x25, x26, [sp, #64]\n\t" \
    "ldp x27, x28, [sp, #80]\n\t" \
    "ldp x29, x30, [sp], #96\n\t" \
    "ret\n\t"

#define ASM_FUNC(name)                                                                   \
    ".text\n\t.global " #name "\n\t.type " #name ", %function\n\t.align 4\n" #name ":\n" \
    "\t"

/* u64 fault_stxr(u32 *a): ldxr w3; stxr w1, w4 (faults: read-only). */
u64
fault_stxr(u32 *a);
extern char fault_stxr_pc[];
/* The same with the store in its own block. */
u64
fault_stxr_split(u32 *a);
extern char fault_stxr_split_pc[];
/* u64 fault_ldxr(u32 *a): ldxr w3 (faults: unmapped); returns x3. */
u64
fault_ldxr(u32 *a);
extern char fault_ldxr_pc[];
/* u64 signal_between(u32 *a): ldxr w3; udf; stxr w1, w4.  Returns x1. */
u64
signal_between(u32 *a);
extern char signal_between_pc[];

/* u64 fault_after_stxr(u32 *a): two completed exclusive pairs (statuses in
 * w5 and w6), then a fault in the same block: the frame must show the
 * statuses, not the registers' values before the stores.
 */
u64
fault_after_stxr(u32 *a);
extern char fault_after_stxr_pc[];

__asm__(ASM_FUNC(fault_after_stxr) SAVE_AND_FILL "ldxr w3, [x0]\n\t"
                                                 "stxr w5, w4, [x0]\n\t"
                                                 "ldxr w3, [x0]\n\t"
                                                 "stxr w6, w4, [x0]\n"
                                                 ".global fault_after_stxr_pc\n"
                                                 "fault_after_stxr_pc:\n\t"
                                                 "udf #0x5b\n\t"
                                                 "mov x0, x1\n\t" RESTORE_AND_RET);

__asm__(ASM_FUNC(fault_stxr) SAVE_AND_FILL "ldxr w3, [x0]\n"
                                           ".global fault_stxr_pc\n"
                                           "fault_stxr_pc:\n\t"
                                           "stxr w1, w4, [x0]\n\t"
                                           "mov x0, x1\n\t" RESTORE_AND_RET);

__asm__(ASM_FUNC(fault_stxr_split) SAVE_AND_FILL "ldxr w3, [x0]\n\t"
                                                 "cbnz x0, 1f\n"
                                                 "1:\n"
                                                 ".global fault_stxr_split_pc\n"
                                                 "fault_stxr_split_pc:\n\t"
                                                 "stxr w1, w4, [x0]\n\t"
                                                 "mov x0, x1\n\t" RESTORE_AND_RET);

__asm__(ASM_FUNC(fault_ldxr) SAVE_AND_FILL ".global fault_ldxr_pc\n"
                                           "fault_ldxr_pc:\n\t"
                                           "ldxr w3, [x0]\n\t"
                                           "mov x0, x3\n\t" RESTORE_AND_RET);

__asm__(ASM_FUNC(signal_between) SAVE_AND_FILL "ldxr w3, [x0]\n"
                                               ".global signal_between_pc\n"
                                               "signal_between_pc:\n\t"
                                               "udf #0x5a\n\t"
                                               "stxr w1, w4, [x0]\n\t"
                                               "mov x0, x1\n\t" RESTORE_AND_RET);

static volatile char *g_expect_pc; /* where the expected fault is */
static volatile u64 g_expect_addr; /* its data address (0: none) */
static volatile u32 g_skip_regs;   /* registers the sequence wrote */
static volatile u32 g_status_regs; /* registers holding a status: 0 or 1 */
static volatile bool g_mark_regs;  /* set x1 and x3 to HANDLED */
static volatile int g_handled, g_bad_frames;

static void
frame_handler(int sig, siginfo_t *si, void *ucv)
{
    ucontext_t *uc = (ucontext_t *)ucv;
    mcontext_t *mc = &uc->uc_mcontext;
    if ((char *)mc->pc != g_expect_pc) {
        /* Not ours: say where, then die with the default action. */
        char buf[256];
        int len = snprintf(buf, sizeof(buf),
                           "FAIL [%s #%d]: unexpected signal %d at pc %p, address %p "
                           "(expected one at %p)\n",
                           g_phase, g_iter, sig, (void *)mc->pc, si->si_addr,
                           (void *)g_expect_pc);
        if (write(STDOUT_FILENO, buf, len) < 0) {
            /* Nothing else to do. */
        }
        signal(sig, SIG_DFL);
        return;
    }
    int bad = 0;
    if (g_expect_addr != 0 && (u64)si->si_addr != g_expect_addr) {
        printf("FAIL: signal %d at the expected pc reports address %p, not 0x%llx\n", sig,
               si->si_addr, (unsigned long long)g_expect_addr);
        bad++;
    }
    for (int r = 1; r <= 30; r++) {
        u64 want = r == 1 ? STATUS_PATTERN : STATUS_PATTERN + r;
        if ((g_skip_regs & (1U << r)) != 0)
            continue;
        if ((g_status_regs & (1U << r)) != 0) {
            if (mc->regs[r] > 1 && bad++ < 4) {
                printf("FAIL: signal %d frame: status x%d = 0x%llx, not 0 or 1\n", sig, r,
                       (unsigned long long)mc->regs[r]);
            }
            continue;
        }
        if (mc->regs[r] != want) {
            if (bad++ < 4) {
                printf("FAIL: signal %d frame: x%d = 0x%llx, expected 0x%llx\n", sig, r,
                       (unsigned long long)mc->regs[r], (unsigned long long)want);
            }
        }
    }
    if (bad > 0)
        g_bad_frames++;
    g_handled++;
    if (g_mark_regs) {
        mc->regs[1] = HANDLED;
        mc->regs[3] = HANDLED;
    }
    mc->pc += 4;
}

/***************************************************************************
 * The strict cases.
 */
static u8 *g_page; /* data, read-only, unmapped and A->B->A pages */

static void
check_status(const char *what, u64 st, u64 want_or_any)
{
    /* want_or_any: 0 or 1 for an exact status, 2 for either. */
    if (st != 0 && st != 1) {
        fail("%s: status register 0x%llx is not a zero-extended 0 or 1", what,
             (unsigned long long)st);
    } else if (want_or_any != 2 && st != want_or_any)
        fail("%s: status %llu, expected %llu", what, (unsigned long long)st,
             (unsigned long long)want_or_any);
}

static void
strict_cases(void)
{
    u32 *w = (u32 *)(g_page + 256);
    u64 *x = (u64 *)(g_page + 512);
    u32 loaded;
    g_phase = "strict: monitor";
    for (int i = 0; i < REPEAT; i++) {
        g_iter = i;
        /* A failed store-exclusive writes nothing. */
        *w = 1000 + i;
        u64 st = clrex_same_block(w, 7, &loaded);
        check_status("clrex (one block)", st, 1);
        if (*w != 1000u + i || loaded != 1000u + i)
            fail("clrex (one block): memory %u, loaded %u, expected %u", *w, loaded,
                 1000 + i);
        st = clrex_split(w, 7, &loaded);
        check_status("clrex (split)", st, 1);
        if (*w != 1000u + i)
            fail("clrex (split): memory %u, expected %u", *w, 1000 + i);

        /* A completed store-exclusive opens the monitor. */
        u64 s1, s2;
        for (int split = 0; split < 2; split++) {
            int tries = 0;
            do {
                *x = 5;
                if (split)
                    reuse_split(x, 6, 9, &s1, &s2);
                else
                    reuse_same_block(x, 6, 9, &s1, &s2);
            } while (s1 != 0 && ++tries < 1000);
            const char *what = split ? "second store-exclusive (split)"
                                     : "second store-exclusive (one block)";
            check_status(what, s1, 2);
            if (s1 != 0) {
                fail("%s: the first store-exclusive never succeeded", what);
                continue;
            }
            check_status(what, s2, 1);
            if (*x != 6)
                fail("%s: memory 0x%llx, expected 6", what, (unsigned long long)*x);
        }

        /* An exception return (system call) clears the monitor. */
        *w = 2000 + i;
        st = svc_between(w, 8);
        check_status("system call between", st, 1);
        if (*w != 2000u + i)
            fail("system call between: memory %u, expected %u", *w, 2000 + i);
        st = svc_intercepted_between(w, 8);
        check_status("intercepted system call between", st, 1);
        if (*w != 2000u + i)
            fail("intercepted system call between: memory %u, expected %u", *w, 2000 + i);

        /* An exception return (a signal handler's) clears the monitor. */
        *w = 3000 + i;
        int before = g_handled;
        g_expect_pc = signal_between_pc;
        g_expect_addr = 0;
        g_skip_regs = 1U << 3; /* ldxr w3 */
        g_mark_regs = false;
        st = signal_between(w);
        g_expect_pc = NULL;
        if (g_handled != before + 1)
            fail("signal between: the signal was not delivered");
        check_status("signal between", st, 1);
        if (*w != 3000u + i)
            fail("signal between: memory %u, expected %u", *w, 3000 + i);
    }

    /* Controls: with nothing in between the store eventually succeeds. */
    g_phase = "strict: control";
    g_iter = 0;
    int ok = 0;
    for (int i = 0; i < 1000 && ok == 0; i++) {
        *w = 1;
        u64 st = nothing_between(w, 2);
        check_status("nothing between", st, 2);
        if (st == 0) {
            ok = 1;
            if (*w != 2)
                fail("nothing between: memory %u after success", *w);
        } else if (*w != 1)
            fail("nothing between: memory %u after failure", *w);
    }
    if (!ok)
        fail("nothing between: the store-exclusive never succeeded");

    /* Sizes: only the access's bytes change; neighbours and status width. */
    u8 *b = g_page + 1024;
    g_phase = "strict: sizes";
    for (int i = 0; i < REPEAT; i++) {
        g_iter = i;
        memset(b, 0xa5, 64);
        u64 st;
        if (store8(b + 9, (u8)(0x10 + i), &st) < 0)
            fail("stxrb never succeeded");
        check_status("stxrb", st, 0);
        if (store16((u16 *)(b + 18), (u16)(0x1234 + i), &st) < 0)
            fail("stxrh never succeeded");
        check_status("stxrh", st, 0);
        if (store32((u32 *)(b + 28), 0x89abcdefu + i, &st) < 0)
            fail("stxr (32) never succeeded");
        check_status("stxr (32)", st, 0);
        if (store64((u64 *)(b + 40), 0x0123456789abcdefULL + i, &st) < 0)
            fail("stxr (64) never succeeded");
        check_status("stxr (64)", st, 0);
        u8 want[64];
        memset(want, 0xa5, 64);
        want[9] = (u8)(0x10 + i);
        u16 h = (u16)(0x1234 + i);
        memcpy(want + 18, &h, 2);
        u32 v32 = 0x89abcdefu + i;
        memcpy(want + 28, &v32, 4);
        u64 v64 = 0x0123456789abcdefULL + i;
        memcpy(want + 40, &v64, 8);
        if (memcmp(b, want, 64) != 0) {
            for (int k = 0; k < 64; k++) {
                if (b[k] != want[k]) {
                    fail("sized stores: byte %d is 0x%02x, expected 0x%02x", k, b[k],
                         want[k]);
                    break;
                }
            }
        }
        /* Pairs. */
        memset(b, 0xa5, 64);
        if (store_pair32((u32 *)(b + 16), 0x11111111u + i, 0x22222222u + i, &st) < 0)
            fail("stxp (2x32) never succeeded");
        check_status("stxp (2x32)", st, 0);
        if (store_pair64((u64 *)(b + 32), 0x3333333333333333ULL + i,
                         0x4444444444444444ULL + i, &st) < 0)
            fail("stxp (2x64) never succeeded");
        check_status("stxp (2x64)", st, 0);
        memset(want, 0xa5, 64);
        u32 p32[2] = { 0x11111111u + i, 0x22222222u + i };
        memcpy(want + 16, p32, 8);
        u64 p64[2] = { 0x3333333333333333ULL + i, 0x4444444444444444ULL + i };
        memcpy(want + 32, p64, 16);
        if (memcmp(b, want, 64) != 0) {
            for (int k = 0; k < 64; k++) {
                if (b[k] != want[k]) {
                    fail("pair stores: byte %d is 0x%02x, expected 0x%02x", k, b[k],
                         want[k]);
                    break;
                }
            }
        }
    }

    /* Faults: the frame shows the faulting instruction and every register. */
    u32 *ro = (u32 *)(g_page + 4096 * 2 + 64); /* read-only page */
    u32 *unmapped = (u32 *)(g_page + 4096 * 3 + 128);
    g_phase = "strict: faults";
    for (int i = 0; i < REPEAT / 4; i++) {
        g_iter = i;
        int before = g_handled;
        g_expect_pc = fault_stxr_pc;
        g_expect_addr = (u64)ro;
        g_skip_regs = 1U << 3;
        g_mark_regs = true;
        /* The store faults if the monitor still holds the load's reservation.
         * If it was lost in between (an interrupt) the store may instead just
         * fail (status 1), which the architecture permits: try again.
         */
        u64 r;
        int tries = 0;
        do
            r = fault_stxr(ro);
        while (r == 1 && ++tries < 100);
        if (r != HANDLED || g_handled != before + 1)
            fail("store-exclusive to a read-only page did not fault (x1 0x%llx, %d "
                 "retries, %d signals)",
                 (unsigned long long)r, tries, g_handled - before);
        g_expect_pc = fault_stxr_split_pc;
        tries = 0;
        do
            r = fault_stxr_split(ro);
        while (r == 1 && ++tries < 100);
        if (r != HANDLED || g_handled != before + 2)
            fail("store-exclusive (split) to a read-only page did not fault (x1 0x%llx, "
                 "%d retries, %d signals)",
                 (unsigned long long)r, tries, g_handled - before);
        g_expect_pc = fault_ldxr_pc;
        g_expect_addr = (u64)unmapped;
        g_skip_regs = 0;
        r = fault_ldxr(unmapped);
        if (r != HANDLED || g_handled != before + 3)
            fail("load-exclusive from an unmapped page did not fault (x3 0x%llx)",
                 (unsigned long long)r);
        g_expect_pc = fault_after_stxr_pc;
        g_expect_addr = 0;
        g_skip_regs = 1U << 3;
        g_status_regs = (1U << 5) | (1U << 6);
        u32 word = 9;
        r = fault_after_stxr(&word);
        g_status_regs = 0;
        if (r != HANDLED || g_handled != before + 4)
            fail("udf after store-exclusives: no signal (x1 0x%llx)",
                 (unsigned long long)r);
        g_expect_pc = NULL;
    }
    if (g_bad_frames > 0)
        fail("%d signal frames had wrong registers or addresses", g_bad_frames);
    printf("strict cases: done\n");
}

/***************************************************************************
 * Algorithm cases.
 */
enum {
    OP_FADD8,
    OP_FADD8_SPLIT,
    OP_FADD16,
    OP_FADD32,
    OP_FADD32_SPLIT,
    OP_FADD64,
    OP_FADD64_SPLIT,
    OP_FPAIR32,
    OP_FPAIR64,
    OP_SWAP64,
    OP_LOCK,
    NUM_OPS
};

/* Fetch-add returning the old value; junk above the stored bits. */
#define DEF_FADD(name, T, LD, ST, R, DIRTY, MID)                                   \
    static T name(T *p, T inc)                                                     \
    {                                                                              \
        u64 old, nv, st, i = inc;                                                  \
        __asm__ volatile("1: " LD " %" R "[old], [%[p]]\n\t"                       \
                         "add %" R "[nv], %" R "[old], %" R "[i]\n\t" DIRTY MID ST \
                         " %w[st], %" R "[nv], [%[p]]\n\t"                         \
                         "cbnz %w[st], 1b\n\t"                                     \
                         : [old] "=&r"(old), [nv] "=&r"(nv), [st] "=&r"(st)        \
                         : [p] "r"(p), [i] "r"(i)                                  \
                         : "memory", "x28");                                       \
        return (T)old;                                                             \
    }
#define SPLIT "cbnz %[p], 2f\n2:\n\t"
DEF_FADD(fadd8, u8, "ldaxrb", "stlxrb", "w", "orr %w[nv], %w[nv], #0xffffff00\n\t", "")
DEF_FADD(fadd8_split, u8, "ldxrb", "stxrb", "w", "orr %w[nv], %w[nv], #0xffffff00\n\t",
         SPLIT)
DEF_FADD(fadd16, u16, "ldaxrh", "stlxrh", "w", "orr %w[nv], %w[nv], #0xffff0000\n\t", "")
DEF_FADD(fadd32, u32, "ldaxr", "stlxr", "w",
         "orr %x[nv], %x[nv], #0xffffffff00000000\n\t", "")
DEF_FADD(fadd32_split, u32, "ldxr", "stxr", "w",
         "orr %x[nv], %x[nv], #0xffffffff00000000\n\t", SPLIT)
DEF_FADD(fadd64, u64, "ldaxr", "stlxr", "x", "", "")
DEF_FADD(fadd64_split, u64, "ldxr", "stlxr", "x", "", SPLIT)

/* Pair fetch-add: both halves get inc. */
static void
fpair32(u32 *p, u32 inc, u32 *lo, u32 *hi)
{
    u64 a, b, na, nb, st, i = inc;
    __asm__ volatile(
        "1: ldaxp %w[a], %w[b], [%[p]]\n\t"
        "add %w[na], %w[a], %w[i]\n\t"
        "add %w[nb], %w[b], %w[i]\n\t"
        "orr %x[na], %x[na], #0xffffffff00000000\n\t"
        "cbnz %[p], 2f\n2:\n\t"
        "stlxp %w[st], %w[na], %w[nb], [%[p]]\n\t"
        "cbnz %w[st], 1b\n\t"
        : [a] "=&r"(a), [b] "=&r"(b), [na] "=&r"(na), [nb] "=&r"(nb), [st] "=&r"(st)
        : [p] "r"(p), [i] "r"(i)
        : "memory", "x28");
    *lo = (u32)a;
    *hi = (u32)b;
}

static void
fpair64(u64 *p, u64 inc, u64 *lo, u64 *hi)
{
    u64 a, b, na, nb, st;
    __asm__ volatile(
        "1: ldaxp %[a], %[b], [%[p]]\n\t"
        "add %[na], %[a], %[i]\n\t"
        "add %[nb], %[b], %[i]\n\t"
        "stlxp %w[st], %[na], %[nb], [%[p]]\n\t"
        "cbnz %w[st], 1b\n\t"
        : [a] "=&r"(a), [b] "=&r"(b), [na] "=&r"(na), [nb] "=&r"(nb), [st] "=&r"(st)
        : [p] "r"(p), [i] "r"(inc)
        : "memory", "x28");
    *lo = a;
    *hi = b;
}

static u64
swap64(u64 *p, u64 nv)
{
    u64 old, st;
    __asm__ volatile("1: ldaxr %[old], [%[p]]\n\t"
                     "stlxr %w[st], %[nv], [%[p]]\n\t"
                     "cbnz %w[st], 1b\n\t"
                     : [old] "=&r"(old), [st] "=&r"(st)
                     : [p] "r"(p), [nv] "r"(nv)
                     : "memory", "x28");
    return old;
}

static void
lock_acquire(u32 *l)
{
    u64 t, st;
    __asm__ volatile("1: ldaxr %w[t], [%[l]]\n\t"
                     "cbnz %w[t], 1b\n\t"
                     "stxr %w[st], %w[one], [%[l]]\n\t"
                     "cbnz %w[st], 1b\n\t"
                     : [t] "=&r"(t), [st] "=&r"(st)
                     : [l] "r"(l), [one] "r"(1)
                     : "memory", "x28");
}

static void
lock_release(u32 *l)
{
    __asm__ volatile("stlr wzr, [%[l]]" : : [l] "r"(l) : "memory");
}

/* The arena: targets at fixed offsets, everything else a canary. */
#define ARENA_SIZE 4096
#define OFF_BYTES 0    /* 8 byte counters */
#define OFF_HALVES 64  /* 4 half counters */
#define OFF_WORDS 128  /* 2 word counters */
#define OFF_DWORDS 192 /* 2 dword counters, same line */
#define OFF_PAIR64 256 /* one 2x64 pair */
#define OFF_PAIR32 320 /* one 2x32 pair */
#define OFF_SWAP 384   /* 2 swap words, separate lines */
#define OFF_LOCK 512   /* lock word, then the guarded counter at +8 */
#define CANARY 0xa5

static u8 *g_arena;

typedef struct {
    u8 kind, target;
    u64 id;
    u64 r1, r2; /* returned values */
} rec_t;

#define MAX_THREADS 8
static int g_ops = 6000; /* operations per thread and round, at most */

typedef struct {
    int idx;
    int nops;
    rec_t *ops;
    pthread_barrier_t *bar;
} worker_t;

/* The current round, for the watchdog. */
static volatile int g_round_threads, g_round_ops;
static volatile u32 g_round_enabled;
static volatile int g_progress[MAX_THREADS];

static u64 *g_lock_log;

/* Asynchronous signals interrupt the workers in half of the rounds. */
static volatile int g_finished;
static volatile u64 g_pings;

static void
ping_handler(int sig)
{
    __atomic_fetch_add(&g_pings, 1, __ATOMIC_RELAXED);
}

typedef struct {
    pthread_t *th;
    int nthreads;
    u64 seed;
} pinger_t;

static void *
pinger(void *arg)
{
    pinger_t *p = (pinger_t *)arg;
    u64 s = p->seed;
    while (__atomic_load_n(&g_finished, __ATOMIC_ACQUIRE) < p->nthreads) {
        pthread_kill(p->th[rnd(&s) % p->nthreads], SIGUSR2);
        for (volatile int i = 0; i < 2000; i++) {
        }
    }
    return NULL;
}

static void *
worker(void *arg)
{
    worker_t *w = (worker_t *)arg;
    u8 *A = g_arena;
    pthread_barrier_wait(w->bar);
    for (int j = 0; j < w->nops; j++) {
        rec_t *r = &w->ops[j];
        g_progress[w->idx] = j;
        switch (r->kind) {
        case OP_FADD8: r->r1 = fadd8(A + OFF_BYTES + r->target, 1); break;
        case OP_FADD8_SPLIT: r->r1 = fadd8_split(A + OFF_BYTES + r->target, 1); break;
        case OP_FADD16: r->r1 = fadd16((u16 *)(A + OFF_HALVES) + r->target, 1); break;
        case OP_FADD32: r->r1 = fadd32((u32 *)(A + OFF_WORDS) + r->target, 1); break;
        case OP_FADD32_SPLIT:
            r->r1 = fadd32_split((u32 *)(A + OFF_WORDS) + r->target, 1);
            break;
        case OP_FADD64: r->r1 = fadd64((u64 *)(A + OFF_DWORDS) + r->target, 1); break;
        case OP_FADD64_SPLIT:
            r->r1 = fadd64_split((u64 *)(A + OFF_DWORDS) + r->target, 1);
            break;
        case OP_FPAIR32: {
            u32 lo, hi;
            fpair32((u32 *)(A + OFF_PAIR32), 1, &lo, &hi);
            r->r1 = lo;
            r->r2 = hi;
            break;
        }
        case OP_FPAIR64: fpair64((u64 *)(A + OFF_PAIR64), 1, &r->r1, &r->r2); break;
        case OP_SWAP64:
            r->r1 = swap64((u64 *)(A + OFF_SWAP + 64 * r->target), r->id);
            break;
        case OP_LOCK: {
            u32 *l = (u32 *)(A + OFF_LOCK);
            volatile u64 *ctr = (volatile u64 *)(A + OFF_LOCK + 8);
            lock_acquire(l);
            u64 c = *ctr;
            g_lock_log[c] = r->id;
            *ctr = c + 1;
            lock_release(l);
            r->r1 = c;
            break;
        }
        }
    }
    __atomic_fetch_add(&g_finished, 1, __ATOMIC_RELEASE);
    return NULL;
}

static int
cmp_u64(const void *a, const void *b)
{
    u64 x = *(const u64 *)a, y = *(const u64 *)b;
    return x < y ? -1 : (x > y ? 1 : 0);
}

/* The old values a counter returned must be init..init+n-1 (mod 2^bits).
 * Failures name the round; "a64_prop_exclusive <mode> <seed> <round+1>"
 * reruns the rounds up to it.
 */
static void
check_counter(const char *what, int round, u64 *olds, int n, u64 init, u64 final,
              int bits)
{
    u64 mask = bits == 64 ? ~0ULL : (1ULL << bits) - 1;
    for (int i = 0; i < n; i++)
        olds[i] = (olds[i] - init) & mask;
    qsort(olds, n, sizeof(u64), cmp_u64);
    for (int i = 0; i < n; i++) {
        if (olds[i] != (u64)i) {
            fail("round %d %s: returned old values are not a history (rank %d is "
                 "+%llu): lost or duplicated update",
                 round, what, i, (unsigned long long)olds[i]);
            return;
        }
    }
    if (((final - init) & mask) != ((u64)n & mask))
        fail("round %d %s: final value +%llu after %d increments", round, what,
             (unsigned long long)((final - init) & mask), n);
}

static void
algorithm_round(int round, u64 seed)
{
    u64 s = seed * 0x2545f4914f6cdd1dULL + round;
    int nthreads = 2 + (int)(rnd(&s) % 7);
    /* Each round enables a random subset of operation kinds. */
    u32 enabled = 0;
    while (enabled == 0)
        enabled = (u32)rnd(&s) & ((1U << NUM_OPS) - 1);
    int nops = g_ops / 2 + (int)(rnd(&s) % (g_ops / 2));
    rec_t *ops[MAX_THREADS];
    int byte_count[8] = { 0 };
    u64 next_id = 1;
    for (int t = 0; t < nthreads; t++) {
        ops[t] = calloc(nops, sizeof(rec_t));
        for (int j = 0; j < nops; j++) {
            rec_t *r = &ops[t][j];
            int k;
            do
                k = (int)(rnd(&s) % NUM_OPS);
            while ((enabled & (1U << k)) == 0);
            r->kind = (u8)k;
            r->id = ((u64)(t + 1) << 32) | (next_id++);
            switch (k) {
            case OP_FADD8:
            case OP_FADD8_SPLIT:
                r->target = (u8)(rnd(&s) % 8);
                /* Keep each byte counter below 256 increments. */
                if (byte_count[r->target] >= 250) {
                    r->kind = OP_FADD64;
                    r->target = 0;
                } else
                    byte_count[r->target]++;
                break;
            case OP_FADD16: r->target = (u8)(rnd(&s) % 4); break;
            case OP_FADD32:
            case OP_FADD32_SPLIT: r->target = (u8)(rnd(&s) % 2); break;
            case OP_FADD64:
            case OP_FADD64_SPLIT: r->target = (u8)(rnd(&s) % 2); break;
            case OP_SWAP64: r->target = (u8)(rnd(&s) % 2); break;
            default: r->target = 0; break;
            }
        }
    }
    /* Initial values: random, with the canary everywhere else. */
    memset(g_arena, CANARY, ARENA_SIZE);
    u8 init_bytes[8];
    u16 init_halves[4];
    u32 init_words[2];
    u64 init_dwords[2], init_pair64[2], init_swap[2];
    u32 init_pair32[2];
    for (int i = 0; i < 8; i++)
        init_bytes[i] = (u8)rnd(&s);
    for (int i = 0; i < 4; i++)
        init_halves[i] = (u16)rnd(&s);
    for (int i = 0; i < 2; i++) {
        init_words[i] = (u32)rnd(&s);
        init_dwords[i] = rnd(&s);
        init_pair64[i] = rnd(&s);
        init_pair32[i] = (u32)rnd(&s);
        init_swap[i] = rnd(&s) & 0xffffffffULL; /* never an operation ID */
    }
    memcpy(g_arena + OFF_BYTES, init_bytes, 8);
    memcpy(g_arena + OFF_HALVES, init_halves, 8);
    memcpy(g_arena + OFF_WORDS, init_words, 8);
    memcpy(g_arena + OFF_DWORDS, init_dwords, 16);
    memcpy(g_arena + OFF_PAIR64, init_pair64, 16);
    memcpy(g_arena + OFF_PAIR32, init_pair32, 8);
    for (int i = 0; i < 2; i++)
        memcpy(g_arena + OFF_SWAP + 64 * i, &init_swap[i], 8);
    memset(g_arena + OFF_LOCK, 0, 16);
    int total = nthreads * nops;
    g_iter = round;
    g_round_threads = nthreads;
    g_round_ops = nops;
    g_round_enabled = enabled;
    memset((void *)g_progress, 0, sizeof(g_progress));
    g_lock_log = calloc(total, sizeof(u64));

    pthread_barrier_t bar;
    pthread_barrier_init(&bar, NULL, nthreads);
    pthread_t th[MAX_THREADS];
    worker_t w[MAX_THREADS];
    g_finished = 0;
    for (int t = 0; t < nthreads; t++) {
        w[t] = (worker_t) { t, nops, ops[t], &bar };
        pthread_create(&th[t], NULL, worker, &w[t]);
    }
    pinger_t pg = { th, nthreads, rnd(&s) };
    pthread_t ping_th;
    bool ping = (rnd(&s) & 1) != 0;
    if (ping)
        pthread_create(&ping_th, NULL, pinger, &pg);
    if (ping)
        pthread_join(ping_th, NULL);
    for (int t = 0; t < nthreads; t++)
        pthread_join(th[t], NULL);
    pthread_barrier_destroy(&bar);

    /* Collect each target's history. */
    u64 *vals = malloc(sizeof(u64) * (total + 1));
    u64 *vals2 = malloc(sizeof(u64) * (total + 1));
    char what[64];
#define COLLECT(cond, field)             \
    n = 0;                               \
    for (int t = 0; t < nthreads; t++) { \
        for (int j = 0; j < nops; j++) { \
            rec_t *r = &ops[t][j];       \
            if (cond)                    \
                vals[n++] = r->field;    \
        }                                \
    }
    int n;
    for (int i = 0; i < 8; i++) {
        COLLECT((r->kind == OP_FADD8 || r->kind == OP_FADD8_SPLIT) && r->target == i, r1);
        snprintf(what, sizeof(what), "byte counter %d", i);
        check_counter(what, round, vals, n, init_bytes[i], g_arena[OFF_BYTES + i], 8);
    }
    for (int i = 0; i < 4; i++) {
        COLLECT(r->kind == OP_FADD16 && r->target == i, r1);
        snprintf(what, sizeof(what), "half counter %d", i);
        check_counter(what, round, vals, n, init_halves[i],
                      ((u16 *)(g_arena + OFF_HALVES))[i], 16);
    }
    for (int i = 0; i < 2; i++) {
        COLLECT((r->kind == OP_FADD32 || r->kind == OP_FADD32_SPLIT) && r->target == i,
                r1);
        snprintf(what, sizeof(what), "word counter %d", i);
        check_counter(what, round, vals, n, init_words[i],
                      ((u32 *)(g_arena + OFF_WORDS))[i], 32);
        COLLECT((r->kind == OP_FADD64 || r->kind == OP_FADD64_SPLIT) && r->target == i,
                r1);
        snprintf(what, sizeof(what), "dword counter %d", i);
        check_counter(what, round, vals, n, init_dwords[i],
                      ((u64 *)(g_arena + OFF_DWORDS))[i], 64);
    }
    /* Pairs: both halves move together. */
    COLLECT(r->kind == OP_FPAIR64, r2);
    memcpy(vals2, vals, sizeof(u64) * n);
    COLLECT(r->kind == OP_FPAIR64, r1);
    for (int i = 0; i < n; i++) {
        if (vals2[i] - vals[i] != init_pair64[1] - init_pair64[0]) {
            fail("round %d 2x64 pair: halves out of step (torn pair)", round);
            break;
        }
    }
    check_counter("2x64 pair", round, vals, n, init_pair64[0],
                  ((u64 *)(g_arena + OFF_PAIR64))[0], 64);
    if (((u64 *)(g_arena + OFF_PAIR64))[1] - ((u64 *)(g_arena + OFF_PAIR64))[0] !=
        init_pair64[1] - init_pair64[0])
        fail("round %d 2x64 pair: final halves out of step", round);
    COLLECT(r->kind == OP_FPAIR32, r2);
    memcpy(vals2, vals, sizeof(u64) * n);
    COLLECT(r->kind == OP_FPAIR32, r1);
    for (int i = 0; i < n; i++) {
        if ((u32)(vals2[i] - vals[i]) != (u32)(init_pair32[1] - init_pair32[0])) {
            fail("round %d 2x32 pair: halves out of step (torn pair)", round);
            break;
        }
    }
    check_counter("2x32 pair", round, vals, n, init_pair32[0],
                  ((u32 *)(g_arena + OFF_PAIR32))[0], 32);
    if ((u32)(((u32 *)(g_arena + OFF_PAIR32))[1] - ((u32 *)(g_arena + OFF_PAIR32))[0]) !=
        (u32)(init_pair32[1] - init_pair32[0]))
        fail("round %d 2x32 pair: final halves out of step", round);
    /* Swaps: the previous values and the final value are the initial value and
     * every swapped-in ID, each exactly once: one chain.
     */
    for (int i = 0; i < 2; i++) {
        COLLECT(r->kind == OP_SWAP64 && r->target == i, r1);
        vals[n++] = *(u64 *)(g_arena + OFF_SWAP + 64 * i);
        int m = 0;
        vals2[m++] = init_swap[i];
        for (int t = 0; t < nthreads; t++) {
            for (int j = 0; j < nops; j++) {
                if (ops[t][j].kind == OP_SWAP64 && ops[t][j].target == i)
                    vals2[m++] = ops[t][j].id;
            }
        }
        qsort(vals, n, sizeof(u64), cmp_u64);
        qsort(vals2, m, sizeof(u64), cmp_u64);
        if (n != m || memcmp(vals, vals2, sizeof(u64) * n) != 0)
            fail("round %d swap word %d: the swapped values do not form one chain", round,
                 i);
    }
    /* The lock: every ID logged exactly once, the counter exact. */
    COLLECT(r->kind == OP_LOCK, id);
    u64 lock_ctr = *(u64 *)(g_arena + OFF_LOCK + 8);
    if (lock_ctr != (u64)n)
        fail("round %d lock: counter %llu after %d critical sections", round,
             (unsigned long long)lock_ctr, n);
    else {
        memcpy(vals2, g_lock_log, sizeof(u64) * n);
        qsort(vals, n, sizeof(u64), cmp_u64);
        qsort(vals2, n, sizeof(u64), cmp_u64);
        if (memcmp(vals, vals2, sizeof(u64) * n) != 0)
            fail("round %d lock: the log does not hold every ID once", round);
    }
    if (*(u32 *)(g_arena + OFF_LOCK) != 0)
        fail("round %d lock: still held", round);
    /* Nothing else moved. */
    static const struct {
        int off, len;
    } used[] = { { OFF_BYTES, 8 },   { OFF_HALVES, 8 },    { OFF_WORDS, 8 },
                 { OFF_DWORDS, 16 }, { OFF_PAIR64, 16 },   { OFF_PAIR32, 8 },
                 { OFF_SWAP, 8 },    { OFF_SWAP + 64, 8 }, { OFF_LOCK, 16 } };
    for (int k = 0; k < ARENA_SIZE; k++) {
        bool in_use = false;
        for (size_t u = 0; u < sizeof(used) / sizeof(used[0]); u++) {
            if (k >= used[u].off && k < used[u].off + used[u].len)
                in_use = true;
        }
        if (!in_use && g_arena[k] != CANARY) {
            fail("round %d: byte %d outside every target was written (0x%02x)", round, k,
                 g_arena[k]);
            break;
        }
    }
    free(vals);
    free(vals2);
    free(g_lock_log);
    for (int t = 0; t < nthreads; t++)
        free(ops[t]);
}

/***************************************************************************
 * The scheduled A->B->A.
 */
#define ABA_ATTEMPTS 64
static u32 *g_aba_word;
static volatile u32 *g_flag_a, *g_flag_b;

/* Thread A's attempt: ldxr; publish k; wait for B; stxr.  Returns status. */
static u64
aba_attempt(u32 k, u32 *loaded)
{
    u64 st = STATUS_PATTERN, v, t;
    __asm__ volatile("ldxr %w[v], [%[a]]\n\t"
                     "stlr %w[k], [%[fa]]\n\t"
                     "1: ldar %w[t], [%[fb]]\n\t"
                     "cmp %w[t], %w[k]\n\t"
                     "b.ne 1b\n\t"
                     "stxr %w[st], %w[nv], [%[a]]\n\t"
                     : [v] "=&r"(v), [st] "+&r"(st), [t] "=&r"(t)
                     : [a] "r"(g_aba_word), [k] "r"(k), [fa] "r"(g_flag_a),
                       [fb] "r"(g_flag_b), [nv] "r"(0xbbbbu)
                     : "memory", "cc", "x28");
    *loaded = (u32)v;
    return st;
}

static void *
aba_thread_b(void *arg)
{
    for (u32 k = 1; k <= ABA_ATTEMPTS; k++) {
        while (__atomic_load_n(g_flag_a, __ATOMIC_ACQUIRE) != k) {
        }
        /* A has loaded exclusively: write B, then A back. */
        __atomic_store_n(g_aba_word, 0xb0b0b0b0u, __ATOMIC_SEQ_CST);
        __atomic_store_n(g_aba_word, 0xaaaau, __ATOMIC_SEQ_CST);
        __atomic_store_n(g_flag_b, k, __ATOMIC_RELEASE);
    }
    return NULL;
}

static void
aba_case(void)
{
    /* The word and the flags are pages apart: separate reservation granules. */
    g_aba_word = (u32 *)(g_page + 4096 * 4);
    g_flag_a = (volatile u32 *)(g_page + 4096 * 5);
    g_flag_b = (volatile u32 *)(g_page + 4096 * 5 + 2048);
    *g_flag_a = 0;
    *g_flag_b = 0;
    pthread_t b;
    pthread_create(&b, NULL, aba_thread_b, NULL);
    int succeeded = 0;
    for (u32 k = 1; k <= ABA_ATTEMPTS; k++) {
        *g_aba_word = 0xaaaau;
        u32 loaded;
        u64 st = aba_attempt(k, &loaded);
        check_status("A->B->A", st, 2);
        if (loaded != 0xaaaau)
            fail("A->B->A: loaded 0x%x", loaded);
        if (st == 0) {
            succeeded++;
            if (*g_aba_word != 0xbbbbu)
                fail("A->B->A: succeeded but memory is 0x%x", *g_aba_word);
        } else if (*g_aba_word != 0xaaaau)
            fail("A->B->A: failed but memory is 0x%x", *g_aba_word);
    }
    pthread_join(b, NULL);
    if (mode == MODE_CAS) {
        /* The known deviation: a compare-and-swap cannot see A->B->A. */
        if (succeeded == 0) {
            fail("A->B->A: the ldstex2cas-aba deviation was not reproduced: update "
                 "this test and api/docs/ldstex.dox if -ldstex2cas now detects it");
        } else
            printf("A->B->A: known deviation ldstex2cas-aba reproduced\n");
    } else {
        if (succeeded > 0) {
            fail("A->B->A: %d of %d store-exclusives succeeded after another thread "
                 "wrote the location",
                 succeeded, ABA_ATTEMPTS);
        } else
            printf("A->B->A: every store-exclusive failed\n");
    }
}

/***************************************************************************
 * Main.
 */
int
main(int argc, char **argv)
{
    if (argc < 2) {
        fprintf(stderr, "usage: %s native|cas|nocas [seed]\n", argv[0]);
        return 2;
    }
    if (strcmp(argv[1], "native") == 0)
        mode = MODE_NATIVE;
    else if (strcmp(argv[1], "cas") == 0)
        mode = MODE_CAS;
    else if (strcmp(argv[1], "nocas") == 0)
        mode = MODE_NOCAS;
    else {
        fprintf(stderr, "unknown mode %s\n", argv[1]);
        return 2;
    }
    u64 seed = argc > 2 ? strtoull(argv[2], NULL, 0) : 1;
    int rounds = argc > 3 ? atoi(argv[3]) : 1200;
    if (argc > 4)
        g_ops = atoi(argv[4]) < 2 ? 2 : atoi(argv[4]);
    /* A livelocked exclusive loop is a failure, not a hang. */
    g_start = now();

    g_page =
        mmap(NULL, 4096 * 6, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (g_page == MAP_FAILED) {
        perror("mmap");
        return 1;
    }
    mprotect(g_page + 4096 * 2, 4096, PROT_READ);
    /* Inaccessible rather than unmapped: the kernel can give a hole left by
     * munmap() to the next small mmap() (g_arena's, natively), and then the
     * load-exclusive there no longer faults.
     */
    mprotect(g_page + 4096 * 3, 4096, PROT_NONE);
    g_arena = mmap(NULL, ARENA_SIZE, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS,
                   -1, 0);

    struct sigaction sa;
    memset(&sa, 0, sizeof(sa));
    sa.sa_sigaction = frame_handler;
    sa.sa_flags = SA_SIGINFO;
    sigaction(SIGSEGV, &sa, NULL);
    sigaction(SIGILL, &sa, NULL);
    sigaction(SIGBUS, &sa, NULL);
    memset(&sa, 0, sizeof(sa));
    sa.sa_handler = ping_handler;
    sa.sa_flags = SA_RESTART;
    sigaction(SIGUSR2, &sa, NULL);

    strict_cases();
    double t_strict = now() - g_start;
    /* A loaded machine runs fewer rounds rather than hit drrun's limit. */
    g_phase = "algorithm";
    int done = 0;
    for (int r = 0; r < rounds; r++) {
        algorithm_round(r, seed);
        done++;
    }
    printf("algorithm rounds: done\n");
    double t_alg = now() - g_start - t_strict;
    g_phase = "A->B->A";
    g_iter = 0;
    aba_case();
    double t_aba = now() - g_start - t_strict - t_alg;
    if (failures > 0 || getenv("A64_PROP_EXCLUSIVE_VERBOSE") != NULL) {
        printf("%s: %d of %d rounds, %llu asynchronous signals, %d frames checked; "
               "%.1fs strict, %.1fs rounds, %.1fs A->B->A\n",
               failures > 0 ? "state" : "stats", done, rounds,
               (unsigned long long)g_pings, g_handled, t_strict, t_alg, t_aba);
    }
    printf("%s\n", failures == 0 ? "all properties hold" : "FAILED");
    return failures == 0 ? 0 : 1;
}
