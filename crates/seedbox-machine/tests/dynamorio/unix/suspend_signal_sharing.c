/* Source-derived from Seedbox PR317 f32472b7fe470367b7eddecde1ac53ad299f3a3e,
 * tools/jvm-fuzz/repros/suspend_signal_sharing.c. No DR internal API or layout is used.
 * Adaptations are described in inventory.tsv and the integration handoff. */
/*
 * The application handles a signal the translator also uses internally. A sender
 * thread queues N of them (rt_tgsigqueueinfo, SI_QUEUE, numbered values) to the
 * computing main thread, waiting for each handler run, while a JIT thread rewrites
 * and flushes code (each IC IVAU flush makes the translator stop all threads).
 *
 *   suspend_signal_sharing SIGNAL N [flush|noflush]     SIGNAL: ILL, STKFLT, USR2, ...
 *
 * Natively every signal is handled once, in order, and the run finishes. Prints OK.
 */
#define _GNU_SOURCE
#include "support.h"
#include <linux/futex.h>
#include <pthread.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <time.h>
#include <unistd.h>

static pid_t main_tid;
static int sig;
static volatile int ack, done, stop_flush;
static long got, in_order, next_val, unacked;
static int odd_code = 1, odd_pid, odd_val;
static long odd_expect; /* the first siginfo that is not as sent */
static void handler(int s, siginfo_t *si, void *uc) {
    (void)s; (void)uc;
    if (odd_code == 1 && (si->si_code != SI_QUEUE || si->si_pid != getpid() || si->si_value.sival_int != next_val)) {
        odd_code = si->si_code;
        odd_pid = si->si_pid;
        odd_val = si->si_value.sival_int;
        odd_expect = next_val;
    }
    if (si->si_code == SI_QUEUE) {
        got++;
        in_order += si->si_value.sival_int == next_val;
        next_val = si->si_value.sival_int + 1;
    }
    __atomic_add_fetch(&ack, 1, __ATOMIC_SEQ_CST);
    syscall(SYS_futex, &ack, FUTEX_WAKE_PRIVATE, 1, NULL, NULL, 0);
}
static uint64_t now_ns(void) {
    struct timespec t;
    clock_gettime(CLOCK_MONOTONIC, &t);
    return (uint64_t)t.tv_sec * 1000000000ULL + (uint64_t)t.tv_nsec;
}
static void *sender(void *p) {
    long n = (long)p;
    for (long i = 0; i < n; i++) {
        int a = ack;
        siginfo_t si;
        memset(&si, 0, sizeof si);
        si.si_signo = sig;
        si.si_code = SI_QUEUE;
        si.si_pid = getpid();
        si.si_uid = getuid();
        si.si_value.sival_int = (int)i;
        syscall(SYS_rt_tgsigqueueinfo, getpid(), main_tid, sig, &si);
        uint64_t end = now_ns() + 5000000000ULL;
        while (ack == a) {
            if (now_ns() > end) {
                unacked++;
                fprintf(stderr, "signal %ld not handled within 5 s\n", i);
                break;
            }
            struct timespec t = {0, 10000000};
            syscall(SYS_futex, &ack, FUTEX_WAIT_PRIVATE, a, &t, NULL, 0);
        }
        if (unacked > 3) break;
    }
    done = 1;
    return NULL;
}
static void *flusher(void *p) {
    (void)p;
    uint32_t *code = mmap(0, 4096, PROT_READ | PROT_WRITE | PROT_EXEC, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    for (long i = 0; !stop_flush; i++) {
        uint32_t *slot = code + (i % 64) * 4;
        unix_emit(slot, i & 0xffff);
        ((int (*)(void))slot)();
        sched_yield();
    }
    return NULL;
}

int main(int argc, char **argv) {
    const char *name = argc > 1 ? argv[1] : "STKFLT";
    long n = argc > 2 ? atol(argv[2]) : 2000;
    int flush = !(argc > 3 && !strcmp(argv[3], "noflush"));
    sig = !strcmp(name, "ILL") ? SIGILL : !strcmp(name, "STKFLT") ? SIGSTKFLT : !strcmp(name, "USR2") ? SIGUSR2
        : !strcmp(name, "USR1") ? SIGUSR1 : !strcmp(name, "PROF") ? SIGPROF : atoi(name);
    main_tid = gettid();
    struct sigaction a;
    memset(&a, 0, sizeof a);
    a.sa_sigaction = handler;
    a.sa_flags = SA_SIGINFO | SA_RESTART;
    sigaction(sig, &a, 0);
    pthread_t f, s;
    if (flush) pthread_create(&f, 0, flusher, 0);
    pthread_create(&s, 0, sender, (void *)n);
    volatile unsigned long x = 0;
    while (!done) x++;
    pthread_join(s, 0);
    stop_flush = 1;
    if (flush) pthread_join(f, 0);
    printf("signal %d (%s) %s: handled %ld/%ld inOrder %ld unacked %ld\n", sig, name, flush ? "flush" : "noflush", got, n,
           in_order, unacked);
    if (odd_code != 1)
        printf("first unexpected siginfo: si_code %d si_pid %s si_value %d (expected SI_QUEUE, own pid, %ld)\n", odd_code,
               odd_pid == getpid() ? "own" : "other", odd_val, odd_expect);
    int ok = got == n && in_order == n && !unacked;
    puts(ok ? "OK" : "FAIL");
    return !ok;
}
