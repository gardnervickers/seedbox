/* sigaltstack_onstack: sigaltstack() queried inside a handler running on the
 * alternate stack reports SS_ONSTACK, and changing the stack fails with EPERM.
 * Natively prints "flags 1 change -1 errno Operation not permitted OK"; under the
 * translator the flags are 0 (EPERM is right). */
#define _GNU_SOURCE
#include <errno.h>
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <unistd.h>

static char alt[65536] __attribute__((aligned(16)));
static int flags = -1, change = 0, err = 0;
static void h(int s) {
    (void)s;
    stack_t cur, again = {.ss_sp = alt, .ss_flags = 0, .ss_size = sizeof alt};
    sigaltstack(NULL, &cur);
    flags = cur.ss_flags;
    change = sigaltstack(&again, NULL);
    err = errno;
}
int main(void) {
    stack_t st = {.ss_sp = alt, .ss_flags = 0, .ss_size = sizeof alt};
    sigaltstack(&st, NULL);
    struct sigaction a;
    memset(&a, 0, sizeof a);
    a.sa_handler = h;
    a.sa_flags = SA_ONSTACK;
    sigaction(SIGUSR1, &a, NULL);
    raise(SIGUSR1);
    int ok = flags == SS_ONSTACK && change == -1 && err == EPERM;
    printf("flags %d change %d errno %s %s\n", flags, change, change ? strerror(err) : "-", ok ? "OK" : "BAD");
    return !ok;
}
