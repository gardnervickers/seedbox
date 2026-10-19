/* Source-derived from Seedbox PR317 f32472b7fe470367b7eddecde1ac53ad299f3a3e,
 * tools/jvm-fuzz/repros/pending_fork_signalfd.c. No DR internal API or layout is used.
 * Adaptations are described in inventory.tsv and the integration handoff. */
/*
 * Deep realtime pending queues across fork and through signalfd.
 *
 *   pending_fork_signalfd fork N      N signals tagged P queued (blocked) before fork;
 *                                     the parent then queues N tagged C to the child.
 *                                     Each side must receive only its own N, in order.
 *   pending_fork_signalfd signalfd N  N queued (blocked), drained through a
 *                                     nonblocking signalfd, then EAGAIN.
 *
 * Natively prints OK (N up to RLIMIT_SIGPENDING). Under the translator N > 8
 * first needs -max_pending_signals raised (see pending_overflow.c).
 */
#define _GNU_SOURCE
#include "support.h"
#include <errno.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/signalfd.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

static int log_v[4096];
static volatile int log_n;
static void handler(int s, siginfo_t *si, void *uc) {
    (void)s; (void)uc;
    int n = log_n;
    if (n < 4096) log_v[n] = si->si_value.sival_int;
    log_n = n + 1;
}
/* got/inOrder/foreign for tag over the log, after waiting for want entries */
static void tally(int tag, int want, int out[3]) {
    while (__atomic_load_n(&log_n, __ATOMIC_ACQUIRE) < want) sched_yield();
    out[0] = 0; out[1] = 1; out[2] = 0;
    for (int i = 0; i < log_n && i < 4096; i++) {
        if (log_v[i] >> 16 != tag) { out[2]++; continue; }
        out[1] &= (log_v[i] & 0xffff) == out[0];
        out[0]++;
    }
}

int main(int argc, char **argv) {
    const char *mode = argc > 1 ? argv[1] : "fork";
    int n = argc > 2 ? atoi(argv[2]) : 257, sig = SIGRTMIN + 2;
    sigset_t s;
    sigemptyset(&s);
    sigaddset(&s, sig);
    sigprocmask(SIG_BLOCK, &s, 0);
    if (!strcmp(mode, "signalfd")) {
        int acc = 0;
        for (int i = 0; i < n; i++) acc += sigqueue(getpid(), sig, (union sigval){.sival_int = i}) == 0;
        int fd = signalfd(-1, &s, SFD_NONBLOCK), got = 0, order = 1, eagain = 0;
        struct signalfd_siginfo b[16];
        const int sizes[4] = {1, 7, 3, 16};
        for (int k = 0; k < 100000; k++) {
            ssize_t r = read(fd, b, sizes[k % 4] * sizeof b[0]);
            if (r < 0) { eagain = errno == EAGAIN; break; }
            for (ssize_t j = 0; j < r / (ssize_t)sizeof b[0]; j++, got++) order &= (int)b[j].ssi_int == got;
        }
        printf("signalfd: queued %d accepted %d read %d inOrder %d thenEagain %d\n", n, acc, got, order, eagain);
        int ok = got == n && acc == n && order && eagain;
        puts(ok ? "OK" : "FAIL");
        return !ok;
    }
    struct sigaction a;
    memset(&a, 0, sizeof a);
    a.sa_sigaction = handler;
    a.sa_flags = SA_SIGINFO | SA_RESTART;
    sigaction(sig, &a, 0);
    int pa = 0, ca = 0, go[2], res[2];
    for (int i = 0; i < n; i++) pa += sigqueue(getpid(), sig, (union sigval){.sival_int = 0x10000 | i}) == 0;
    if (pipe(go) || pipe(res)) return 2;
    pid_t c = fork();
    if (c < 0) { perror("fork"); return 2; }
    if (!c) {
        int want, out[3];
        if (read(go[0], &want, sizeof want) != sizeof want) _exit(1);
        log_n = 0;
        sigprocmask(SIG_UNBLOCK, &s, 0);
        tally(2, want, out);
        _exit(write(res[1], out, sizeof out) == sizeof out ? 0 : 1);
    }
    for (int i = 0; i < n; i++) ca += sigqueue(c, sig, (union sigval){.sival_int = 0x20000 | i}) == 0;
    if (write(go[1], &ca, sizeof ca) != sizeof ca) return 2;
    log_n = 0;
    sigprocmask(SIG_UNBLOCK, &s, 0);
    int p[3], ch[3] = {-1, -1, -1};
    tally(1, pa, p);
    if (read(res[0], ch, sizeof ch) != sizeof ch) {}
    waitpid(c, 0, 0);
    printf("fork %d: parent %d/%d inOrder %d othersTags %d | child %d/%d inOrder %d othersTags %d\n", n, p[0], pa, p[1], p[2], ch[0],
           ca, ch[1], ch[2]);
    int ok = p[0] == n && p[1] && !p[2] && ch[0] == n && ch[1] && !ch[2];
    puts(ok ? "OK" : "FAIL");
    return !ok;
}
