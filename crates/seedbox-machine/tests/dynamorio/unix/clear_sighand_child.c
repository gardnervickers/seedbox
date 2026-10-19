/* Source-derived from Seedbox PR317 f32472b7fe470367b7eddecde1ac53ad299f3a3e,
 * tools/jvm-fuzz/repros/clear_sighand_child.c. No DR internal API or layout is used.
 * Adaptations are described in inventory.tsv and the integration handoff. */
/* clear_sighand_child MODE [fail]: a fork-like clone3 child made with CLONE_CLEAR_SIGHAND
 * then (MODE) exits ("exit"), starts a thread and rewrites and calls code 200 times
 * ("flush"), or installs a SIGSEGV handler and faults once ("segv"), and exits 7.
 * "fail" first issues a CLONE_CLEAR_SIGHAND clone3 the kernel rejects. With THREAD set
 * in the environment the parent has a second thread when it clones; with CGT set both
 * print their SigCgt/SigIgn lines to stderr.
 * Natively prints "child exit 7" and "OK". The release translator passes; the debug
 * build fails in the child (status 255, signal.c "sigsegv_handler_is_ours() ||
 * removed_sig_handler") when the parent had another thread. */
#define _GNU_SOURCE
#include "support.h"
#include <semaphore.h>
#include <pthread.h>
#include <setjmp.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>
struct c3 { uint64_t flags, pidfd, child_tid, parent_tid, exit_signal, stack, stack_size, tls, set_tid, set_tid_size, cgroup; };
static sigjmp_buf jb;
static void onsegv(int s) { (void)s; siglongjmp(jb, 1); }
static sem_t idle_ready;
static void *idle(void *a) { sem_post(&idle_ready); unix_park(); return a; }
static void sigcgt(const char *who) {
    char b[4096] = "";
    FILE *f = fopen("/proc/self/status", "r");
    if (!f) return;
    while (fgets(b, sizeof b, f))
        if (!strncmp(b, "SigCgt:", 7) || !strncmp(b, "SigIgn:", 7)) fprintf(stderr, "%s %s", who, b);
    fclose(f);
}
static int child(const char *mode) {
    if (getenv("CGT")) sigcgt("child");
    if (!strcmp(mode, "flush")) {
        pthread_t t;
        if(pthread_create(&t, NULL, idle, NULL))return 2;
        while(sem_wait(&idle_ready) && errno==EINTR){}
        uint32_t *p = mmap(0, 4096, PROT_READ | PROT_WRITE | PROT_EXEC, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
        for (int i = 0; i < 200; i++) {
            unix_emit(p, i);
            if (((int (*)(void))p)() != i) return 1;
        }
    } else if (!strcmp(mode, "segv")) {
        signal(SIGSEGV, onsegv);
        if (!sigsetjmp(jb, 1)) *(volatile int *)8 = 1;
    }
    return 7;
}
int main(int argc, char **argv) {
    sem_init(&idle_ready,0,0);

    const char *mode = argc > 1 ? argv[1] : "flush";
    signal(SIGUSR1, onsegv); /* a handled signal, so the clear has something to reset */
    { pthread_t t; if(pthread_create(&t, NULL, idle, NULL))return 2; while(sem_wait(&idle_ready) && errno==EINTR){} }
    if (argc > 2) { /* "fail": first a CLONE_CLEAR_SIGHAND clone3 the kernel rejects */
        struct c3 bad = {.flags = 0x100000000ULL, .exit_signal = 200};
        long r = syscall(SYS_clone3, &bad, sizeof bad);
        if (r == 0) _exit(99);
        printf("failed clone3 %ld\n", r);
    }
    struct c3 a = {.flags = 0x100000000ULL /* CLONE_CLEAR_SIGHAND */, .exit_signal = SIGCHLD};
    if (getenv("CGT")) sigcgt("parent");
    long c = syscall(SYS_clone3, &a, sizeof a);
    if (c == 0) _exit(child(mode));
    int st = -1;
    if (c <= 0) { perror("clone3"); return 2; }
    waitpid((pid_t)c, &st, 0);
    if (WIFEXITED(st)) printf("child exit %d\n", WEXITSTATUS(st));
    else printf("child signal %d\n", WIFSIGNALED(st) ? WTERMSIG(st) : -1);
    puts(WIFEXITED(st) && WEXITSTATUS(st) == 7 ? "OK" : "FAIL");
    return !(WIFEXITED(st) && WEXITSTATUS(st) == 7);
}
