/* Source-derived from Seedbox PR317 f32472b7fe470367b7eddecde1ac53ad299f3a3e,
 * tools/jvm-fuzz/repros/pending_overflow.c. No DR internal API or layout is used.
 * Adaptations are described in inventory.tsv and the integration handoff. */
/*
 * Queues N realtime signals to the calling thread while it blocks them, then
 * unblocks: each must be delivered once, in order. The kernel accepts up to
 * RLIMIT_SIGPENDING; DynamoRIO keeps its own per-thread pending queue.
 *
 *   pending_overflow N [ROUNDS]
 *
 * Natively prints OK for any N up to the limit (e.g. 600).
 */
#define _GNU_SOURCE
#include "support.h"
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/syscall.h>
#include <unistd.h>

static int seen[100000];
static volatile int n_seen;
static void handler(int s, siginfo_t *si, void *uc) {
    (void)s; (void)uc;
    if (n_seen < 100000) seen[n_seen] = si->si_value.sival_int;
    n_seen++;
}

int main(int argc, char **argv) {
    int n = argc > 1 ? atoi(argv[1]) : 16, rounds = argc > 2 ? atoi(argv[2]) : 3;
    struct sigaction a;
    memset(&a, 0, sizeof a);
    a.sa_sigaction = handler;
    a.sa_flags = SA_SIGINFO;
    sigaction(SIGRTMIN, &a, 0);
    sigset_t s;
    sigemptyset(&s);
    sigaddset(&s, SIGRTMIN);
    int bad = 0;
    for (int r = 0; r < rounds; r++) {
        n_seen = 0;
        sigprocmask(SIG_BLOCK, &s, 0);
        int accepted = 0;
        for (int i = 0; i < n; i++) {
            siginfo_t si;
            memset(&si, 0, sizeof si);
            si.si_signo = SIGRTMIN;
            si.si_code = SI_QUEUE;
            si.si_pid = getpid();
            si.si_uid = getuid();
            si.si_value.sival_int = i;
            accepted += syscall(SYS_rt_tgsigqueueinfo, getpid(), gettid(), SIGRTMIN, &si) == 0;
        }
        sigprocmask(SIG_UNBLOCK, &s, 0);
        int order = n_seen == accepted;
        for (int i = 0; order && i < n_seen; i++) order = seen[i] == i;
        printf("round %d: queued %d accepted %d delivered %d inOrder %d", r, n, accepted, n_seen, order);
        if (!order)
            for (int i = 0; i < n_seen && i < 32; i++) printf("%s%d", i ? "," : " order ", seen[i]);
        printf("\n");
        bad += !order || accepted != n;
    }
    puts(bad ? "FAIL" : "OK");
    return bad != 0;
}
