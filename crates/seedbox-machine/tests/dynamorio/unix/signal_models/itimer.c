/* Derived from DR:a64_prop_signal/itimer at 356c1821045a4b5678d0813e132074324623df6e.
 * Copyright (c) 2026 Google, Inc. See LICENSE for redistribution terms.
 * Retains all generated operations. Live checks deliberately exclude the
 * elapsed-clock oracle; oracle.h retains that model for synthetic inputs. */
#include "timer_model.h"

static pid_t main_tid;
static _Atomic int wrong_thread, delivered[3];
static pthread_mutex_t lock = PTHREAD_MUTEX_INITIALIZER;
static pthread_cond_t request = PTHREAD_COND_INITIALIZER;
static pthread_cond_t done = PTHREAD_COND_INITIALIZER;
static const itop_t *pending;
static int quit;
static struct itimerval result_old;
static unsigned result_alarm;
static int result_errno;

static int64_t timer_clock(int which)
{
    if (which == ITIMER_VIRTUAL) {
        struct rusage ru;
        REQUIRE(getrusage(RUSAGE_SELF, &ru) == 0);
        return (int64_t)ru.ru_utime.tv_sec * 1000000 + ru.ru_utime.tv_usec;
    }
    struct timespec t;
    REQUIRE(clock_gettime(which == ITIMER_REAL ? CLOCK_MONOTONIC : CLOCK_PROCESS_CPUTIME_ID, &t) == 0);
    return (int64_t)t.tv_sec * 1000000 + t.tv_nsec / 1000;
}
static void handler(int sig, siginfo_t *si, void *uc)
{
    (void)si; (void)uc;
    if (syscall(SYS_gettid) != main_tid) atomic_fetch_add(&wrong_thread, 1);
    atomic_fetch_add(&delivered[sig == SIGALRM ? 0 : sig == SIGVTALRM ? 1 : 2], 1);
}
static int operate(const itop_t *o, int which, struct itimerval *old, unsigned *alarm_old)
{
    if (o->op == IT_ALARM) { *alarm_old = alarm((unsigned)o->alarm_sec); return 0; }
    if (o->op == IT_GET) return getitimer(which, old);
    struct itimerval value = {
        { o->interval / 1000000, o->interval % 1000000 },
        { o->value / 1000000, o->value % 1000000 }
    };
    return setitimer(which, &value, old);
}
static void *helper(void *unused)
{
    (void)unused;
    REQUIRE(pthread_mutex_lock(&lock) == 0);
    for (;;) {
        while (!pending && !quit) REQUIRE(pthread_cond_wait(&request, &lock) == 0);
        if (quit) break;
        result_errno = operate(pending, ITIMER_REAL, &result_old, &result_alarm);
        pending = NULL;
        REQUIRE(pthread_cond_signal(&done) == 0);
    }
    REQUIRE(pthread_mutex_unlock(&lock) == 0);
    return NULL;
}
int main(int argc, char **argv)
{
    /* Explicit helper policy matches source defaults: multiplexed profiles use
     * main-thread calls unless the source helper axis is explicitly requested. */
    REQUIRE(setvbuf(stdout, NULL, _IOLBF, 0) == 0);
    REQUIRE(argc == 3);
    uint64_t seed = strtoull(argv[1], NULL, 0);
    int nohelper = atoi(argv[2]);
    REQUIRE(nohelper == 0 || nohelper == 1);
    main_tid = syscall(SYS_gettid);
    set_handler(SIGALRM, handler, SA_RESTART, 0);
    set_handler(SIGVTALRM, handler, SA_RESTART, 0);
    set_handler(SIGPROF, handler, SA_RESTART, 0);
    sigset_t all, oldmask;
    sigfillset(&all);
    REQUIRE(pthread_sigmask(SIG_BLOCK, &all, &oldmask) == 0);
    pthread_t thread;
    if (!nohelper) REQUIRE(pthread_create(&thread, NULL, helper, NULL) == 0);
    REQUIRE(pthread_sigmask(SIG_SETMASK, &oldmask, NULL) == 0);
    unsigned ops = 0, helper_ops = 0;
    for (int k = 0; k < 7; ++k) {
        itcase_t c;
        gen_itcase(seed + k, &c);
        struct itimerval zero = {0};
        REQUIRE(setitimer(c.which, &zero, NULL) == 0);
        int64_t expected_interval = 0;
        printf("case seed=%" PRIu64 " which=%d nops=%d\n", c.seed, c.which, c.nops);
        for (int i = 0; i <= c.nops; ++i) {
            itop_t final = { IT_SET, 0, 0, 0, 0, c.which == ITIMER_REAL ? 50000 : 100000 };
            const itop_t *o = i < c.nops ? &c.op[i] : &final;
            struct itimerval old = {0};
            unsigned alarm_old = 0;
            if (o->other_thread && !nohelper) {
                REQUIRE(pthread_mutex_lock(&lock) == 0);
                pending = o;
                memset(&result_old, 0, sizeof(result_old));
                result_alarm = 0;
                REQUIRE(pthread_cond_signal(&request) == 0);
                while (pending) REQUIRE(pthread_cond_wait(&done, &lock) == 0);
                REQUIRE(result_errno == 0);
                old = result_old; alarm_old = result_alarm;
                REQUIRE(pthread_mutex_unlock(&lock) == 0);
                helper_ops++;
            } else REQUIRE(operate(o, c.which, &old, &alarm_old) == 0);
            REQUIRE(old.it_value.tv_sec >= 0 && old.it_value.tv_usec >= 0 && old.it_value.tv_usec < 1000000);
            if (o->op != IT_ALARM)
                REQUIRE(old.it_interval.tv_sec * 1000000 + old.it_interval.tv_usec == expected_interval);
            if (o->op != IT_GET)
                expected_interval = o->op == IT_ALARM ? 0 : o->interval;
            printf("op case=%d index=%d kind=%d other=%d value=%" PRId64 " interval=%" PRId64 " alarm=%d wait=%" PRId64 " old=%ld:%ld old_alarm=%u\n", k, i, o->op, o->other_thread, o->value, o->interval, o->alarm_sec, o->wait, (long)old.it_value.tv_sec, (long)old.it_value.tv_usec, alarm_old);
            /* Original scheduled workload, not an elapsed-time assertion:
             * process/user CPU clocks must advance to exercise both CPU timers.
             * Containment belongs to the external process supervisor. No
             * delivery absence or elapsed-time result is used as success. */
            int64_t until = timer_clock(c.which) + o->wait;
            while (timer_clock(c.which) < until) atomic_signal_fence(memory_order_seq_cst);
            ops++;
        }
        REQUIRE(setitimer(c.which, &zero, NULL) == 0);
    }
    if (!nohelper) {
        REQUIRE(pthread_mutex_lock(&lock) == 0);
        quit = 1;
        REQUIRE(pthread_cond_signal(&request) == 0);
        REQUIRE(pthread_mutex_unlock(&lock) == 0);
        REQUIRE(pthread_join(thread, NULL) == 0);
    }
    REQUIRE(atomic_load(&wrong_thread) == 0);
    printf("itimer-state cases=7 operations=%u helper_ops=%u wrong_thread=0 deliveries=%d,%d,%d time_oracle=PENDING\n", ops, helper_ops, delivered[0], delivered[1], delivered[2]);
    return 0;
}
