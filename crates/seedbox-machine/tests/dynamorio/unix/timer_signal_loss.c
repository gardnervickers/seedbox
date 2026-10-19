/* Source: Seedbox f32472b7fe470367b7eddecde1ac53ad299f3a3e tools/jvm-fuzz/repros/timer_signal_loss.c.
 * Readiness delays replaced with positive kernel/pipe/semaphore acknowledgements. */
/* timer_signal_loss MODE [ROUNDS]: 100 one-shot SIGEV_SIGNAL timers per round send
 * a process-directed realtime signal (with a value) that only the main thread
 * leaves unblocked. MODE plain: nothing else. MODE thread: each round also arms
 * one SIGEV_THREAD timer (1 ms) just before them, as cfz's timerstorm does; glibc
 * starts its timer helper thread at the first timer_create and a new thread for
 * each expiration, which runs with the mask timer_create was called with. MODE
 * helper: creates that timer without arming it. MODE masked: arms it but calls
 * timer_create with the signal blocked, so its threads never take the signal.
 * Natively prints "missing 0" for every round and "OK"; under the translator a
 * round (usually the first) can lose up to all 100. */
#define _GNU_SOURCE
#include "support.h"
#include <semaphore.h>
#include <errno.h>
#include <pthread.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <unistd.h>

#define N 100
static volatile int seen[N], total, notified;
static sem_t received, notifications;
static void h(int s, siginfo_t *si, void *uc) {
    (void)s; (void)uc;
    int v = si->si_value.sival_int;
    if (v >= 0 && v < N) __atomic_add_fetch(&seen[v], 1, __ATOMIC_RELAXED);
    __atomic_add_fetch(&total, 1, __ATOMIC_RELAXED);
    sem_post(&received);
}
static void notify(union sigval v) { (void)v; __atomic_add_fetch(&notified, 1, __ATOMIC_RELAXED); sem_post(&notifications); }
static void *blocker(void *p) {
    (void)p;
    for (;;) pause();
    return NULL;
}
int main(int argc, char **argv) {
    sem_init(&received, 0, 0); sem_init(&notifications, 0, 0);
    int masked = argc > 1 && !strcmp(argv[1], "masked");
    int thread = argc > 1 && (!strcmp(argv[1], "thread") || !strcmp(argv[1], "helper") || masked);
    int arm = argc > 1 && (!strcmp(argv[1], "thread") || masked);
    int rounds = argc > 2 ? atoi(argv[2]) : 5;
    int sig = SIGRTMIN + 9;
    struct sigaction a;
    memset(&a, 0, sizeof a);
    a.sa_sigaction = h;
    a.sa_flags = SA_SIGINFO | SA_RESTART;
    sigemptyset(&a.sa_mask);
    sigaction(sig, &a, NULL);
    sigset_t s;
    sigemptyset(&s);
    sigaddset(&s, sig);
    pthread_sigmask(SIG_BLOCK, &s, NULL);
    pthread_t b;
    pthread_create(&b, NULL, blocker, NULL); /* inherits the blocked signal */
    pthread_sigmask(SIG_UNBLOCK, &s, NULL);
    int bad = 0;
    for (int r = 0; r < rounds; r++) {
        memset((void *)seen, 0, sizeof seen);
        total = 0;
        timer_t tm[N + 1];
        struct sigevent ev;
        if (thread) {
            memset(&ev, 0, sizeof ev);
            ev.sigev_notify = SIGEV_THREAD;
            ev.sigev_notify_function = notify;
            if (masked) pthread_sigmask(SIG_BLOCK, &s, NULL);
            if (timer_create(CLOCK_MONOTONIC, &ev, &tm[N])) { perror("timer_create"); return 2; }
            if (masked) pthread_sigmask(SIG_UNBLOCK, &s, NULL);
            struct itimerspec its = {{0, 0}, {0, 1000000L}};
            if (arm) timer_settime(tm[N], 0, &its, NULL);
        }
        for (int i = 0; i < N; i++) {
            memset(&ev, 0, sizeof ev);
            ev.sigev_notify = SIGEV_SIGNAL;
            ev.sigev_signo = sig;
            ev.sigev_value.sival_int = i;
            if (timer_create(CLOCK_MONOTONIC, &ev, &tm[i])) { perror("timer_create"); return 2; }
        }
        for (int i = 0; i < N; i++) {
            struct itimerspec its = {{0, 0}, {0, 1000000L + (i % 20) * 100000L}};
            timer_settime(tm[i], 0, &its, NULL);
        }
        for (int i = 0; i < N; i++) while (sem_wait(&received) && errno == EINTR) {}
        if (arm) while (sem_wait(&notifications) && errno == EINTR) {}
        int missing = 0;
        for (int i = 0; i < N; i++) missing += seen[i] != 1;
        printf("round %d missing %d\n", r, missing);
        bad |= missing != 0;
        for (int i = 0; i < N; i++) timer_delete(tm[i]);
        if (thread) timer_delete(tm[N]);
    }
    printf("%s\n", bad ? "BAD" : "OK");
    fflush(stdout);
    _exit(bad);
}
