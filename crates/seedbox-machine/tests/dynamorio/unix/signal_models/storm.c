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

#include "storm_model.h"
static uint64_t monotonic_ns(void)
{
    struct timespec t;
    REQUIRE(clock_gettime(CLOCK_MONOTONIC, &t) == 0);
    return (uint64_t)t.tv_sec * 1000000000ULL + t.tv_nsec;
}
#include <semaphore.h>
#define STORM_Q 24
/* Threads (the main one included) blocked in read()/ppoll() of their own empty
 * pipe, each interrupted by its own SIGEV_THREAD_ID timer.
 */
#define STORM_WAITERS 4
typedef struct {
    _Atomic long weintr[STORM_WAITERS];
    _Atomic pid_t wtid[STORM_WAITERS];
    _Atomic long t3_acct, q_dup, q_seen_n;
    _Atomic long churn_live;
    _Atomic uint8_t q_seen[STORM_Q + 1];
    _Atomic int round_over, sender_done, churn_on;
    _Atomic long lost, hang;
    uint64_t t1_period_ns, t3_period_ns;
} storm_t;
static storm_t st;
static sem_t progress, queue_ack, churn_slots, churn_done;
static _Atomic long churn_created;
#include "storm_diagnostics.h"
static int sig_t1, sig_t3, sig_q;

/* -churn_max: live churn threads (default 8). */
static long storm_churn_max = 8;
/* -waiters N (1..STORM_WAITERS), -poker 0|1. */
static int storm_waiters = STORM_WAITERS, storm_poker = 1;
static int wpipe[STORM_WAITERS][2];
static timer_t wtimer[STORM_WAITERS];
static _Atomic int wtimer_ok[STORM_WAITERS];
static __thread int my_waiter = -1;

static void
h_t1(int sig, siginfo_t *si, void *uc)
{
    (void)sig; (void)uc;
    /* Thread-directed: only a waiter may get its timer's signal (the poker can
     * race with a waiter's exit).
     */
    if (my_waiter < 0 && si->si_code == SI_TIMER)
        atomic_fetch_add(&st.lost, 1000);
}

static void
storm_wake_waiters(void)
{
    for (int w = 0; w < STORM_WAITERS; w++) {
        diag[D_w0_wake_attempt + w * 6] = 1;
        ssize_t result = write(wpipe[w][1], "e", 1);
        int error = result < 0 ? errno : 0;
        diag[D_w0_wake_errno + w * 6] = error;
        diag[D_w0_wake_result + w * 6] = (uint64_t)result;
        diag[D_w0_wake_attempt + w * 6] = 2;
    }
}

/* Waits in read() or an unbounded ppoll() of its pipe, interrupted by its own
 * timer, until the pipe gets a byte.  The ppoll is issued by DR from its
 * syscall gencode after delivering pending signals: the path of "Deliver a
 * signal that arrives on the way into a syscall before it".
 */
static void
waiter_loop(int w)
{
    my_waiter = w;
    WROLE(w);
    struct sigevent sev;
    memset(&sev, 0, sizeof(sev));
    sev.sigev_notify = SIGEV_THREAD_ID;
    sev.sigev_signo = sig_t1;
    sev._sigev_un._tid = (pid_t)syscall(SYS_gettid);
    st.wtid[w] = sev._sigev_un._tid;
    if (timer_create(CLOCK_MONOTONIC, &sev, &wtimer[w]) != 0) {
        fprintf(stderr, "ERROR: storm: timer_create failed\n");
        exit(3);
    }
    long period = (long)st.t1_period_ns * (2 + w) / 2;
    struct itimerspec it = { { 0, period }, { 0, period } };
    REQUIRE(timer_settime(wtimer[w], 0, &it, NULL) == 0);
    wtimer_ok[w] = 1;
    diag[D_w0_armed + w * 6] = 1;
    char b;
    for (int iter = 0;; iter++) {
        WROLE(w);
        ssize_t n;
        if ((iter + w) % 2 == 0)
            n = read(wpipe[w][0], &b, 1);
        else {
            struct pollfd pfd = { wpipe[w][0], POLLIN, 0 };
            WROLE(w);
            n = syscall(SYS_ppoll, &pfd, 1, NULL, NULL, 0);
            if (n == 1)
                n = read(wpipe[w][0], &b, 1);
        }
        if (n == 1)
            break;
        if (n < 0 && errno == EINTR) {
            atomic_fetch_add(&st.weintr[w], 1);
            atomic_fetch_add(&diag[D_w0_eintr + w * 6], 1);
            REQUIRE(post_progress() == 0);
        }
        else {
            fprintf(stderr, "  storm: waiter %d: read returned %zd errno %d\n", w, n,
                    errno);
            st.hang++;
            break;
        }
    }
    WROLE(w);
    st.wtid[w] = 0;
    wtimer_ok[w] = 0;
    timer_delete(wtimer[w]);
    diag[D_w0_armed + w * 6] = 0;
    WROLE(w);
    my_waiter = -1;
}

/* Sends the waiters' signal at random moments, so that some arrive while a
 * waiter is on its way into a syscall (the timers' signals arrive a fixed
 * delay after the waiter re-entered its wait).
 */
static void *
poker_main(void *arg)
{
    ROLE(poker);
    rng_t r = { (uint64_t)(uintptr_t)arg };
    while (!st.round_over) {
        int w = (int)rn(&r, (uint32_t)storm_waiters);
        pid_t t = st.wtid[w];
        if (t != 0 && st.weintr[w] > 0)
            syscall(SYS_tgkill, getpid(), t, sig_t1);
        for (volatile int i = (int)rn(&r, 20000); i > 0; i--)
            ;
    }
    return NULL;
}

static void *
waiter_thread(void *arg)
{
    sigset_t none;
    sigemptyset(&none);
    pthread_sigmask(SIG_SETMASK, &none, NULL);
    waiter_loop((int)(uintptr_t)arg);
    return NULL;
}

static void
h_t3(int sig, siginfo_t *si, void *uc)
{
    (void)sig; (void)uc;
    atomic_fetch_add(&st.t3_acct, 1 + si->si_overrun);
    atomic_fetch_add(&diag[D_t3_acct], 1 + si->si_overrun);
    post_progress();
}

static void
h_q(int sig, siginfo_t *si, void *uc)
{
    (void)sig; (void)uc;
    int v = si->si_value.sival_int;
    if (v < 1 || v > STORM_Q)
        return;
    if (atomic_exchange(&st.q_seen[v], 1))
        atomic_fetch_add(&st.q_dup, 1);
    atomic_fetch_add(&st.q_seen_n, 1);
    atomic_fetch_add(&diag[D_acknowledged], 1);
    sem_post(&queue_ack);
    post_progress();
}

typedef struct { _Atomic long notifications, inflight; } notify_t;
static notify_t notification[40];
static notify_t *current_notification;
static void
t2_notify(union sigval v)
{
    notify_t *n = v.sival_ptr;
    _Atomic uint64_t *nd = diagnostics[n - notification];
    atomic_fetch_add(&nd[D_inflight], 1);
    atomic_fetch_add(&nd[D_notifications], 1);
    atomic_fetch_add(&n->inflight, 1);
    atomic_fetch_add(&n->notifications, 1);
    for (volatile int i = 0; i < 2000; i++) ;
    atomic_fetch_sub(&n->inflight, 1);
    atomic_fetch_sub(&nd[D_inflight], 1);
    atomic_fetch_add(&nd[D_progress_posts], 1);
    sem_post(&progress);
}

static void *
churn_child(void *arg)
{
    sigset_t none;
    sigemptyset(&none);
    pthread_sigmask(SIG_SETMASK, &none, NULL);
    for (volatile long i = (long)(uintptr_t)arg; i > 0; i--)
        ;
    /* Mask before publishing completion: no exiting churn thread can take a
     * process signal belonging to the next round. */
    sigset_t all;
    sigfillset(&all);
    REQUIRE(pthread_sigmask(SIG_SETMASK, &all, NULL) == 0);
    atomic_fetch_sub(&st.churn_live, 1);
    atomic_fetch_sub(&diag[D_churn_live], 1);
    atomic_fetch_add(&diag[D_churn_done], 1);
    REQUIRE(sem_post(&churn_slots) == 0);
    REQUIRE(sem_post(&churn_done) == 0);
    return NULL;
}

static void wait_sem(sem_t *s)
{
    int rc;
    do { rc = sem_wait(s); } while (rc < 0 && errno == EINTR);
    REQUIRE(rc == 0);
}
static void *churn_main(void *arg)
{
    ROLE(churn);
    rng_t *r = arg;
    while (st.churn_on) {
        ROLE(churn);
        wait_sem(&churn_slots);
        if (!st.churn_on) { sem_post(&churn_slots); break; }
        pthread_attr_t a;
        pthread_t t;
        REQUIRE(pthread_attr_init(&a) == 0);
        REQUIRE(pthread_attr_setdetachstate(&a, PTHREAD_CREATE_DETACHED) == 0);
        atomic_fetch_add(&st.churn_live, 1);
        atomic_fetch_add(&diag[D_churn_live], 1);
        ROLE(churn);
        REQUIRE(pthread_create(&t, &a, churn_child, (void *)(uintptr_t)rn(r, 20000)) == 0);
        atomic_fetch_add(&churn_created, 1);
        atomic_fetch_add(&diag[D_churn_created], 1);
        ROLE(churn);
        REQUIRE(pthread_attr_destroy(&a) == 0);
        (void)rn(r, 300); /* Preserve the original pacing draw, without sleeping. */
    }
    return NULL;
}
static void *sender_main(void *unused)
{
    (void)unused;
    ROLE(sender);
    for (int q = 1; q <= STORM_Q; q++) {
        union sigval v = { .sival_int = q };
        ROLE(sender);
        REQUIRE(sigqueue(getpid(), sig_q, v) == 0);
        diag[D_sent] = q;
        ROLE(sender);
        wait_sem(&queue_ack);
        REQUIRE(st.q_seen[q] == 1);
    }
    st.sender_done = 1;
    diag[D_sender_done] = 1;
    ROLE(sender);
    REQUIRE(post_progress() == 0);
    return NULL;
}
static void *controller_main(void *unused)
{
    (void)unused;
    ROLE(controller);
    for (;;) {
        ROLE(controller);
        wait_sem(&progress);
        ROLE(controller);
        atomic_fetch_add(&diag[D_controller_seq], 1);
        unsigned bits = (st.sender_done ? 1 : 0) |
            (current_notification->notifications > 0 ? 2 : 0) |
            (st.t3_acct > 0 ? 4 : 0) | (churn_created > 0 ? 8 : 0);
        for (int w = 0; w < storm_waiters; w++)
            if (st.weintr[w] >= 10) bits |= 16U << w;
        diag[D_predicate] = bits;
        atomic_fetch_add(&diag[D_controller_seq], 1);
        if (bits == ((16U << storm_waiters) - 1)) break;
    }
    st.round_over = 1;
    diag[D_round_over] = 1;
    ROLE(controller);
    storm_wake_waiters();
    ROLE(controller);
    return NULL;
}

static void
drain_fd(int fd)
{
    int n = 0;
    char b[64];
    while (ioctl(fd, FIONREAD, &n) == 0 && n > 0) {
        if (read(fd, b, n > 64 ? 64 : n) <= 0)
            break;
    }
}

static int
storm_round(int round, rng_t *r, timer_t t2, timer_t t3, long *eintr_total)
{
    diag = diagnostics[round];
    diag[D_pid] = getpid();
    diag[D_round] = round + 1;
    ROLE(main);
    memset((void *)&st, 0, sizeof(st));
    churn_created = 0;
    REQUIRE(sem_init(&churn_slots, 0, (unsigned)storm_churn_max) == 0);
    REQUIRE(sem_init(&churn_done, 0, 0) == 0);
    for (int w = 0; w < STORM_WAITERS; w++)
        drain_fd(wpipe[w][0]);
    /* Short enough to often catch the thread on its way into read(), long
     * enough for a debug DR to keep up.
     */
    st.t1_period_ns = 40000 + rn(r, 260) * 1000ULL;
    st.t3_period_ns = 50000 + rn(r, 450) * 1000ULL;
    uint64_t t2_period = 500000 + rn(r, 2500) * 1000ULL;
    uint64_t dur = 40000000ULL + rn(r, 80) * 1000000ULL;
    st.churn_on = 1;
    diag[D_churn_on] = 1;
    sigset_t all, none;
    sigfillset(&all);
    sigemptyset(&none);
    /* The helper threads block the test signals; the main thread and the
     * exiting threads do not.
     */
    pthread_sigmask(SIG_BLOCK, &all, NULL);
    pthread_t churn, sender, ctl, poker, waiters[STORM_WAITERS];
    rng_t cr = { rnext(r) };
    int ok = pthread_create(&churn, NULL, churn_main, &cr) == 0;
    ok = ok && pthread_create(&sender, NULL, sender_main, NULL) == 0;
    for (int w = 1; w < storm_waiters; w++)
        ok = ok &&
            pthread_create(&waiters[w], NULL, waiter_thread, (void *)(uintptr_t)w) == 0;
    ok = ok && pthread_create(&ctl, NULL, controller_main, &dur) == 0;
    uint64_t poker_seed = rnext(r);
    if (storm_poker)
        ok = ok &&
            pthread_create(&poker, NULL, poker_main, (void *)(uintptr_t)poker_seed) == 0;
    pthread_sigmask(SIG_SETMASK, &none, NULL);
    if (!ok) {
        fprintf(stderr, "ERROR: storm: cannot create threads\n");
        exit(3);
    }
    struct itimerspec i2 = { { 0, (long)t2_period }, { 0, (long)t2_period } };
    struct itimerspec i3 = { { 0, (long)st.t3_period_ns }, { 0, (long)st.t3_period_ns } };
    struct itimerspec zero;
    memset(&zero, 0, sizeof(zero));
    /* Original bounds around actual kernel arming, no pacing delay. */
    uint64_t t3_start = monotonic_ns();
    REQUIRE(timer_settime(t3, 0, &i3, NULL) == 0);
    uint64_t t3_armed = monotonic_ns();
    REQUIRE(timer_settime(t2, 0, &i2, NULL) == 0);
    ROLE(main);
    waiter_loop(0);
    ROLE(main);
    for (int w = 1; w < storm_waiters; w++)
        pthread_join(waiters[w], NULL);
    timer_settime(t2, 0, &zero, NULL);
    st.churn_on = 0;
    diag[D_churn_on] = 0;
    st.round_over = 1;
    ROLE(main);
    pthread_join(ctl, NULL);
    if (storm_poker)
        pthread_join(poker, NULL);
    ROLE(main);
    pthread_join(sender, NULL);
    ROLE(main);
    pthread_join(churn, NULL);
    /* Original synchronous cut: every thread eligible for t3 has completed
     * before main blocks it; SIGEV_THREAD callbacks inherit a blocked mask. */
    ROLE(main);
    for (long i = 0; i < churn_created; ++i) wait_sem(&churn_done);
    REQUIRE(st.churn_live == 0);
    REQUIRE(sem_destroy(&churn_slots) == 0);
    REQUIRE(sem_destroy(&churn_done) == 0);
    sigset_t s3;
    sigemptyset(&s3);
    sigaddset(&s3, sig_t3);
    pthread_sigmask(SIG_BLOCK, &s3, NULL);
    /* The kernel counts the expirations up to the moment it dequeues the
     * signal (it forwards the timer then); wait for one to be queued, as a
     * loaded host can delay the timer interrupt by several periods.
     */
    siginfo_t si3;
    struct timespec wait3 = { 60, 0 };
    uint64_t t3_cut = monotonic_ns();
    ROLE(main);
    int got3 = sigtimedwait(&s3, &si3, &wait3) == sig_t3;
    uint64_t t3_end = monotonic_ns();
    if (got3)
        st.t3_acct += 1 + si3.si_overrun;
    timer_settime(t3, 0, &zero, NULL);
    int fails = 0;

    long acct = st.t3_acct;
    if (st.hang > 0 || st.lost > 0 || st.q_dup > 0) {
        fprintf(stderr, "  storm round %d: hang %ld lost %ld duplicated %ld\n", round,
                st.hang, st.lost, st.q_dup);
        fails++;
    }
    REQUIRE(got3 && si3.si_code == SI_TIMER && si3.si_overrun >= 0);
    REQUIRE(st.q_seen_n == STORM_Q && st.q_dup == 0 && st.lost == 0 && st.hang == 0);
    REQUIRE(acct > 0);
    long expected_min = (long)((t3_cut - t3_armed) / st.t3_period_ns);
    long expected_max = (long)((t3_end - t3_start) / st.t3_period_ns);
    int counted = storm_accounted(t3_start, t3_armed, t3_cut, t3_end, st.t3_period_ns, acct, got3);
    printf("storm-oracle round=%d accounted=%ld expected_min=%ld expected_max=%ld tolerance=2 failures=%d\n",
           round, acct, expected_min, expected_max, !counted);
    if (!counted) fails++;
    for (int q = 1; q <= STORM_Q; ++q) REQUIRE(st.q_seen[q] == 1);
    for (int w = 0; w < storm_waiters; ++w) REQUIRE(st.weintr[w] >= 10);
    printf("round=%d queued=24 delivered=24 duplicate=0 lost=0 waiters=%d churn=%ld notifications=%ld accounted=%ld duration_draw=%" PRIu64 " time_oracle=LIVE\n",
           round, storm_waiters, (long)churn_created, (long)current_notification->notifications, acct, dur);
    for (int w = 0; w < STORM_WAITERS; w++)
        *eintr_total += st.weintr[w];
    ROLE(main);
    return fails;
}


int main(int argc, char **argv)
{
    REQUIRE(setvbuf(stdout, NULL, _IOLBF, 0) == 0);
    REQUIRE(argc == 5);
    diag_init();
    uint64_t seed = strtoull(argv[1], NULL, 0);
    storm_waiters = atoi(argv[2]); storm_poker = atoi(argv[3]); storm_churn_max = atol(argv[4]);
    REQUIRE(storm_waiters >= 1 && storm_waiters <= STORM_WAITERS);
    REQUIRE(storm_poker == 0 || storm_poker == 1);
    REQUIRE(storm_churn_max >= 1 && storm_churn_max <= UINT_MAX);
    REQUIRE(sem_init(&progress, 0, 0) == 0);
    REQUIRE(sem_init(&queue_ack, 0, 0) == 0);
    for (int w = 0; w < STORM_WAITERS; ++w) REQUIRE(pipe(wpipe[w]) == 0);
    sig_t1 = SIGRTMIN + 10; sig_t3 = SIGRTMIN + 11; sig_q = SIGRTMIN + 12;
    set_handler(sig_t1, h_t1, 0, 0);
    set_handler(sig_t3, h_t3, SA_RESTART, 0);
    set_handler(sig_q, h_q, SA_RESTART, 0);
    rng_t r = { seed * 0x9e3779b97f4a7c15ULL + 3 };
    long eintr = 0, notify = 0;
    int failures = 0;
    for (int i = 0; i < 40; ++i) {
        current_notification = &notification[i];
        timer_t t2, t3;
        struct sigevent sev = {0};
        sev.sigev_notify = SIGEV_THREAD; sev.sigev_notify_function = t2_notify;
        sev.sigev_value.sival_ptr = current_notification;
        REQUIRE(timer_create(CLOCK_MONOTONIC, &sev, &t2) == 0);
        memset(&sev, 0, sizeof(sev));
        sev.sigev_notify = SIGEV_SIGNAL; sev.sigev_signo = sig_t3;
        REQUIRE(timer_create(CLOCK_MONOTONIC, &sev, &t3) == 0);
        failures += storm_round(i, &r, t2, t3, &eintr);
        notify += current_notification->notifications;
        REQUIRE(timer_delete(t2) == 0 && timer_delete(t3) == 0);
    }
    REQUIRE(eintr >= 400 && notify >= 40);
    printf("storm-state rounds=40 queued=960 delivered=960 duplicate=0 lost=0 eintr=%ld notifications=%ld time_oracle=LIVE\n", eintr, notify);
    return failures != 0;
}
