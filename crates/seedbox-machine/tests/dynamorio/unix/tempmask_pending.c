/* Source-derived from Seedbox PR317 f32472b7fe470367b7eddecde1ac53ad299f3a3e,
 * tools/jvm-fuzz/repros/tempmask_pending.c. No DR internal API or layout is used.
 * Adaptations are described in inventory.tsv and the integration handoff. */
/*
 * SIGUSR1 and SIGUSR2 are blocked and pending. A call with a temporary mask that
 * unblocks only SIGUSR1 must run its handler once, return EINTR, restore the mask
 * and leave SIGUSR2 pending (sigpending reports it; unblocking delivers it once).
 *
 *   tempmask_pending BASE[-ready|-badarg] [ROUNDS]
 *   BASE: sigsuspend|ppoll|pselect6|epoll_pwait|epoll_pwait2
 *
 * -ready: an fd is readable, so the polling call returns 1 without delivering anything
 * and both signals stay pending. -badarg: an invalid timeout (ppoll, pselect6), sigset
 * size (sigsuspend) or epoll fd fails the call before or without delivering anything.
 * The first line checks sigpending without any temporary mask.
 *
 * Natively every mode prints OK.
 */
#define _GNU_SOURCE
#include "support.h"
#include <errno.h>
#include <poll.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/epoll.h>
#include <sys/eventfd.h>
#include <sys/select.h>
#include <sys/syscall.h>
#include <unistd.h>

static volatile int h1, h2;
static void handler(int s) {
    if (s == SIGUSR1) h1++;
    else h2++;
}
#define BIT(s) (1ULL << ((s) - 1))

int main(int argc, char **argv) {
    const char *mode = argc > 1 ? argv[1] : "sigsuspend";
    int rounds = argc > 2 ? atoi(argv[2]) : 10;
    signal(SIGUSR1, handler);
    signal(SIGUSR2, handler);
    int p[2];
    if (pipe(p)) return 2;
    int ep = epoll_create1(0);
    struct epoll_event e = {.events = EPOLLIN};
    epoll_ctl(ep, EPOLL_CTL_ADD, p[0], &e);
    uint64_t both = BIT(SIGUSR1) | BIT(SIGUSR2), orig = 0, temp, mask = 0, pend = 0;
    int bad = 0;
    /* Without any temporary mask: both blocked and pending. */
    syscall(SYS_rt_sigprocmask, SIG_BLOCK, &both, NULL, 8);
    syscall(SYS_tgkill, getpid(), gettid(), SIGUSR1);
    syscall(SYS_tgkill, getpid(), gettid(), SIGUSR2);
    syscall(SYS_rt_sigpending, &pend, 8);
    syscall(SYS_rt_sigprocmask, SIG_UNBLOCK, &both, NULL, 8);
    printf("plain: pending %#llx | after unblock h1 %d h2 %d%s\n", (unsigned long long)pend, h1, h2,
           pend == both && h1 == 1 && h2 == 1 ? "" : "  <-- wrong");
    bad += !(pend == both && h1 == 1 && h2 == 1);
    /* MODE is BASE, BASE-ready (an fd is readable: the call returns 1 and delivers
     * nothing) or BASE-badarg (rejected arguments: an error, nothing delivered). */
    char base[32];
    snprintf(base, sizeof base, "%s", mode);
    int ready = 0, badarg = 0;
    char *dash = strchr(base, '-');
    if (dash) { ready = !strcmp(dash, "-ready"); badarg = !strcmp(dash, "-badarg"); *dash = 0; }
    int efd = eventfd(1, 0), epr = epoll_create1(0);
    struct epoll_event er = {.events = EPOLLIN};
    epoll_ctl(epr, EPOLL_CTL_ADD, efd, &er);
    for (int i = 0; i < rounds; i++) {
        h1 = h2 = 0;
        syscall(SYS_rt_sigprocmask, SIG_BLOCK, &both, NULL, 8);
        syscall(SYS_rt_sigprocmask, SIG_BLOCK, NULL, &orig, 8);
        temp = orig & ~BIT(SIGUSR1);
        syscall(SYS_tgkill, getpid(), gettid(), SIGUSR1);
        syscall(SYS_tgkill, getpid(), gettid(), SIGUSR2);
        int fd = ready ? efd : p[0], epfd = badarg ? -1 : ready ? epr : ep;
        struct pollfd pf = {fd, POLLIN, 0};
        fd_set rf;
        FD_ZERO(&rf);
        FD_SET(fd, &rf);
        struct { const uint64_t *ss; size_t len; } psm = {&temp, 8};
        /* 2 s bounds a signal that is never delivered; natively these return at once. */
        struct timespec limit = {2, 0}, bad_ts = {0, -1}, *ts = badarg ? &bad_ts : &limit;
        long r;
        if (!strcmp(base, "ppoll")) r = syscall(SYS_ppoll, &pf, 1, ts, &temp, 8);
        else if (!strcmp(base, "pselect6")) r = syscall(SYS_pselect6, fd + 1, &rf, NULL, NULL, ts, &psm);
        else if (!strcmp(base, "epoll_pwait")) r = syscall(SYS_epoll_pwait, epfd, &e, 1, 2000, &temp, 8);
        else if (!strcmp(base, "epoll_pwait2")) r = syscall(441, epfd, &e, 1, &limit, &temp, 8);
        else r = syscall(SYS_rt_sigsuspend, &temp, badarg ? 4 : 8);
        int err = errno, a1 = h1, a2 = h2;
        syscall(SYS_rt_sigprocmask, SIG_BLOCK, NULL, &mask, 8);
        syscall(SYS_rt_sigpending, &pend, 8);
        syscall(SYS_rt_sigprocmask, SIG_UNBLOCK, &both, NULL, 8);
        int want_err = !badarg ? EINTR : !strncmp(base, "epoll", 5) ? EBADF : EINVAL, interrupted = !ready && !badarg;
        int ok = (ready ? r == 1 : r == -1 && err == want_err) && a1 == interrupted && a2 == 0 && mask == orig &&
                 pend == (interrupted ? BIT(SIGUSR2) : both) && h1 == 1 && h2 == 1;
        if (!ok || i == 0)
            printf("round %d: ret %ld errno %d h1 %d h2 %d maskRestored %d pending %#llx | after unblock h1 %d h2 %d%s\n", i, r,
                   r < 0 ? err : 0, a1, a2, mask == orig, (unsigned long long)pend, h1, h2, ok ? "" : "  <-- wrong");
        bad += !ok;
    }
    puts(bad ? "FAIL" : "OK");
    return bad != 0;
}
