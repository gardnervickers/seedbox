/* Source: Seedbox f32472b7fe470367b7eddecde1ac53ad299f3a3e tools/jvm-fuzz/repros/vfork_flush.c.
 * Readiness is acknowledged, pacing is finite work, and child ownership remains explicit. */
/* vfork_flush ROUNDS SPINNERS [PERIOD_US [spin]]: a thread vforks a child that waits for a
 * byte on a pipe before it execs "vfork_flush exit 3", so the parent stays blocked in vfork;
 * main sends the byte 2 ms later ("spin": the child computes for a few ms instead, then
 * execs). Meanwhile another thread rewrites and flushes code every PERIOD_US (default 1000;
 * 0: no such thread) and SPINNERS threads loop through indirect calls.
 * Natively prints "OK". A watchdog prints "STUCK round R" after 20 s without progress. */
#define _GNU_SOURCE
#include "support.h"
#include <semaphore.h>
#include <pthread.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>
extern char **environ;
static int rounds, period = 1000, stop, round_no, fds[2], bad, spin;
static const char *self;
static volatile pid_t child_tid;
static volatile int acknowledged;
static __attribute__((noinline)) long f0(long x) { return x + 1; }
static __attribute__((noinline)) long f1(long x) { return x ^ 5; }
static long (*volatile fns[2])(long) = {f0, f1};
static void *spinner(void *a) {
    volatile long x = (long)a;
    while (!__atomic_load_n(&stop, __ATOMIC_ACQUIRE)) x = fns[x & 1](x);
    return NULL;
}
static void *flusher(void *a) {
    (void)a;
    uint32_t *code = mmap(NULL, 4096, PROT_READ | PROT_WRITE | PROT_EXEC, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    for (uint32_t i = 0; !__atomic_load_n(&stop, __ATOMIC_ACQUIRE); i++) {
        unix_emit(code, i & 0xffff);
        ((int (*)(void))code)();
        sched_yield();
    }
    return NULL;
}
static void *forker(void *a) {
    (void)a;
    char *av[] = {(char *)self, "exit", "3", NULL};
    for (int r = 1; r <= rounds; r++) {
        __atomic_store_n(&round_no, r, __ATOMIC_RELEASE);
        pid_t p = vfork();
        if (p == 0) {
            child_tid = gettid();
            char b;
            if (spin) {
                for (volatile long k = 0; k < 3000000; k++) {}
            } else {
                while (syscall(SYS_read, fds[0], &b, 1) < 0) {}
            }
            execve(self, av, environ);
            _exit(127);
        }
        int st = 0;
        if (waitpid(p, &st, 0) != p || !WIFEXITED(st) || WEXITSTATUS(st) != 3) bad++;
        __atomic_store_n(&round_no, -r, __ATOMIC_RELEASE);
        if (!spin) while (__atomic_load_n(&acknowledged, __ATOMIC_ACQUIRE) < r) sched_yield();
    }
    return NULL;
}
int main(int argc, char **argv) {
    if (argc > 2 && !strcmp(argv[1], "exit")) return atoi(argv[2]);
    self = argv[0];
    rounds = atoi(argv[1]);
    int spinners = atoi(argv[2]);
    if (argc > 3) period = atoi(argv[3]);
    spin = argc > 4 && !strcmp(argv[4], "spin");
    if (pipe(fds)) return 2;
    pthread_t w, fl, fk, sp[16];

    if (period) pthread_create(&fl, NULL, flusher, NULL);
    for (long k = 0; k < spinners && k < 16; k++) pthread_create(&sp[k], NULL, spinner, (void *)k);
    pthread_create(&fk, NULL, forker, NULL);
    for (int r = 1; r <= rounds && !spin; r++) {
        while (__atomic_load_n(&round_no, __ATOMIC_ACQUIRE) != r) sched_yield();
        while (!child_tid) sched_yield();
        unix_wait_syscall(child_tid, child_tid, SYS_read);
        if (write(fds[1], "x", 1) != 1) return 2;
        while (__atomic_load_n(&round_no, __ATOMIC_ACQUIRE) == r) sched_yield();
        child_tid = 0;
        __atomic_store_n(&acknowledged, r, __ATOMIC_RELEASE);
    }
    pthread_join(fk, NULL);
    __atomic_store_n(&stop, 1, __ATOMIC_RELEASE);
    if (period) pthread_join(fl, NULL);
    for (long k = 0; k < spinners && k < 16; k++) pthread_join(sp[k], NULL);
    puts(bad ? "BAD" : "OK");
    return bad != 0;
}
