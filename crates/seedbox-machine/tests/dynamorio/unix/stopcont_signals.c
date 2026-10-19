/* Source Seedbox f32472b7 stopcont_signals.c. Pacing/readiness uses completed work,
 * observed kernel waits and waitid receipts. Actual bounded sleep syscalls stay
 * as the behavior under test; no elapsed-time or timed-absence assertions. */
/*
 * A child whose main thread computes (SIGUSR1 unblocked, counting handler) and
 * whose sleeper threads block SIGUSR1 and loop in 2 ms timed waits (nanosleep,
 * futex, ppoll, clock_nanosleep ABSTIME). The parent
 * either stops and continues the child (SIGSTOP/SIGCONT), or sends it paced
 * process-directed SIGUSR1 (waiting for each handler run), or both.
 *
 *   stopcont_signals stop|usr1|both [CYCLES [SLEEPERS [NCOMPUTE]]]
 *
 * With NCOMPUTE > 0, that many computing threads take SIGUSR1 and the main thread
 * blocks it.
 *
 * Natively: no sleeper ever sees EINTR (stops restart via restart_syscall; SIGUSR1
 * is blocked there) and the handler runs exactly once per SIGUSR1. Prints OK.
 */
#define _GNU_SOURCE
#include "support.h"
#include <errno.h>
#include <linux/futex.h>
#include <poll.h>
#include <stdint.h>
#include <sys/syscall.h>
#include <pthread.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

struct shared { volatile int stop, ready, usr1; long calls, eintr; pid_t tids[16]; };
static struct shared *sh;
static void on_usr1(int s) { (void)s; __atomic_add_fetch(&sh->usr1, 1, __ATOMIC_SEQ_CST); }
/* Sleeper k waits 2 ms with nanosleep, a timed futex, ppoll or clock_nanosleep(ABSTIME). */
static void *sleeper(void *p) {
    long k = (long)p % 4;
    sh->tids[(long)p]=gettid();
    uint32_t word = 0;
    int pp[2];
    if (pipe(pp)) return NULL;
    __atomic_add_fetch(&sh->ready, 1, __ATOMIC_SEQ_CST);
    while (!sh->stop) {
        struct timespec t = {0, 2000000}, now;
        long r;
        if (k == 0) {
            /* seedbox-syscall-subject: nanosleep; bound-ms=2; invariant=masked sleeper must not leak EINTR across acknowledged process stop/continue and routed signals. */
            r = syscall(SYS_nanosleep,&t,NULL);
        }
        else if (k == 1) r = syscall(SYS_futex, &word, FUTEX_WAIT_PRIVATE, 0, &t, NULL, 0);
        else if (k == 2) {
            struct pollfd pf = {pp[0], POLLIN, 0};
            r = syscall(SYS_ppoll, &pf, 1, &t, NULL, 8);
        } else {
            clock_gettime(CLOCK_MONOTONIC, &now);
            now.tv_nsec += 2000000;
            if (now.tv_nsec >= 1000000000) { now.tv_sec++; now.tv_nsec -= 1000000000; }
            /* seedbox-syscall-subject: clock_nanosleep; bound-ms=2; invariant=absolute masked wait must not leak EINTR across acknowledged process stop/continue. */
            r = syscall(SYS_clock_nanosleep,CLOCK_MONOTONIC,TIMER_ABSTIME,&now,NULL);
        }
        __atomic_add_fetch(&sh->calls, 1, __ATOMIC_SEQ_CST);
        if (r < 0 && errno == EINTR) __atomic_add_fetch(&sh->eintr, 1, __ATOMIC_SEQ_CST);
    }
    return NULL;
}

static void *computer(void *u) {
    if (u) pthread_sigmask(SIG_UNBLOCK, u, 0);
    volatile unsigned long x = 0;
    while (!sh->stop) x++;
    return NULL;
}

int main(int argc, char **argv) {
    const char *mode = argc > 1 ? argv[1] : "both";
    int cycles = argc > 2 ? atoi(argv[2]) : 100, nsleep = argc > 3 ? atoi(argv[3]) : 2, ncompute = argc > 4 ? atoi(argv[4]) : 0;
    int do_stop = strcmp(mode, "usr1") != 0, do_usr1 = strcmp(mode, "stop") != 0;
    sh = mmap(0, 4096, PROT_READ | PROT_WRITE, MAP_SHARED | MAP_ANONYMOUS, -1, 0);
    pid_t c = fork();
    if (!c) {
        struct sigaction a;
        memset(&a, 0, sizeof a);
        a.sa_handler = on_usr1;
        a.sa_flags = SA_RESTART;
        sigaction(SIGUSR1, &a, 0);
        sigset_t u;
        sigemptyset(&u);
        sigaddset(&u, SIGUSR1);
        pthread_sigmask(SIG_BLOCK, &u, 0); /* inherited by the sleepers */
        pthread_t t[16];
        for (int i = 0; i < nsleep && i < 16; i++) pthread_create(&t[i], 0, sleeper, (void *)(long)i);
        if (ncompute == 0) { /* the main thread receives SIGUSR1 */
            pthread_sigmask(SIG_UNBLOCK, &u, 0);
            computer(NULL);
        } else { /* NCOMPUTE threads receive it; the main thread blocks it and joins */
            pthread_t w[16];
            for (int i = 0; i < ncompute && i < 16; i++) pthread_create(&w[i], 0, computer, &u);
            for (int i = 0; i < ncompute && i < 16; i++) pthread_join(w[i], 0);
        }
        for (int i = 0; i < nsleep && i < 16; i++) pthread_join(t[i], 0);
        _exit(0);
    }
    while (sh->ready < nsleep) sched_yield();
    for(int k=0;k<nsleep;k++) unix_wait_syscall(c,sh->tids[k],k%4==0?SYS_nanosleep:k%4==1?SYS_futex:k%4==2?SYS_ppoll:SYS_clock_nanosleep);
    int stopped = 0, sent = 0, lost = 0;
    for (int i = 0; i < cycles; i++) {
        long previous=__atomic_load_n(&sh->calls,__ATOMIC_ACQUIRE);
        while(__atomic_load_n(&sh->calls,__ATOMIC_ACQUIRE)<=previous)sched_yield();
        if (do_stop) {
            siginfo_t si;
            kill(c, SIGSTOP);
            if(waitid(P_PID,c,&si,WSTOPPED) || si.si_code!=CLD_STOPPED || si.si_status!=SIGSTOP)return 1;

            kill(c, SIGCONT);
            if(waitid(P_PID,c,&si,WCONTINUED) || si.si_code!=CLD_CONTINUED || si.si_status!=SIGCONT)return 1;
            stopped++;
        }
        if (do_usr1 && i % 4 == 0) {
            int before = sh->usr1, k;
            kill(c, SIGUSR1);
            sent++;
            while (__atomic_load_n(&sh->usr1,__ATOMIC_ACQUIRE)==before)sched_yield();
            lost += sh->usr1 == before;
        }
    }
    long previous=__atomic_load_n(&sh->calls,__ATOMIC_ACQUIRE);
    while(__atomic_load_n(&sh->calls,__ATOMIC_ACQUIRE)<=previous)sched_yield();
    sh->stop = 1;
    int st;
    waitpid(c, &st, 0);
    printf("mode %s: stop/continue cycles %d, SIGUSR1 sent %d handled %d (lost %d), sleeper calls %ld EINTR %ld, exit %d\n", mode,
           stopped, sent, sh->usr1, lost, sh->calls, sh->eintr, WIFEXITED(st) ? WEXITSTATUS(st) : -1);
    int ok = sh->eintr == 0 && sh->usr1 == sent && WIFEXITED(st) && WEXITSTATUS(st) == 0;
    puts(ok ? "OK" : "FAIL");
    return !ok;
}
