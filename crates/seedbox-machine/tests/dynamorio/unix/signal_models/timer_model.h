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

#ifndef SIGNAL_TIMER_MODEL_H
#define SIGNAL_TIMER_MODEL_H
#include "common.h"
#define IT_MAX_OPS 8
#define IT_MAX_LOG 512
enum { IT_SET, IT_GET, IT_ALARM };
typedef struct {
    int op, other_thread, alarm_sec;
    int64_t value, interval, wait; /* microseconds */
} itop_t;
typedef struct {
    int which, nops;
    uint64_t seed;
    itop_t op[IT_MAX_OPS];
} itcase_t;
typedef struct {
    int valid, nexp, stalled;
    long deliveries;
} itres_t;
static void
gen_itcase(uint64_t seed, itcase_t *c)
{
    rng_t r = { seed * 0x9e3779b97f4a7c15ULL + 11 };
    memset(c, 0, sizeof(*c));
    c->seed = seed;
    /* Half the cases on ITIMER_REAL, whose checks are much tighter. */
    c->which = rp(&r, 50) ? ITIMER_REAL : 1 + (int)rn(&r, 2);
    c->nops = 3 + (int)rn(&r, IT_MAX_OPS - 3);
    int alarm_armed = 0;
    for (int i = 0; i < c->nops; i++) {
        itop_t *o = &c->op[i];
        int k = (int)rn(&r, 10);
        if (c->which == ITIMER_REAL && k == 0 && !alarm_armed && i < c->nops - 1) {
            o->op = IT_ALARM;
            o->alarm_sec = 1 + (int)rn(&r, 2); /* re-armed or disarmed before it fires */
            alarm_armed = 1;
        } else if (c->which == ITIMER_REAL && k == 1) {
            o->op = IT_ALARM; /* alarm(0) */
            alarm_armed = 0;
        } else if (k == 2) {
            o->op = IT_GET;
        } else {
            o->op = IT_SET;
            alarm_armed = 0;
            if (!rp(&r, 15)) {
                o->value = 3000 + rn(&r, 97000);
                o->interval = rp(&r, 40) ? 15000 + rn(&r, 35000) : 0;
            }
        }
        if (c->which == ITIMER_REAL)
            o->other_thread = rp(&r, 30);
        o->wait = 2000 + rn(&r, 100000);
    }
    /* Whatever is left armed is disarmed at the end (and checked). */
}

/* The model: the timer's remaining time at t for an arming at s. */
typedef struct {
    int armed;
    int64_t s0, s1; /* the arming happened in [s0, s1] */
    int64_t value, interval;
} itarm_t;

static inline int64_t
it_rem(const itarm_t *a, int64_t s, int64_t t)
{
    if (!a->armed)
        return 0;
    int64_t d = s + a->value;
    if (t < d)
        return d - t;
    if (a->interval == 0)
        return 0;
    return a->interval - (t - d) % a->interval;
}

/* Whether an expiry of a may fall in [lo, hi]. */
static inline int
it_due_in(const itarm_t *a, int64_t lo, int64_t hi)
{
    if (!a->armed)
        return 0;
    for (int64_t j = 0;; j++) {
        int64_t d0 = a->s0 + a->value + j * a->interval;
        int64_t d1 = a->s1 + a->value + j * a->interval;
        if (d0 > hi)
            return 0;
        if (d1 >= lo)
            return 1;
        if (a->interval == 0)
            return 0;
    }
}

/* alarm()'s rounding of a remaining time to seconds. */
static inline unsigned
it_alarm_secs(int64_t us)
{
    if (us <= 0)
        return 0;
    unsigned s = (unsigned)(us / 1000000);
    if ((s == 0 && us % 1000000 != 0) || us % 1000000 >= 500000)
        s++;
    return s;
}


/* Model arithmetic accepts explicit clock inputs. The live subject-timer
 * fixture supplies independent observations; pure tests supply synthetic ones. */
static inline int model_old_interval(const itarm_t *a, const struct itimerval *old) {
    return old->it_interval.tv_sec * 1000000 + old->it_interval.tv_usec == a->interval;
}
#endif
