/* Derived from DR:a64_prop_signal/itimer at 356c1821045a4b5678d0813e132074324623df6e.
 * Copyright (c) 2026 Google, Inc. See LICENSE for redistribution terms.
 * Retains generated work and the original model. Independent real-clock
 * observations are authorized only for these subject-timer comparisons. */
#include "oracle.h"

static pid_t main_tid;
static _Atomic int wrong_thread, delivered[3];
static volatile sig_atomic_t log_count, log_overflow, active_clock;
static int64_t expiry_log[IT_MAX_LOG];
static int64_t thread_cpu_us(void)
{
    struct timespec t;
    REQUIRE(clock_gettime(CLOCK_THREAD_CPUTIME_ID, &t) == 0);
    return (int64_t)t.tv_sec * 1000000 + t.tv_nsec / 1000;
}
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
    if (syscall(SYS_gettid) != main_tid) {
        atomic_fetch_add(&wrong_thread, 1);
        return;
    }
    int n = log_count;
    if (n < IT_MAX_LOG) {
        expiry_log[n] = timer_clock(active_clock);
        log_count = n + 1;
    } else log_overflow = 1;
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
    REQUIRE(argc == 4);
    const char *profile = argv[3];
    REQUIRE(strcmp(profile, "native") == 0 || strcmp(profile, "default") == 0 ||
            strcmp(profile, "dr-itimers") == 0 || strcmp(profile, "prof-pcs") == 0);
    if (strcmp(profile, "dr-itimers") == 0) {
        it_mux[ITIMER_REAL] = 3000; it_mux[ITIMER_VIRTUAL] = 4000; it_mux[ITIMER_PROF] = 5000;
    } else if (strcmp(profile, "prof-pcs") == 0) it_mux[ITIMER_VIRTUAL] = 10000;
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
    unsigned ops = 0, helper_ops = 0, compared = 0, stalled = 0;
    int failures = 0;
    for (int k = 0; k < 7; ++k) {
        itcase_t c;
        gen_itcase(seed + k, &c);
        struct itimerval zero = {0};
        REQUIRE(setitimer(c.which, &zero, NULL) == 0);
        active_clock = c.which;
        log_count = log_overflow = 0;
        atomic_store(&wrong_thread, 0);
        model_trace_t trace = { .report = 1 };
        int64_t wlast = timer_clock(ITIMER_REAL), clast = thread_cpu_us();
        int64_t expected_interval = 0;
        printf("case seed=%" PRIu64 " which=%d nops=%d\n", c.seed, c.which, c.nops);
        for (int i = 0; i <= c.nops; ++i) {
            itop_t final = { IT_SET, 0, 0, 0, 0, c.which == ITIMER_REAL ? 50000 : 100000 };
            const itop_t *o = i < c.nops ? &c.op[i] : &final;
            struct itimerval old = {0};
            unsigned alarm_old = 0;
            trace.op[i].t0 = timer_clock(c.which);
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
            trace.op[i].t1 = timer_clock(c.which);
            trace.op[i].old = old;
            trace.op[i].alarm_old = alarm_old;
            REQUIRE(old.it_value.tv_sec >= 0 && old.it_value.tv_usec >= 0 && old.it_value.tv_usec < 1000000);
            if (o->op != IT_ALARM)
                REQUIRE(old.it_interval.tv_sec * 1000000 + old.it_interval.tv_usec == expected_interval);
            if (o->op != IT_GET)
                expected_interval = o->op == IT_ALARM ? 0 : o->interval;
            printf("op case=%d index=%d kind=%d other=%d value=%" PRId64 " interval=%" PRId64 " alarm=%d wait=%" PRId64 " old=%ld:%ld old_alarm=%u\n", k, i, o->op, o->other_thread, o->value, o->interval, o->alarm_sec, o->wait, (long)old.it_value.tv_sec, (long)old.it_value.tv_usec, alarm_old);
            /* Existing original workload only. CLOCK_MONOTONIC, process
             * user CPU and process CPU supply references independent of the
             * setitimer/getitimer values being tested. The original classifier
             * measures off-CPU gaps across calls as well as the work below. */
            int64_t until = trace.op[i].t1 + o->wait;
            while (timer_clock(c.which) < until) {
                int64_t w = timer_clock(ITIMER_REAL), cpu = thread_cpu_us();
                int64_t gap = (w - wlast) - (cpu - clast);
                if (gap > trace.max_gap) trace.max_gap = gap;
                wlast = w; clast = cpu;
            }
            ops++;
        }
        REQUIRE(setitimer(c.which, &zero, NULL) == 0);
        /* As in the source, final disarm precedes taking the observed log.
         * Log truncation is a failure, not silently incomplete evidence. */
        REQUIRE(!log_overflow);
        trace.nlog = log_count;
        trace.wrong_thread = atomic_load(&wrong_thread);
        memcpy(trace.log, expiry_log, (size_t)trace.nlog * sizeof(trace.log[0]));
        itres_t result = {0};
        long before_suppressed = it_suppressed;
        int failed = model_check(&c, &trace, &result, strcmp(profile, "native") != 0);
        failures += failed;
        if (result.stalled) stalled++; else compared++;
        printf("timer-oracle case=%d seed=%" PRIu64 " profile=%s failures=%d stalled=%d suppressed=%ld deliveries=%d expected=%d max_gap_us=%" PRId64 " values_checked=%ld values_ambiguous=%ld first_required=%ld periodic_required=%ld observed_checked=%ld\n",
               k, c.seed, profile, failed, result.stalled, it_suppressed - before_suppressed,
               trace.nlog, result.nexp, trace.max_gap, model_coverage.values_checked,
               model_coverage.values_ambiguous, model_coverage.first_required,
               model_coverage.periodic_required, model_coverage.observed_checked);
    }
    if (!nohelper) {
        REQUIRE(pthread_mutex_lock(&lock) == 0);
        quit = 1;
        REQUIRE(pthread_cond_signal(&request) == 0);
        REQUIRE(pthread_mutex_unlock(&lock) == 0);
        REQUIRE(pthread_join(thread, NULL) == 0);
    }
    REQUIRE(atomic_load(&wrong_thread) == 0);
    printf("itimer-state cases=7 operations=%u helper_ops=%u wrong_thread=0 deliveries=%d,%d,%d time_oracle=LIVE\n", ops, helper_ops, delivered[0], delivered[1], delivered[2]);
    printf("timer-oracle-summary cases=7 compared=%u inconclusive=%u suppressed=%ld failures=%d\n",
           compared, stalled, it_suppressed, failures);
    /* Paired native/DR coverage floor is enforced by the external Rust runner,
     * exactly as the source run_itimer checks after both observations exist. */
    return failures != 0;
}
