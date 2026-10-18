/* stxr_flush_dup MODE [CHUNKS]: two threads increment a shared and a private word
 * with ldxr/add/stxr retry loops (CHUNKS x 500 increments each, default 20000)
 * while the main thread repeats one action: none, signal (tgkill a worker),
 * flush (rewrite, IC IVAU and call a code slot), fork or mprotect.
 * Natively prints exact totals and "OK" for every MODE; under the translator
 * "flush" adds increments (e.g. shared 20000004/20000000), with or without the
 * client and with or without -ldstex2cas. */
#define _GNU_SOURCE
#include <pthread.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sched.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

static long inc(volatile uint64_t *w, long n) {
    long fails = 0;
    for (long i = 0; i < n; i++) {
        uint64_t v;
        unsigned f;
        for (;;) {
            asm volatile("ldxr %0, [%2]\nadd %0, %0, #1\nstxr %w1, %0, [%2]" : "=&r"(v), "=&r"(f) : "r"(w) : "memory");
            if (!f) break;
            fails++;
        }
    }
    return fails;
}
struct line { volatile uint64_t v; char pad[56]; };
static struct line shared __attribute__((aligned(64))), priv[2] __attribute__((aligned(64)));
static long chunks = 20000, fails[2];
static volatile int done;
static volatile pid_t tids[2];
static void usr1(int s) { (void)s; }
static void *worker(void *p) {
    long k = (long)p;
    tids[k] = (pid_t)syscall(SYS_gettid);
    for (long c = 0; c < chunks; c++) {
        fails[k] += inc(&shared.v, 500);
        fails[k] += inc(&priv[k].v, 500);
    }
    __atomic_add_fetch(&done, 1, __ATOMIC_RELEASE);
    return NULL;
}
int main(int argc, char **argv) {
    const char *mode = argc > 1 ? argv[1] : "flush";
    if (argc > 2) chunks = atol(argv[2]);
    signal(SIGUSR1, usr1);
    uint32_t *code = mmap(0, 4096, PROT_READ | PROT_WRITE | PROT_EXEC, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    char *scratch = mmap(0, 4096, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    pthread_t t[2];
    for (long k = 0; k < 2; k++) pthread_create(&t[k], NULL, worker, (void *)k);
    long events = 0;
    for (long i = 0; __atomic_load_n(&done, __ATOMIC_ACQUIRE) < 2; i++) {
        if (!strcmp(mode, "signal")) {
            pid_t tid = tids[i & 1];
            if (tid) syscall(SYS_tgkill, getpid(), tid, SIGUSR1);
        } else if (!strcmp(mode, "flush")) {
            uint32_t *slot = code + (i % 64) * 4;
            slot[0] = 0x52800000 | ((uint32_t)(i & 0xffff) << 5);
            slot[1] = 0xd65f03c0;
            __builtin___clear_cache((char *)slot, (char *)slot + 8);
            ((int (*)(void))slot)();
        } else if (!strcmp(mode, "fork")) {
            pid_t c = fork();
            if (!c) _exit(0);
            waitpid(c, NULL, 0);
        } else if (!strcmp(mode, "mprotect")) {
            mprotect(scratch, 4096, PROT_READ);
            mprotect(scratch, 4096, PROT_READ | PROT_WRITE);
        }
        events++;
        sched_yield();
    }
    for (int k = 0; k < 2; k++) pthread_join(t[k], NULL);
    uint64_t want = (uint64_t)chunks * 500;
    int ok = shared.v == 2 * want && priv[0].v == want && priv[1].v == want;
    printf("mode %s shared %lu/%lu private %lu %lu/%lu events %ld %s\n", mode, (unsigned long)shared.v,
           (unsigned long)(2 * want), (unsigned long)priv[0].v, (unsigned long)priv[1].v, (unsigned long)want, events > 0 ? 1L : 0L,
           ok ? "OK" : "BAD");
    return !ok;
}
