/* Source-derived from Seedbox PR317 f32472b7fe470367b7eddecde1ac53ad299f3a3e,
 * tools/jvm-fuzz/repros/exec_sigign.c. No DR internal API or layout is used.
 * Adaptations are described in inventory.tsv and the integration handoff. */
/* exec_sigign: SIG_IGN dispositions survive execve and handled ones become SIG_DFL.
 * Sets SIGPIPE, SIGINT and SIGHUP to SIG_IGN and SIGUSR1 to a handler, then execs
 * itself; the new image prints the dispositions. Natively prints "... = IIID
 * (expected IIID) OK"; under the translator all four are SIG_DFL ("DDDD"). */
#define _GNU_SOURCE
#include "support.h"
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <unistd.h>

static void h(int s) { (void)s; }
static char d(int s) {
    struct sigaction a;
    sigaction(s, NULL, &a);
    return a.sa_handler == SIG_DFL ? 'D' : a.sa_handler == SIG_IGN ? 'I' : 'H';
}
int main(int argc, char **argv) {
    if (argc > 1) {
        char got[5] = {d(SIGPIPE), d(SIGINT), d(SIGHUP), d(SIGUSR1), 0};
        int ok = !strcmp(got, "IIID");
        printf("PIPE INT HUP USR1 = %s (expected IIID) %s\n", got, ok ? "OK" : "BAD");
        return !ok;
    }
    signal(SIGPIPE, SIG_IGN);
    signal(SIGINT, SIG_IGN);
    signal(SIGHUP, SIG_IGN);
    signal(SIGUSR1, h);
    char self[4096];
    ssize_t n = readlink("/proc/self/exe", self, sizeof self - 1);
    if (n < 0) return 2;
    self[n] = 0;
    char *args[] = {argv[0], "child", NULL};
    execv(self, args);
    return 3;
}
