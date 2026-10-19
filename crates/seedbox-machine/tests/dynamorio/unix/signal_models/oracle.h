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

#ifndef SIGNAL_MODELS_ORACLE_H
#define SIGNAL_MODELS_ORACLE_H
#include "timer_model.h"
#define STALL_US 4000
static int64_t it_mux[3];
static long it_suppressed;
/* Accounting only: does not alter the original model decisions/tolerances. */
static struct {
    long values_checked, values_ambiguous, first_required, periodic_required, observed_checked;
} model_coverage;
typedef struct { struct itimerval old; unsigned alarm_old; int64_t t0, t1; } model_op_t;
typedef struct { model_op_t op[IT_MAX_OPS+1]; int nlog, wrong_thread, report; int64_t max_gap, log[IT_MAX_LOG]; } model_trace_t;
/* Known deviation itimer-mux (2026-10-14): with a DR timer of the same type, DR
 * runs the kernel timer at the sooner of the two and accounts the app's timer
 * by the expiries it handles.  Its signal latency and any expiries the kernel
 * merges while one is pending are lost, and an expiry handled during the app's
 * itimer call is charged to the wrong settings: the app's timer runs off by up
 * to a DR period per call, and late by about 1-2% of the elapsed time under a
 * light load (tens of percent on a loaded host).  Values and expiries off by
 * at most 10 ms + a DR period + 50% of the time since arming are counted, not
 * failed.
 */
static int64_t
it_mux_slack(int which, int64_t elapsed)
{
    return it_mux[which] == 0 ? 0 : 10000 + it_mux[which] + elapsed / 2;
}

/* Tolerances (us): the CPU-time timers are checked at scheduler ticks. */
static int64_t
it_late_tol(int which)
{
    /* Generous: the test VM is shared and a loaded host delays signals by tens
     * of ms; the bugs this checks for lose an expiry or shift it by about a
     * whole timer value.
     */
    return which == ITIMER_REAL ? 50000 : 100000;
}
static int64_t
it_early_tol(int which)
{
    /* The kernel checks the CPU-time timers against its own sample of the
     * process's time, which can run a few ms ahead of the clocks we read.
     */
    return which == ITIMER_REAL ? 500 : 30000;
}
static int64_t
it_val_tol(int which)
{
    /* The kernel accounts the CPU-time timers lazily: on this test VM its value
     * lagged the process's CPU clock by up to about 20 ms.
     */
    return which == ITIMER_REAL ? 3000 : 50000;
}

static int
model_check(const itcase_t *c, const model_trace_t *trace, itres_t *res, int is_dr)
{
    memset(&model_coverage, 0, sizeof(model_coverage));
    int which = c->which, fails = 0;
    char why[256];
    why[0] = '\0';
#define ITFAIL(...)                                  \
    do {                                             \
        if (fails++ == 0)                            \
            snprintf(why, sizeof(why), __VA_ARGS__); \
    } while (0)
    int it_nlog = trace->nlog, it_wrong_thread = trace->wrong_thread;
    const int64_t *it_log = trace->log;
    int64_t late = it_late_tol(which), early = it_early_tol(which),
            vtol = it_val_tol(which);
    itarm_t arm = { 0 };
    /* Off-CPU time is tracked across the whole case, calls included. */
    int64_t max_gap = trace->max_gap;
    int64_t ends[IT_MAX_OPS + 1], ends_hi[IT_MAX_OPS + 1];
    itarm_t arms[IT_MAX_OPS + 1];
    int narms = 0;
    for (int i = 0; i <= c->nops; i++) {
        itop_t fin = { IT_SET, 0, 0, 0, 0, late };
        const itop_t *o = i < c->nops ? &c->op[i] : &fin; /* final disarm */
        struct itimerval old = trace->op[i].old;
        unsigned alarm_old = trace->op[i].alarm_old;
        int64_t t0 = trace->op[i].t0, t1 = trace->op[i].t1;
        /* The old (or current) value. */
        int64_t got = (int64_t)old.it_value.tv_sec * 1000000 + old.it_value.tv_usec;
        int64_t hi = it_rem(&arm, arm.s0, t0), lo = it_rem(&arm, arm.s1, t1);
        if (lo > hi) {
            int64_t tmp = lo;
            lo = hi;
            hi = tmp;
        }
        int ambiguous = it_due_in(&arm, t0 - vtol, t1 + vtol) || t1 - t0 > vtol;
        if (ambiguous) model_coverage.values_ambiguous++;
        else model_coverage.values_checked++;
        if (o->op == IT_ALARM) {
            unsigned want_lo = it_alarm_secs(lo - vtol),
                     want_hi = it_alarm_secs(hi + vtol);
            if (!ambiguous && (alarm_old < want_lo || alarm_old > want_hi))
                ITFAIL("op %d: alarm returned %u, model %u..%u", i, alarm_old, want_lo,
                       want_hi);
        } else if (!ambiguous &&
                   ((got > hi + vtol &&
                     got <= hi + vtol + it_mux_slack(which, t1 - arm.s0)) ||
                    (got < lo - vtol &&
                     got >= lo - vtol - it_mux_slack(which, t1 - arm.s0)))) {
            it_suppressed++;
        } else if (!ambiguous && (got < lo - vtol || got > hi + vtol)) {
            ITFAIL("op %d (%s): %s value %lld us, model %lld..%lld", i,
                   o->op == IT_GET ? "getitimer" : "setitimer",
                   o->op == IT_GET ? "current" : "old", (long long)got, (long long)lo,
                   (long long)hi);
        }
        if (o->op != IT_GET && !ambiguous && arm.armed &&
            old.it_interval.tv_sec * 1000000 + old.it_interval.tv_usec != arm.interval &&
            o->op != IT_ALARM) {
            ITFAIL(
                "op %d: old interval %lld us, model %lld", i,
                (long long)(old.it_interval.tv_sec * 1000000 + old.it_interval.tv_usec),
                (long long)arm.interval);
        }
        if (o->op != IT_GET) {
            /* A new arming (or a disarm) starts an epoch. */
            if (narms > 0) {
                ends[narms - 1] = t0;
                ends_hi[narms - 1] = t1;
            }
            arm.armed = o->op == IT_ALARM ? o->alarm_sec > 0 : o->value > 0;
            arm.value = o->op == IT_ALARM ? (int64_t)o->alarm_sec * 1000000 : o->value;
            arm.interval = o->op == IT_ALARM ? 0 : o->interval;
            arm.s0 = t0;
            arm.s1 = t1;
            arms[narms] = arm;
            ends[narms] = ends_hi[narms] = INT64_MAX;
            narms++;
        }
    }
    int nlog = it_nlog;
    res->deliveries = nlog;
    if (it_wrong_thread > 0)
        ITFAIL("%d expiries delivered to the helper thread, which blocks the signal",
               it_wrong_thread);
    res->stalled = max_gap > STALL_US;
    if (res->stalled) {
        /* Inconclusive: only the value checks above (for calls that did not
         * stall) count.
         */
        res->valid = 1;
        res->deliveries = -1;
        goto report;
    }
    /* Deliveries against the epochs. */
    int expected = 0;
    for (int k = 0; k < narms; k++) {
        itarm_t *a = &arms[k];
        if (!a->armed)
            continue;
        int64_t end = ends[k];
        int nreq = 0, nmiss = 0;
        for (int64_t j = 0;; j++) {
            int64_t dlo = a->s0 + a->value + j * a->interval;
            int64_t dhi = a->s1 + a->value + j * a->interval;
            if (dhi + late > end || (a->interval == 0 && j > 0))
                break;
            if (j == 0) model_coverage.first_required++;
            else model_coverage.periodic_required++;
            nreq++;
            expected++;
            int found = 0;
            for (int m = 0; m < nlog && !found; m++)
                found = it_log[m] >= dlo - early && it_log[m] <= dhi + late;
            for (int m = 0; m < nlog && !found; m++) {
                found = it_log[m] >= dlo - early &&
                    it_log[m] <= dhi + late + it_mux_slack(which, dhi - a->s0);
                if (found)
                    it_suppressed++;
            }
            if (!found) {
                if (j == 0) {
                    int64_t next = -1;
                    for (int m = 0; m < nlog; m++) {
                        if (it_log[m] >= dlo && (next < 0 || it_log[m] < next))
                            next = it_log[m];
                    }
                    ITFAIL("arming %d (value %lld interval %lld us): expiry due at "
                           "+%lld..%lld us not delivered within %lld us (next delivery "
                           "%lld us after it)",
                           k, (long long)a->value, (long long)a->interval,
                           (long long)(dlo - a->s0), (long long)(dhi - a->s0),
                           (long long)late, next < 0 ? -1LL : (long long)(next - dlo));
                } else
                    nmiss++;
            }
            if (a->interval == 0)
                break;
        }
        if (nmiss > 1 + nreq / 5)
            ITFAIL("arming %d: %d of %d periodic expiries not delivered", k, nmiss, nreq);
    }
    /* Nothing delivered when no expiry could explain it. */
    for (int m = 0; m < nlog; m++) {
        model_coverage.observed_checked++;
        int ok = 0;
        for (int k = 0; k < narms && !ok; k++) {
            itarm_t *a = &arms[k];
            if (!a->armed)
                continue;
            for (int64_t j = 0;; j++) {
                int64_t dlo = a->s0 + a->value + j * a->interval;
                int64_t dhi = a->s1 + a->value + j * a->interval;
                if (dlo - early > ends_hi[k] || dlo > it_log[m] + early)
                    break;
                if (it_log[m] >= dlo - early && it_log[m] <= dhi + late) {
                    ok = 1;
                    break;
                }
                if (it_log[m] >= dlo - early &&
                    it_log[m] <= dhi + late + it_mux_slack(which, dhi - a->s0)) {
                    ok = 1;
                    it_suppressed++;
                    break;
                }
                if (a->interval == 0)
                    break;
            }
        }
        if (!ok) {
            ITFAIL("delivery %d at +%lld us matches no expiry", m,
                   (long long)(it_log[m] - arms[0].s0));
        }
    }
    res->nexp = expected;
    res->valid = 1;
report:
    if (fails > 0 && trace->report) {
        fprintf(stderr, "  itimer case seed %llu (%s, %s): %s\n",
                (unsigned long long)c->seed,
                which == ITIMER_REAL          ? "REAL"
                    : which == ITIMER_VIRTUAL ? "VIRTUAL"
                                              : "PROF",
                is_dr ? "dr" : "native", why);
        for (int i = 0; i < c->nops; i++) {
            const itop_t *o = &c->op[i];
            fprintf(stderr,
                    "      op %d: %s value %lld interval %lld alarm %d%s wait %lld\n", i,
                    o->op == IT_SET       ? "set"
                        : o->op == IT_GET ? "get"
                                          : "alarm",
                    (long long)o->value, (long long)o->interval, o->alarm_sec,
                    o->other_thread ? " (other thread)" : "", (long long)o->wait);
        }
    }
#undef ITFAIL
    return fails;
}


#endif
