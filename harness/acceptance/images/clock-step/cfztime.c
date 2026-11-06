/*
 * Clock-step consumers for harness/acceptance/workloads/clock-step.yaml:
 *   cfztime PORT
 * The oracle connects to PORT and drives phases with one command per line:
 *   arm PHASE MS      park every waiter below with a deadline MS from now
 *   report PHASE MS   at arm + MS (monotonic), report which waiters finished
 *   finish PHASE MS   join every waiter (bounded by arm + MS) and report
 * Waiters: a REALTIME timerfd with TFD_TIMER_ABSTIME|TFD_TIMER_CANCEL_ON_SET,
 * absolute REALTIME clock_nanosleep, timer_settime(TIMER_ABSTIME) and the
 * absolute REALTIME futex wait under glibc's pthread_cond_timedwait, plus MONOTONIC-absolute and relative waits that a
 * wall-clock step must not move. A watcher keeps a far-future cancel-on-set
 * timerfd and records every discontinuity. Each reply is one JSON line, also
 * printed on stdout; times are CLOCK_MONOTONIC with CLOCK_MONOTONIC_RAW
 * brackets. The oracle (driver/clock_step.py) owns every assertion.
 */
#define _GNU_SOURCE
#include <arpa/inet.h>
#include <errno.h>
#include <netinet/in.h>
#include <pthread.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/syscall.h>
#include <sys/timerfd.h>
#include <time.h>
#include <unistd.h>

/* Alpine's build-base has no kernel headers. */
#define FUTEX_WAIT_BITSET 9
#define FUTEX_PRIVATE_FLAG 128
#define FUTEX_CLOCK_REALTIME 256
#define FUTEX_BITSET_MATCH_ANY 0xffffffff

#ifndef SIGEV_THREAD_ID
#define SIGEV_THREAD_ID 4
#endif
#ifndef TFD_TIMER_CANCEL_ON_SET
#define TFD_TIMER_CANCEL_ON_SET (1 << 1)
#endif

typedef int64_t ns_t;

static ns_t now(clockid_t clock)
{
    struct timespec t;
    clock_gettime(clock, &t);
    return (ns_t)t.tv_sec * 1000000000 + t.tv_nsec;
}

static struct timespec spec(ns_t value)
{
    return (struct timespec){.tv_sec = value / 1000000000, .tv_nsec = value % 1000000000};
}

enum {
    TIMERFD_CANCEL,
    NANOSLEEP_REAL_ABS,
    TIMER_REAL_ABS,
    COND_REAL_ABS,
    NANOSLEEP_MONO_ABS,
    COND_MONO_ABS,
    NANOSLEEP_REL,
    NANOSLEEP_REAL_REL,
    WAITERS
};
static const char *NAMES[WAITERS] = {
    "timerfd_cancel", "nanosleep_real_abs", "timer_real_abs", "futex_real_abs",
    "nanosleep_mono_abs", "futex_mono_abs", "nanosleep_rel", "nanosleep_real_rel",
};

struct result {
    int done;
    long rc;      /* 0 on a normal expiry, otherwise -errno */
    ns_t mono;    /* CLOCK_MONOTONIC at the wakeup */
    ns_t raw_lo;  /* CLOCK_MONOTONIC_RAW just before and after that read */
    ns_t raw_hi;
};

static pthread_mutex_t lock = PTHREAD_MUTEX_INITIALIZER;
static struct result results[WAITERS];
static pthread_t threads[WAITERS];
static ns_t arm_mono, arm_real, deadline_ns;
static ns_t steps[64];
static int nsteps;

static void finish(int which, long rc)
{
    ns_t lo = now(CLOCK_MONOTONIC_RAW), mono = now(CLOCK_MONOTONIC), hi = now(CLOCK_MONOTONIC_RAW);
    pthread_mutex_lock(&lock);
    results[which] = (struct result){1, rc, mono, lo, hi};
    pthread_mutex_unlock(&lock);
}

static void *waiter(void *arg)
{
    int which = (int)(intptr_t)arg;
    ns_t real_deadline = arm_real + deadline_ns, mono_deadline = arm_mono + deadline_ns;
    long rc = 0;
    switch (which) {
    case TIMERFD_CANCEL: {
        int fd = timerfd_create(CLOCK_REALTIME, TFD_CLOEXEC);
        struct itimerspec value = {.it_value = spec(real_deadline)};
        if (fd < 0 || timerfd_settime(fd, TFD_TIMER_ABSTIME | TFD_TIMER_CANCEL_ON_SET, &value, 0)) {
            rc = -errno;
            break;
        }
        uint64_t expirations;
        ssize_t n;
        do n = read(fd, &expirations, sizeof expirations);
        while (n < 0 && errno == EINTR);
        rc = n < 0 ? -errno : 0;
        close(fd);
        break;
    }
    case NANOSLEEP_REAL_ABS: {
        struct timespec t = spec(real_deadline);
        while ((rc = clock_nanosleep(CLOCK_REALTIME, TIMER_ABSTIME, &t, 0)) == EINTR) {}
        rc = -rc;
        break;
    }
    case TIMER_REAL_ABS: {
        sigset_t set;
        sigemptyset(&set);
        sigaddset(&set, SIGRTMIN);
        /* Thread-directed: under the translator a process-directed signal never
         * reaches a sigwaitinfo() thread. */
        struct sigevent event = {.sigev_notify = SIGEV_THREAD_ID, .sigev_signo = SIGRTMIN};
        event.sigev_notify_thread_id = gettid();
        timer_t timer;
        struct itimerspec value = {.it_value = spec(real_deadline)};
        if (timer_create(CLOCK_REALTIME, &event, &timer) ||
            timer_settime(timer, TIMER_ABSTIME, &value, 0)) {
            rc = -errno;
            break;
        }
        siginfo_t info;
        while (sigwaitinfo(&set, &info) < 0 && errno == EINTR) {}
        timer_delete(timer);
        break;
    }
    case COND_REAL_ABS:
    case COND_MONO_ABS: {
        /* glibc's pthread_cond_timedwait: an absolute FUTEX_WAIT_BITSET on the
         * condition's clock. musl's converts to a relative wait at the call, so
         * the futex is called directly to test the kernel's semantics. */
        static uint32_t word;
        struct timespec t = spec(which == COND_REAL_ABS ? real_deadline : mono_deadline);
        int op = FUTEX_WAIT_BITSET | FUTEX_PRIVATE_FLAG | (which == COND_REAL_ABS ? FUTEX_CLOCK_REALTIME : 0);
        while ((rc = syscall(SYS_futex, &word, op, 0, &t, 0, FUTEX_BITSET_MATCH_ANY)) < 0 &&
               (errno == EINTR || errno == EAGAIN)) {}
        rc = rc < 0 && errno == ETIMEDOUT ? 0 : rc < 0 ? -errno : -1000;
        break;
    }
    case NANOSLEEP_MONO_ABS: {
        struct timespec t = spec(mono_deadline);
        while ((rc = clock_nanosleep(CLOCK_MONOTONIC, TIMER_ABSTIME, &t, 0)) == EINTR) {}
        rc = -rc;
        break;
    }
    case NANOSLEEP_REL:
    case NANOSLEEP_REAL_REL: {
        /* Relative to the arm time, so a late thread start cannot hide a short wait. */
        ns_t left = mono_deadline - now(CLOCK_MONOTONIC);
        struct timespec t = spec(left > 0 ? left : 0), rest;
        for (;;) {
            rc = which == NANOSLEEP_REL ? (nanosleep(&t, &rest) ? errno : 0)
                                        : clock_nanosleep(CLOCK_REALTIME, 0, &t, &rest);
            if (rc != EINTR) break;
            t = rest;
        }
        rc = -rc;
        break;
    }
    }
    finish(which, rc);
    return 0;
}

/* Records every wall-clock discontinuity: a cancel-on-set timerfd far ahead. */
static void *watcher(void *arg)
{
    (void)arg;
    int fd = timerfd_create(CLOCK_REALTIME, TFD_CLOEXEC);
    for (;;) {
        struct itimerspec value = {.it_value = spec(now(CLOCK_REALTIME) + 10LL * 365 * 86400 * 1000000000)};
        if (timerfd_settime(fd, TFD_TIMER_ABSTIME | TFD_TIMER_CANCEL_ON_SET, &value, 0)) {
            perror("watcher timerfd_settime");
            exit(1);
        }
        uint64_t expirations;
        if (read(fd, &expirations, sizeof expirations) < 0 && errno == ECANCELED) {
            ns_t at = now(CLOCK_MONOTONIC);
            pthread_mutex_lock(&lock);
            if (nsteps < 64) steps[nsteps++] = at;
            pthread_mutex_unlock(&lock);
        }
    }
    return 0;
}

static void reply(FILE *out, const char *kind, const char *phase)
{
    char text[4096];
    int n = snprintf(text, sizeof text, "{\"kind\":\"%s\",\"phase\":\"%s\",\"arm_mono\":%lld,"
                     "\"arm_real\":%lld,\"deadline_ns\":%lld,\"now\":%lld,\"waiters\":{",
                     kind, phase, (long long)arm_mono, (long long)arm_real,
                     (long long)deadline_ns, (long long)now(CLOCK_MONOTONIC));
    pthread_mutex_lock(&lock);
    for (int i = 0; i < WAITERS; i++)
        n += snprintf(text + n, sizeof text - n,
                      "%s\"%s\":{\"done\":%d,\"rc\":%ld,\"mono\":%lld,\"raw_lo\":%lld,\"raw_hi\":%lld}",
                      i ? "," : "", NAMES[i], results[i].done, results[i].rc,
                      (long long)results[i].mono, (long long)results[i].raw_lo,
                      (long long)results[i].raw_hi);
    n += snprintf(text + n, sizeof text - n, "},\"steps\":[");
    for (int i = 0; i < nsteps; i++)
        n += snprintf(text + n, sizeof text - n, "%s%lld", i ? "," : "", (long long)steps[i]);
    pthread_mutex_unlock(&lock);
    snprintf(text + n, sizeof text - n, "]}");
    printf("%s\n", text);
    fflush(stdout);
    fprintf(out, "%s\n", text);
    fflush(out);
}

static void sleep_until(ns_t mono)
{
    struct timespec t = spec(mono);
    while (clock_nanosleep(CLOCK_MONOTONIC, TIMER_ABSTIME, &t, 0) == EINTR) {}
}

int main(int argc, char **argv)
{
    if (argc != 2) {
        fprintf(stderr, "usage: cfztime PORT\n");
        return 2;
    }
    /* Only the timer waiter takes SIGRTMIN, through sigwaitinfo. */
    sigset_t set;
    sigemptyset(&set);
    sigaddset(&set, SIGRTMIN);
    pthread_sigmask(SIG_BLOCK, &set, 0);
    pthread_t watch;
    pthread_create(&watch, 0, watcher, 0);

    int listener = socket(AF_INET, SOCK_STREAM, 0), one = 1;
    setsockopt(listener, SOL_SOCKET, SO_REUSEADDR, &one, sizeof one);
    struct sockaddr_in address = {.sin_family = AF_INET, .sin_port = htons(atoi(argv[1]))};
    if (bind(listener, (struct sockaddr *)&address, sizeof address) || listen(listener, 1)) {
        perror("listen");
        return 1;
    }
    printf("CFZTIME_READY\n");
    fflush(stdout);
    int channel = accept(listener, 0, 0);
    if (channel < 0) {
        perror("accept");
        return 1;
    }
    FILE *in = fdopen(channel, "r"), *out = fdopen(dup(channel), "w");
    char line[256], command[32], phase[32];
    long ms;
    int armed = 0;
    while (fgets(line, sizeof line, in)) {
        if (sscanf(line, "%31s %31s %ld", command, phase, &ms) != 3) {
            fprintf(stderr, "bad command: %s", line);
            return 1;
        }
        if (!strcmp(command, "arm")) {
            if (armed) {
                fprintf(stderr, "arm %s: the previous phase was not finished\n", phase);
                return 1;
            }
            memset(results, 0, sizeof results);
            deadline_ns = ms * 1000000LL;
            arm_mono = now(CLOCK_MONOTONIC);
            arm_real = now(CLOCK_REALTIME);
            for (int i = 0; i < WAITERS; i++)
                pthread_create(&threads[i], 0, waiter, (void *)(intptr_t)i);
            armed = 1;
            reply(out, "armed", phase);
        } else if (!strcmp(command, "report")) {
            sleep_until(arm_mono + ms * 1000000LL);
            reply(out, "report", phase);
        } else if (!strcmp(command, "finish")) {
            ns_t bound = arm_mono + ms * 1000000LL;
            for (;;) {
                int done = 1;
                pthread_mutex_lock(&lock);
                for (int i = 0; i < WAITERS; i++) done &= results[i].done;
                pthread_mutex_unlock(&lock);
                if (done || now(CLOCK_MONOTONIC) >= bound) break;
                usleep(10000);
            }
            int done = 1;
            for (int i = 0; i < WAITERS; i++) done &= results[i].done;
            if (done)
                for (int i = 0; i < WAITERS; i++) pthread_join(threads[i], 0);
            armed = !done;
            reply(out, done ? "finished" : "unfinished", phase);
            if (!done) return 1;
        } else {
            fprintf(stderr, "unknown command: %s", line);
            return 1;
        }
    }
    printf("OK time\n");
    return 0;
}
