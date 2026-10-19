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

/* Signal ABI and DR-internal-path properties of AArch64 execution under
 * (static) DynamoRIO.  Each case is a hand-written scenario template (below,
 * in assembly) with seeded parameters: initial GPRs/V registers/NZCV/DIT,
 * the signal, its source, its sigaction flags and masks, and the effects the
 * handler applies to the interrupted context.  Every case runs natively (the
 * paired oracle) and then under DR, and each run is also checked against an
 * independent model of the scenario:
 *
 *   - every handler invocation sees a permitted application context: a PC the
 *     scenario allows at that point (legal deferral of asynchronous delivery
 *     is allowed: any PC of the spin loop the scenario waits in) with exactly
 *     the modelled GPRs, SP, NZCV, DIT, V registers and FPCR, and a frame
 *     whose record list is valid by the kernel ABI (a64p_check_frame_records)
 *     and identical to the native one;
 *   - handler-written GPR, NZCV, DIT, V register, PC (a labelled continuation)
 *     and SP (moved within the live stack) take effect after sigreturn, and
 *     state the handler does not write is preserved (notably DIT);
 *   - nesting (SA_NODEFER, or a second signal raised from the handler) is
 *     delivered inside the outer handler and the inner handler's edit of the
 *     outer handler's context takes effect; sa_mask blocks a signal raised in
 *     the handler until sigreturn, which then delivers it with the restored
 *     mask; sigprocmask inside the block keeps a signal pending until it is
 *     unblocked; sigaltstack enabled and disabled from the block moves the
 *     frame onto and off the alternate stack and reports it in uc_stack and
 *     sigaltstack(); real-time payloads arrive exactly once, lowest signal
 *     first and FIFO per signal; a blocking pipe read interrupted with
 *     SA_RESTART is restarted (frame PC at the svc, x0 the fd) and without it
 *     returns EINTR (frame PC after the svc, x0 -EINTR);
 *   - synchronous signals (UDF, BRK, a load from a guard page) are taken at
 *     the faulting instruction with the native si_code, si_addr, fault_address
 *     and ESR record;
 *   - live stack above the interrupted SP (canaries) is never written;
 *   - nothing hangs (every wait is bounded; a child is killed and reported
 *     after a timeout).
 *
 * Modes:
 *   abi: the scenarios natively and under DR (default, -max_bb_instrs 3 and
 *     -disable_traces, plus -steal_reg 27 and resets at every pending signal
 *     against a debug DR).
 *   paths: the same scenarios plus an IBL-churning spin loop under an
 *     in-process client that adds drreg reservations with clobbered values,
 *     aflags clobbers, drx counters and clean calls (all of them with
 *     A64_DR_PROP_CLEANCALL=1, which one variant always sets) to every
 *     scenario instruction; then "storm": threads exiting while signals are in
 *     flight.  Four threads (the main one included) wait in read() or an
 *     unbounded ppoll() of their own empty pipe, each interrupted by its own
 *     SIGEV_THREAD_ID timer and by tgkills at random moments, while a
 *     SIGEV_THREAD timer spawns notification threads that exit, other threads
 *     with the signals unblocked are created and exit (at most -churn_max, 8,
 *     at a time), a process-directed POSIX timer runs, and another thread sends
 *     sequence-numbered process-directed rt signals one at a time: each must be
 *     delivered exactly once, every expiration of the process timer must be
 *     accounted (deliveries plus overruns, counted exactly at a cut), and no
 *     waiter may stop being interrupted for 3 s (the shape of the hang fixed by
 *     "Deliver a signal that arrives on the way into a syscall before it").
 *   storm: just the storm (-waiters N, -poker 0|1 and -churn_max N vary it).
 *
 * Usage: a64_prop_signal <abi|paths|storm> [-seed S] [-n N] [-only I]
 *        [-rounds R] [-variants a,b] [-waiters N] [-poker 0|1] [-churn_max N] [-v]
 * -n is the number of cases per scenario kind; -only runs one case index.
 *
 * Known deviation (counted as suppressed): rt-overflow-order, see
 * DR_MAX_PENDING_SIGNALS below.  Known issue: memcache-check-race, see
 * STORM_DEBUG_DR_OPS below.
 *
 * Counts are printed as generated / accepted / executed / checked /
 * suppressed / inconclusive, with floors; "all properties hold" is only
 * printed if every check and floor passed.
 */

#include <stdint.h>
typedef struct {
    uint64_t s;
} rng_t;

static inline uint64_t
rnext(rng_t *r)
{
    uint64_t z = (r->s += 0x9e3779b97f4a7c15ULL);
    z = (z ^ (z >> 30)) * 0xbf58476d1ce4e5b9ULL;
    z = (z ^ (z >> 27)) * 0x94d049bb133111ebULL;
    return z ^ (z >> 31);
}

static inline uint32_t
rn(rng_t *r, uint32_t n)
{
    return n == 0 ? 0 : (uint32_t)(rnext(r) % n);
}

static inline int
rp(rng_t *r, int pct)
{
    return (int)rn(r, 100) < pct;
}

/* Interesting 64-bit values. */
static uint64_t
rval(rng_t *r)
{
    switch (rn(r, 10)) {
    case 0: return 0;
    case 1: return ~0ULL;
    case 2: return 1ULL << rn(r, 64);
    case 3: return rn(r, 256);
    case 4: return (uint64_t)-(int64_t)rn(r, 256);
    case 5: return 0x8000000000000000ULL ^ rn(r, 4);
    case 6: return 0x7fffffffULL + rn(r, 3) - 1;
    case 7: return rnext(r) & 0xffffffffULL;
    default: return rnext(r);
    }
}

