/* Source-derived from Seedbox PR317 f32472b7fe470367b7eddecde1ac53ad299f3a3e,
 * tools/jvm-fuzz/repros/failed_clone_vm.c. No DR internal API or layout is used.
 * Adaptations are described in inventory.tsv and the integration handoff. */
/* failed_clone_vm FIRST [THEN]: one clone/clone3 call that the kernel rejects, then THEN.
 * FIRST (each fails with EINVAL natively):
 *   none    no failing call
 *   vm      clone3 CLONE_VM | CLONE_SIGHAND | CLONE_CLEAR_SIGHAND, valid stack
 *   thread  clone3 CLONE_VM | CLONE_SIGHAND | CLONE_THREAD with exit_signal SIGCHLD
 *   vfork   clone3 CLONE_VM | CLONE_VFORK with exit_signal 200
 *   clone   legacy clone CLONE_VM | CLONE_THREAD without CLONE_SIGHAND
 *   fork    clone3 without CLONE_VM, exit_signal 200 (control: not a thread creation)
 *   eagain  pthread_create until it fails with EAGAIN (run under a pids limit, such as
 *           podman --pids-limit 32, with THEN flush: a fork cannot run under the limit)
 * THEN:
 *   fork    a fork-like clone3 whose child exits 7 (default)
 *   flush   rewrite, IC IVAU and call a code page 100 times while a second thread exists
 *   thread  pthread_create + join
 * Natively prints "first EINVAL", the THEN result and "OK"; under the translator the
 * THEN step hangs after a failed CLONE_VM call: the parent counted a thread that never
 * started (uninit_thread_count), and every later synch-all (fork, code flush, exit)
 * waits for it. */
#define _GNU_SOURCE
#include "support.h"
#include <semaphore.h>
#include <errno.h>
#include <pthread.h>
#include <sched.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>
struct c3 { uint64_t flags, pidfd, child_tid, parent_tid, exit_signal, stack, stack_size, tls, set_tid, set_tid_size, cgroup; };
static sem_t idle_ready;
static void *idle(void *a) { sem_post(&idle_ready); unix_park(); return a; }
static void *thr(void *a) { return a; }
int main(int argc, char **argv) {
    sem_init(&idle_ready,0,0);

    const char *first = argc > 1 ? argv[1] : "vm", *then = argc > 2 ? argv[2] : "fork";
    setvbuf(stdout, NULL, _IONBF, 0);
    char *stack = mmap(NULL, 1 << 16, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    struct c3 a = {.stack = (uintptr_t)stack, .stack_size = 1 << 16};
    long r = 0;
    if (!strcmp(first, "vm")) a.flags = CLONE_VM | CLONE_SIGHAND | 0x100000000ULL, a.exit_signal = SIGCHLD;
    else if (!strcmp(first, "thread")) a.flags = CLONE_VM | CLONE_SIGHAND | CLONE_THREAD, a.exit_signal = SIGCHLD;
    else if (!strcmp(first, "vfork")) a.flags = CLONE_VM | CLONE_VFORK, a.exit_signal = 200;
    else if (!strcmp(first, "fork")) a.flags = 0, a.exit_signal = 200, a.stack = a.stack_size = 0;
    if (!strcmp(first,"eagain")) { fputs("use unix-eagain under the owned pids launcher\n",stderr);return 2;
    } else if (!strcmp(first, "clone")) {
        r = syscall(SYS_clone, CLONE_VM | CLONE_THREAD, stack + (1 << 16), NULL, NULL, NULL);
        if (r == 0) _exit(99);
    } else if (strcmp(first, "none")) {
        r = syscall(SYS_clone3, &a, sizeof a);
        if (r == 0) _exit(99);
    }
    if (strcmp(first, "none") && strcmp(first, "eagain")) printf("first %s\n", r < 0 ? strerrorname_np(errno) : "success");
    int first_ok = !strcmp(first, "none") || (!strcmp(first, "eagain") ? r == 0 : r == -1 && errno == EINVAL);
    int ok = 0;
    if (!strcmp(then, "flush")) {
        pthread_t t;
        if(pthread_create(&t, NULL, idle, NULL))return 2;
        while(sem_wait(&idle_ready) && errno==EINTR){} /* live helper before synchronization */
        uint32_t *p = mmap(0, 4096, PROT_READ | PROT_WRITE | PROT_EXEC, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
        int good = 0;
        for (int i = 0; i < 100; i++) {
            unix_emit(p, i);
            good += ((int (*)(void))p)() == i;
        }
        printf("flushed %d\n", good);
        ok = good == 100;
    } else if (!strcmp(then, "thread")) {
        pthread_t t;
        void *res = NULL;
        ok = !pthread_create(&t, NULL, thr, (void *)1) && !pthread_join(t, &res) && res == (void *)1;
        printf("thread %s\n", ok ? "ok" : "bad");
    } else {
        struct c3 b = {.exit_signal = SIGCHLD};
        long c = syscall(SYS_clone3, &b, sizeof b);
        if (c == 0) _exit(7);
        int st = -1;
        if (c > 0) waitpid((pid_t)c, &st, 0);
        printf("child %d\n", WIFEXITED(st) ? WEXITSTATUS(st) : -1);
        ok = c > 0 && WIFEXITED(st) && WEXITSTATUS(st) == 7;
    }
    ok &= first_ok;
    puts(ok ? "OK" : "FAIL");
    _exit(!ok);
}
