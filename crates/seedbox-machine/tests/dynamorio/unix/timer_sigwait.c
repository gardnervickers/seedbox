/* Source: Seedbox f32472b7fe470367b7eddecde1ac53ad299f3a3e tools/jvm-fuzz/repros/timer_sigwait.c.
 * Readiness delays replaced with positive kernel/pipe/semaphore acknowledgements. */
/*
 * A process-directed realtime signal for a thread waiting in sigwaitinfo:
 *   timer_sigwait SOURCE WAITER
 * SOURCE: timer (a POSIX timer with SIGEV_SIGNAL, 100 ms) or kill (kill(getpid())).
 * WAITER: thread (every thread blocks SIGRTMIN; a second thread sigwaitinfo()s
 * while the main thread blocks in read), self (the main thread sigwaitinfo()s),
 * or handler (only the waiter thread unblocks SIGRTMIN and loops in pause()).
 * Prints OK when the waiter sees the signal within 5 s; natively all six pass.
 * Found by cfztime (harness/acceptance/workloads/clock-step.yaml): under the
 * translator, "timer thread" never returns from sigwaitinfo.
 */
#define _GNU_SOURCE
#include "support.h"
#include <semaphore.h>
#include <pthread.h>
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <sys/select.h>
#include <time.h>
#include <unistd.h>

static volatile sig_atomic_t handled;
static volatile pid_t waiter_tid;
static timer_t owned_timer;
static int has_timer;
static int done_pipe[2];
static const char *source, *mode;

static void on_signal(int sig) { (void)sig; handled = 1; }

static void fire(void)
{
    if (!strcmp(source, "kill")) {

        kill(getpid(), SIGRTMIN);
        return;
    }
    struct sigevent event = {.sigev_notify = SIGEV_SIGNAL, .sigev_signo = SIGRTMIN};

    struct itimerspec value = {.it_value = {.tv_nsec = 100000000}};
    if (timer_create(CLOCK_MONOTONIC, &event, &owned_timer) || timer_settime(owned_timer, 0, &value, 0))
        { perror("timer"); exit(2); }
    has_timer = 1;
}

static void *waiter(void *arg)
{
    (void)arg;
    waiter_tid = gettid();
    if (!strcmp(mode, "handler")) {
        sigset_t set;
        sigemptyset(&set);
        sigaddset(&set, SIGRTMIN);
        sigset_t wait; pthread_sigmask(SIG_SETMASK, NULL, &wait);
        sigdelset(&wait, SIGRTMIN);
        while (!handled) sigsuspend(&wait);
    } else {
        sigset_t set;
        siginfo_t info;
        sigemptyset(&set);
        sigaddset(&set, SIGRTMIN);
        while (sigwaitinfo(&set, &info) < 0) {}
    }
    if(write(done_pipe[1], "x", 1)!=1)abort();
    return 0;
}

int main(int argc, char **argv)
{
    if (argc != 3) {
        fprintf(stderr, "usage: timer_sigwait timer|kill thread|self|handler\n");
        return 2;
    }
    source = argv[1];
    mode = argv[2];
    signal(SIGRTMIN, on_signal);
    sigset_t set;
    sigemptyset(&set);
    sigaddset(&set, SIGRTMIN);
    pthread_sigmask(SIG_BLOCK, &set, 0);
    if(pipe(done_pipe))return 2;
    if (!strcmp(mode, "self")) {
        fire();

        siginfo_t info;
        while (sigwaitinfo(&set, &info) < 0) {}
        if (has_timer) timer_delete(owned_timer);
        printf("OK %s %s\n", source, mode);
        return 0;
    }
    pthread_t thread;
    pthread_create(&thread, 0, waiter, 0);
    while (!waiter_tid) sched_yield();
    unix_wait_syscall(getpid(), waiter_tid, !strcmp(mode, "handler") ? SYS_rt_sigsuspend : SYS_rt_sigtimedwait);
    fire();
    /* The main thread blocks in a syscall, as a server's accept or read would. */
    struct timespec limit = {.tv_sec = 5};
    fd_set fds;
    FD_ZERO(&fds);
    FD_SET(done_pipe[0], &fds);
    if (pselect(done_pipe[0] + 1, &fds, 0, 0, &limit, 0) != 1) {
        printf("FAIL %s %s: the waiter never saw the signal\n", source, mode);
        return 1;
    }
    pthread_join(thread, NULL);
    if (has_timer) timer_delete(owned_timer);
    printf("OK %s %s\n", source, mode);
    return 0;
}
