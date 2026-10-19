/* Source: Seedbox f32472b7fe470367b7eddecde1ac53ad299f3a3e tools/jvm-fuzz/repros/sigtimedwait_queued.c.
 * Readiness delays replaced with positive kernel/pipe/semaphore acknowledgements. */
/*
 * rt_sigtimedwait must return a blocked signal that is already pending or that
 * arrives while it waits. The wait has a 2 s timeout so a lost signal shows as EAGAIN.
 *
 *   sigtimedwait_queued pending|arrive|afterEintr [ROUNDS]
 *
 * pending:    tgkill self, then wait.
 * arrive:     another thread sends the signal while the main thread waits.
 * afterEintr: the wait is interrupted by a handled SIGUSR1 (EINTR); the signal
 *             is sent before the main thread waits again.
 *
 * Natively every mode prints OK.
 */
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
#include <sys/syscall.h>
#include <time.h>
#include <unistd.h>

static pid_t main_tid;
static volatile int usr1, phase;
static sem_t queued;
static void on_usr1(int s) { (void)s; usr1++; }
static long wait_sig(int sig, siginfo_t *info) {
    uint64_t set = 1ULL << (sig - 1);
    struct timespec t = {2, 0};
    return syscall(SYS_rt_sigtimedwait, &set, info, &t, 8);
}
static void *sender(void *p) {
    long mode = (long)p;
    int sig = SIGRTMIN + 5;
    unix_wait_syscall(getpid(), main_tid, SYS_rt_sigtimedwait);
    if (mode == 2) {
        syscall(SYS_tgkill, getpid(), main_tid, SIGUSR1);
        while (__atomic_load_n(&phase, __ATOMIC_ACQUIRE) == 0) sched_yield(); /* main saw EINTR and is about to wait again */
    }
    syscall(SYS_tgkill, getpid(), main_tid, sig);
    sem_post(&queued);
    return NULL;
}

int main(int argc, char **argv) {
    const char *m = argc > 1 ? argv[1] : "afterEintr";
    long mode = !strcmp(m, "pending") ? 0 : !strcmp(m, "arrive") ? 1 : 2;
    int rounds = argc > 2 ? atoi(argv[2]) : 3, bad = 0, sig = SIGRTMIN + 5;
    sem_init(&queued, 0, 0);
    main_tid = gettid();
    struct sigaction a;
    memset(&a, 0, sizeof a);
    a.sa_handler = on_usr1;
    sigaction(SIGUSR1, &a, 0);
    sigset_t s;
    sigemptyset(&s);
    sigaddset(&s, sig);
    sigprocmask(SIG_BLOCK, &s, 0);
    for (int r = 0; r < rounds; r++) {
        siginfo_t info;
        memset(&info, 0, sizeof info);
        phase = 0;
        long first = 0, ret;
        int err = 0;
        pthread_t t;
        if (mode == 0) {
            syscall(SYS_tgkill, getpid(), main_tid, sig);
            ret = wait_sig(sig, &info);
            err = ret < 0 ? errno : 0;
        } else {
            pthread_create(&t, 0, sender, (void *)mode);
            ret = wait_sig(sig, &info);
            err = ret < 0 ? errno : 0;
            if (mode == 2) {
                first = ret < 0 ? -err : ret;
                __atomic_store_n(&phase, 1, __ATOMIC_RELEASE);
                while (sem_wait(&queued) && errno == EINTR) {} /* positively queued outside the wait */
                ret = wait_sig(sig, &info);
                err = ret < 0 ? errno : 0;
            }
            pthread_join(t, 0);
            if (mode == 1) while (sem_wait(&queued) && errno == EINTR) {}
        }
        int ok = ret == sig && info.si_code == SI_TKILL && (mode != 2 || first == -EINTR);
        printf("round %d: %sret %ld (%s) si_code %d%s\n", r, mode == 2 ? (first == -EINTR ? "first EINTR, " : "first not EINTR, ") : "",
               ret, ret < 0 ? strerror(err) : "signal", info.si_code, ok ? "" : "  <-- wrong");
        bad += !ok;
    }
    puts(bad ? "FAIL" : "OK");
    return bad != 0;
}
