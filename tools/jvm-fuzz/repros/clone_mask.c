/* clone_mask KIND: raw clone children inherit the blocked signal mask.
 * KIND: thread (CLONE_THREAD|CLONE_SETTLS|..., like a pthread without glibc), vm (CLONE_VM|CLONE_SETTLS|SIGCHLD).
 * The parent blocks SIGUSR1; the child reads its mask with rt_sigprocmask and exits.
 * Natively prints "mask blocked" and "OK". */
#define _GNU_SOURCE
#include <linux/futex.h>
#include <sched.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>
static uint64_t child_mask = ~0ULL, tls[64] __attribute__((aligned(64)));
static int ctid;
static long child(void) {
    register long x0 asm("x0") = SIG_BLOCK, x1 asm("x1") = 0, x2 asm("x2") = (long)&child_mask, x3 asm("x3") = 8, x8 asm("x8") = SYS_rt_sigprocmask;
    asm volatile("svc #0" : "+r"(x0) : "r"(x1), "r"(x2), "r"(x3), "r"(x8) : "memory");
    return 7;
}
static long raw_clone(unsigned long flags, void *stack, int *ptid, unsigned long tls, int *ctid) {
    register unsigned long x0 asm("x0") = flags;
    register void *x1 asm("x1") = stack;
    register int *x2 asm("x2") = ptid;
    register unsigned long x3 asm("x3") = tls;
    register int *x4 asm("x4") = ctid;
    register long (*x5)(void) asm("x5") = child;
    register long x8 asm("x8") = SYS_clone;
    asm volatile("svc #0\ncbnz x0, 1f\nmov x29, xzr\nmov x30, xzr\nblr x5\nmov x8, #93\nsvc #0\nbrk #0\n1:"
                 : "+r"(x0) : "r"(x1), "r"(x2), "r"(x3), "r"(x4), "r"(x5), "r"(x8) : "memory", "cc");
    return (long)x0;
}
int main(int argc, char **argv) {
    const char *kind = argc > 1 ? argv[1] : "thread";
    sigset_t s;
    sigemptyset(&s);
    sigaddset(&s, SIGUSR1);
    sigprocmask(SIG_BLOCK, &s, NULL);
    char *stack = mmap(NULL, 1 << 20, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    char *top = stack + (1 << 20);
    long rc;
    int st = 0;
    if (!strcmp(kind, "thread")) {
        rc = raw_clone(CLONE_VM | CLONE_FS | CLONE_FILES | CLONE_SIGHAND | CLONE_THREAD | CLONE_SYSVSEM | CLONE_SETTLS |
                       CLONE_PARENT_SETTID | CLONE_CHILD_CLEARTID, top, &ctid, (unsigned long)&tls[32], &ctid);
        while (rc > 0 && __atomic_load_n(&ctid, __ATOMIC_ACQUIRE)) syscall(SYS_futex, &ctid, FUTEX_WAIT, ctid, NULL, NULL, 0);
    } else {
        rc = raw_clone(CLONE_VM | SIGCHLD | CLONE_SETTLS, top, NULL, (unsigned long)&tls[32], NULL);
        if (rc > 0) waitpid((pid_t)rc, &st, 0);
    }
    printf("clone %s, child exit %d\n", rc > 0 ? "ok" : "failed", WEXITSTATUS(st));
    printf("mask %s\n", child_mask == ~0ULL ? "unread" : (child_mask >> (SIGUSR1 - 1)) & 1 ? "blocked" : "NOT BLOCKED");
    puts("OK");
    return 0;
}
