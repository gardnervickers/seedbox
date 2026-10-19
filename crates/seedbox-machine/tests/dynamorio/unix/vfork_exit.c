/* Source-derived from Seedbox PR317 f32472b7fe470367b7eddecde1ac53ad299f3a3e,
 * tools/jvm-fuzz/repros/vfork_exit.c. No DR internal API or layout is used.
 * Adaptations are described in inventory.tsv and the integration handoff. */
/* vfork_exit MODE [N]: a vfork child that exits without exec, N times (default 1).
 *   vfork  glibc vfork(), the child calls _exit(3)
 *   clone  raw clone(CLONE_VM|CLONE_VFORK|SIGCHLD) on its own stack, the child calls exit(3)
 *   spawn  posix_spawn of a missing program: glibc's child fails execve and exits 127
 *   fail   dedicated SYS_vfork returns EAGAIN under an owned seccomp filter;
 *          follow with successful fork, code modification and thread creation
 * Natively prints "OK". */
#define _GNU_SOURCE
#include "support.h"
#include <errno.h>
#include <stddef.h>
#include <linux/filter.h>
#include <linux/seccomp.h>
#include <pthread.h>
#include <sched.h>
#include <spawn.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/prctl.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>
extern char **environ;
#if defined(__aarch64__)
static long raw_vfork_exit(void *stack_top) {
    register long x0 asm("x0") = CLONE_VM | CLONE_VFORK | SIGCHLD;
    register void *x1 asm("x1") = stack_top;
    register long x2 asm("x2") = 0, x3 asm("x3") = 0, x4 asm("x4") = 0;
    register long x8 asm("x8") = SYS_clone;
    asm volatile("svc #0\ncbnz x0, 1f\nmov x0, #3\nmov x8, #93\nsvc #0\n1:"
                 : "+r"(x0) : "r"(x1), "r"(x2), "r"(x3), "r"(x4), "r"(x8) : "memory", "cc");
    return x0;
}
#else
long raw_vfork_exit(void *);
asm(".text\n.type raw_vfork_exit,@function\nraw_vfork_exit:\nmov %rdi,%rsi\nmov $16657,%edi\nxor %edx,%edx\nxor %r10d,%r10d\nxor %r8d,%r8d\nmov $56,%eax\nsyscall\ntest %rax,%rax\njnz 1f\nmov $3,%edi\nmov $60,%eax\nsyscall\nud2\n1: ret\n");
#endif
#ifdef SYS_vfork
/* Owned-process error control: reject only the dedicated syscall, leaving
 * subsequent fork, code modification and pthread creation available. */
static void *failure_thread(void *arg) { return arg; }
static int failed_vfork(void) {
    struct sock_filter filter[] = {
        BPF_STMT(BPF_LD | BPF_W | BPF_ABS, offsetof(struct seccomp_data, nr)),
        BPF_JUMP(BPF_JMP | BPF_JEQ | BPF_K, SYS_vfork, 0, 1),
        BPF_STMT(BPF_RET | BPF_K, SECCOMP_RET_ERRNO | EAGAIN),
        BPF_STMT(BPF_RET | BPF_K, SECCOMP_RET_ALLOW),
    };
    struct sock_fprog prog = {sizeof(filter) / sizeof(filter[0]), filter};
    setvbuf(stdout, NULL, _IONBF, 0);
    if (prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) ||
        prctl(PR_SET_SECCOMP, SECCOMP_MODE_FILTER, &prog)) {
        perror("install vfork error filter");
        return 2;
    }
    errno = 0;
    pid_t pid = vfork();
    if (pid == 0) _exit(99);
    if (pid != -1 || errno != EAGAIN) return 1;
    puts("vfork EAGAIN");
    pid = fork();
    if (pid == 0) _exit(7);
    int status = 0;
    if (pid < 0 || waitpid(pid, &status, 0) != pid ||
        !WIFEXITED(status) || WEXITSTATUS(status) != 7) return 1;
    puts("child 7");
    uint32_t *code = mmap(NULL, 4096, PROT_READ | PROT_WRITE | PROT_EXEC,
                          MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (code == MAP_FAILED) return 2;
    for (int i = 0; i < 100; ++i) {
        unix_emit(code, i);
        if (((int (*)(void))code)() != i) return 1;
    }
    if (munmap(code, 4096)) return 2;
    puts("flush 100");
    pthread_t thread;
    void *result = NULL;
    if (pthread_create(&thread, NULL, failure_thread, &status) ||
        pthread_join(thread, &result) || result != &status) return 1;
    puts("thread ok");
    puts("OK");
    return 0;
}
#endif
int main(int argc, char **argv) {
    const char *mode = argc > 1 ? argv[1] : "vfork";
#ifdef SYS_vfork
    if (!strcmp(mode, "fail")) return failed_vfork();
#endif
    int n = argc > 2 ? atoi(argv[2]) : 1, bad = 0;
    char *stack = mmap(NULL, 1 << 16, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    for (int i = 0; i < n; i++) {
        pid_t p;
        int want = 3, st = 0;
        if (!strcmp(mode, "spawn")) {
            char *av[] = {"/nonexistent", NULL};
            /* glibc reports the failed execve as the error; the child has already exited 127. */
            bad += posix_spawn(&p, "/nonexistent", NULL, NULL, av, environ) == 0;
            continue;
        }
        if (!strcmp(mode, "clone")) {
            p = (pid_t)raw_vfork_exit(stack + (1 << 16));
        } else if ((p = vfork()) == 0) {
            _exit(3);
        }
        bad += waitpid(p, &st, 0) != p || !WIFEXITED(st) || WEXITSTATUS(st) != want;
    }
    puts(bad ? "BAD" : "OK");
    return bad != 0;
}
