/* Source-derived from Seedbox PR317 f32472b7fe470367b7eddecde1ac53ad299f3a3e,
 * tools/jvm-fuzz/repros/srsample.c. No DR internal API or layout is used.
 * Adaptations are described in inventory.tsv and the integration handoff. */
/*
 * HotSpot-style suspend/resume sampling (what JFR's execution sampler does) against a worker that
 * keeps known values in callee-saved general and SIMD registers. Natively it prints "OK".
 *
 * The sampler sends SIGUSR2; the worker's handler saves its context, posts "suspended" and waits in
 * sigsuspend() for a second SIGUSR2 (resume), as HotSpot's SR_handler does. The sampler reads the
 * saved pc/sp while the worker is stopped, then resumes it.
 *
 * Usage: srsample SAMPLES [WORKERS]
 */
#define _GNU_SOURCE
#include "support.h"
#include <pthread.h>
#include <semaphore.h>
#include <signal.h>
#include <stdatomic.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <ucontext.h>
#include <unistd.h>

enum { RUNNING, SUSPEND_REQUEST, SUSPENDED, RESUME_REQUEST };

struct worker {
    pthread_t thread;
    _Atomic int state;
    sem_t suspended, resumed;
    _Atomic uint64_t pc, sp;
    _Atomic long rounds, errors;
    uint64_t expected[18];
    uint64_t seen[18];
};

static __thread struct worker *self;
static atomic_int stop;
static sem_t started;

static void sr_handler(int sig, siginfo_t *info, void *uc) {
    (void)sig;
    (void)info;
    struct worker *w = self;
    if (!w) return;
    int st = atomic_load(&w->state);
    if (st == SUSPEND_REQUEST) {
        ucontext_t *u = uc;
        atomic_store(&w->pc, u->uc_mcontext.pc);
        atomic_store(&w->sp, u->uc_mcontext.sp);
        atomic_store(&w->state, SUSPENDED);
        sem_post(&w->suspended);
        sigset_t wait;
        pthread_sigmask(SIG_SETMASK, NULL, &wait);
        sigdelset(&wait, SIGUSR2);
        while (atomic_load(&w->state) != RESUME_REQUEST) sigsuspend(&wait);
        atomic_store(&w->state, RUNNING);
        sem_post(&w->resumed);
    }
    /* A resume signal that interrupts sigsuspend above just returns here. */
}

/* Loads known values into x19-x28 and d8-d15, spins, then stores what the registers hold. */
static void check_registers(struct worker *w, uint64_t spins) {
    uint64_t *e = w->expected, *s = w->seen;
    __asm__ volatile(
        "ldp x19, x20, [%[e], #0]\n"
        "ldp x21, x22, [%[e], #16]\n"
        "ldp x23, x24, [%[e], #32]\n"
        "ldp x25, x26, [%[e], #48]\n"
        "ldp x27, x28, [%[e], #64]\n"
        "ldp d8, d9, [%[e], #80]\n"
        "ldp d10, d11, [%[e], #96]\n"
        "ldp d12, d13, [%[e], #112]\n"
        "ldp d14, d15, [%[e], #128]\n"
        "mov x9, %[n]\n"
        "1:\n"
        "add x10, x19, x9\n"
        "eor x11, x10, x28\n"
        "fadd d0, d8, d15\n"
        "fmul d1, d0, d9\n"
        "subs x9, x9, #1\n"
        "b.ne 1b\n"
        "stp x19, x20, [%[s], #0]\n"
        "stp x21, x22, [%[s], #16]\n"
        "stp x23, x24, [%[s], #32]\n"
        "stp x25, x26, [%[s], #48]\n"
        "stp x27, x28, [%[s], #64]\n"
        "stp d8, d9, [%[s], #80]\n"
        "stp d10, d11, [%[s], #96]\n"
        "stp d12, d13, [%[s], #112]\n"
        "stp d14, d15, [%[s], #128]\n"
        :
        : [e] "r"(e), [s] "r"(s), [n] "r"(spins)
        : "x9", "x10", "x11", "x19", "x20", "x21", "x22", "x23", "x24", "x25", "x26", "x27", "x28",
          "d0", "d1", "d8", "d9", "d10", "d11", "d12", "d13", "d14", "d15", "memory", "cc");
}

static void *work(void *arg) {
    struct worker *w = arg;
    self = w;
    sem_post(&started);
    for (int i = 0; i < 18; i++) w->expected[i] = 0x1111111111111111ULL * (i + 1) ^ (uintptr_t)w;
    while (!atomic_load(&stop)) {
        memset(w->seen, 0, sizeof(w->seen));
        check_registers(w, 100000);
        if (memcmp(w->seen, w->expected, sizeof(w->seen))) {
            atomic_fetch_add(&w->errors, 1);
            for (int i = 0; i < 18; i++)
                if (w->seen[i] != w->expected[i])
                    fprintf(stderr, "worker %p: %s%d = %#llx, expected %#llx\n", (void *)w, i < 10 ? "x" : "d",
                            i < 10 ? 19 + i : i - 2, (unsigned long long)w->seen[i], (unsigned long long)w->expected[i]);
        }
        atomic_fetch_add(&w->rounds, 1);
    }
    return NULL;
}

int main(int argc, char **argv) {
    long samples = argc > 1 ? atol(argv[1]) : 10000;
    int n = argc > 2 ? atoi(argv[2]) : 2;
    struct sigaction sa;
    memset(&sa, 0, sizeof(sa));
    sa.sa_sigaction = sr_handler;
    sa.sa_flags = SA_SIGINFO | SA_RESTART;
    sigemptyset(&sa.sa_mask);
    sigaction(SIGUSR2, &sa, NULL);
    struct worker *ws = calloc(n, sizeof(*ws));
    sem_init(&started, 0, 0);
    for (int i = 0; i < n; i++) {
        sem_init(&ws[i].suspended, 0, 0);
        sem_init(&ws[i].resumed, 0, 0);
        pthread_create(&ws[i].thread, NULL, work, &ws[i]);
    }
    /* A suspend request sent before a worker set `self` would be dropped. */
    for (int i = 0; i < n; i++) sem_wait(&started);
    uint64_t pcs = 0;
    for (long k = 0; k < samples; k++) {
        struct worker *w = &ws[k % n];
        atomic_store(&w->state, SUSPEND_REQUEST);
        pthread_kill(w->thread, SIGUSR2);
        sem_wait(&w->suspended);
        pcs ^= atomic_load(&w->pc) + atomic_load(&w->sp);
        atomic_store(&w->state, RESUME_REQUEST);
        pthread_kill(w->thread, SIGUSR2);
        sem_wait(&w->resumed);
        /* Positive work acknowledgement: the sampled thread must finish a register round. */
        long previous = atomic_load(&w->rounds);
        while (atomic_load(&w->rounds) <= previous) sched_yield();
    }
    atomic_store(&stop, 1);
    long errors = 0, rounds = 0;
    for (int i = 0; i < n; i++) {
        pthread_join(ws[i].thread, NULL);
        errors += ws[i].errors;
        rounds += ws[i].rounds;
    }
    fprintf(stderr, "samples %ld rounds %ld errors %ld (pcs %#llx)\n", samples, rounds, errors, (unsigned long long)pcs);
    printf(errors ? "BAD %ld\n" : "OK\n", errors);
    return errors != 0 || rounds == 0;
}
