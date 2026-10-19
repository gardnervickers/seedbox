/* Source: Seedbox f32472b7fe470367b7eddecde1ac53ad299f3a3e tools/jvm-fuzz/repros/sigchld_flags.c.
 * Readiness is acknowledged, pacing is finite work, and child ownership remains explicit. */
/* sigchld_flags MODE: SIGCHLD dispositions the kernel acts on.
 *   ign        SIGCHLD set to SIG_IGN: exited children are reaped by the kernel, so
 *              wait() blocks until all 3 have exited and then fails with ECHILD
 *   nocldwait  a SIGCHLD handler with SA_NOCLDWAIT: the same
 *   nocldstop  a SIGCHLD handler with SA_NOCLDSTOP: a child that stops itself sends no
 *              SIGCHLD (the handler must not run before the child exits)
 * Natively prints "reaped 0 ECHILD" (ign, nocldwait) or "handled at stop 0" and "OK". */
#define _GNU_SOURCE
#include "support.h"
#include <semaphore.h>
#include <errno.h>
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>
static volatile sig_atomic_t chld;
static void h(int s) { (void)s; chld++; }
int main(int argc, char **argv) {
    const char *mode = argc > 1 ? argv[1] : "ign";
    struct sigaction a = {0};
    a.sa_handler = !strcmp(mode, "ign") ? SIG_IGN : h;
    a.sa_flags = !strcmp(mode, "nocldwait") ? SA_NOCLDWAIT : !strcmp(mode, "nocldstop") ? SA_NOCLDSTOP : 0;
    sigaction(SIGCHLD, &a, NULL);
    if (!strcmp(mode, "nocldstop")) {
        sigset_t blocked, old, pending; sigemptyset(&blocked); sigaddset(&blocked, SIGCHLD);
        sigprocmask(SIG_BLOCK, &blocked, &old);
        pid_t p = fork();
        if (p == 0) {
            syscall(SYS_kill, syscall(SYS_getpid), SIGSTOP);
            _exit(3);
        }
        int st;
        waitpid(p, &st, WUNTRACED);
        /* waitpid positively acknowledged the kernel stop; with NOCLDSTOP
         * no SIGCHLD may be pending at that completed transition. */
        sigpending(&pending);
        int at_stop = chld + sigismember(&pending, SIGCHLD);
        sigprocmask(SIG_SETMASK, &old, NULL);
        kill(p, SIGCONT);
        waitpid(p, &st, 0);
        printf("stopped %d handled at stop %d exit %d\n%s\n", 1, at_stop, WEXITSTATUS(st), at_stop == 0 ? "OK" : "FAIL");
        return at_stop != 0;
    }
    for (int i = 0; i < 3; i++)
        if (fork() == 0) _exit(0);
    int reaped = 0;
    for (;;) {
        if(wait(NULL)>0) { reaped++;continue; }
        if(errno==EINTR)continue;
        break;
    }
    int e = errno;
    printf("reaped %d %s\n%s\n", reaped, strerrorname_np(e), reaped == 0 && e == ECHILD ? "OK" : "FAIL");
    return reaped != 0;
}
