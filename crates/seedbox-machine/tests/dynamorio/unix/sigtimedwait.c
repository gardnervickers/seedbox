/* Source: Seedbox f32472b7fe470367b7eddecde1ac53ad299f3a3e tools/jvm-fuzz/repros/sigtimedwait.c.
 * Readiness delays replaced with positive kernel/pipe/semaphore acknowledgements. */
/* sigtimedwait MODE [ROUNDS]: a thread blocks SIGRTMIN+5 and takes it with sigtimedwait (2 s timeout).
 * MODE: before (the signal is queued to the waiting thread before it calls sigtimedwait),
 *       during (another thread queues it while the waiter is blocked in sigtimedwait),
 *       flush (as during, while a third thread rewrites and flushes code every 200 us).
 * Natively prints "got ROUNDS/ROUNDS" and "OK". */
#define _GNU_SOURCE
#include "support.h"
#include <semaphore.h>
#include <errno.h>
#include <pthread.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <time.h>
#include <unistd.h>
static int rounds, during, got, ready, stop, flush_calls;
static pid_t waiter;
static void *wait_thread(void *a) {
    (void)a;
    sigset_t s;
    sigemptyset(&s);
    sigaddset(&s, SIGRTMIN + 5);
    pthread_sigmask(SIG_BLOCK, &s, NULL);
    waiter = (pid_t)syscall(SYS_gettid);
    for (int r = 1; r <= rounds; r++) {
        if (!during) {
            union sigval v = {.sival_int = r};
            pthread_sigqueue(pthread_self(), SIGRTMIN + 5, v);
        } else {
            __atomic_store_n(&ready, r, __ATOMIC_RELEASE);
        }
        siginfo_t si;
        struct timespec t = {2, 0};
        int n = sigtimedwait(&s, &si, &t);
        if (n == SIGRTMIN + 5 && si.si_value.sival_int == r) got++;
        else fprintf(stderr, "round %d: sigtimedwait %d (%s)\n", r, n, n < 0 ? strerror(errno) : "wrong value");
    }
    return NULL;
}
static void *flusher(void *a) {
    (void)a;
    unsigned *code = mmap(NULL, 4096, PROT_READ | PROT_WRITE | PROT_EXEC, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    for (unsigned i = 0; !__atomic_load_n(&stop, __ATOMIC_ACQUIRE); i++) {
        unix_emit(code, i & 0xffff);
        if (((int (*)(void))code)() != (int)(i & 0xffff)) abort();
        __atomic_add_fetch(&flush_calls,1,__ATOMIC_RELEASE);
        sched_yield();
    }
    return NULL;
}
int main(int argc, char **argv) {
    int flush = argc > 1 && !strcmp(argv[1], "flush");
    during = flush || (argc > 1 && !strcmp(argv[1], "during"));
    pthread_t f;
    if (flush) { pthread_create(&f, NULL, flusher, NULL); while(!__atomic_load_n(&flush_calls,__ATOMIC_ACQUIRE))sched_yield(); }
    rounds = argc > 2 ? atoi(argv[2]) : 3;
    pthread_t t;
    pthread_create(&t, NULL, wait_thread, NULL);
    if (during) {
        for (int r = 1; r <= rounds; r++) {
            while (__atomic_load_n(&ready, __ATOMIC_ACQUIRE) < r) sched_yield();
            unix_wait_syscall(getpid(), waiter, SYS_rt_sigtimedwait);
            siginfo_t si;
            memset(&si, 0, sizeof si);
            si.si_signo = SIGRTMIN + 5;
            si.si_code = SI_QUEUE;
            si.si_pid = getpid();
            si.si_uid = getuid();
            si.si_value.sival_int = r;
            syscall(SYS_rt_tgsigqueueinfo, getpid(), waiter, SIGRTMIN + 5, &si);
        }
    }
    pthread_join(t, NULL);
    __atomic_store_n(&stop, 1, __ATOMIC_RELEASE);
    if (flush) pthread_join(f, NULL);
    printf("got %d/%d\n", got, rounds);
    puts(got == rounds ? "OK" : "BAD");
    return got != rounds;
}
