/* Source: suite/tests/linux/memcache_unmap_race.c at DR 356c1821.
 * License: licenses/a64_prop_flush.c.txt (same 2026 Google BSD-3 notice).
 * The source's timed race becomes finite work with a positive start barrier.
 * Public memory queries are added by memcache_client.c; these do not prove
 * every private debug-cache invariant checked by the source runtime. */
#define _GNU_SOURCE
#include <errno.h>
#include <pthread.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/mman.h>
#include <unistd.h>
#define NREG 8
#define NQUERY 4
#define ROUNDS 4096
static char *base;
static size_t pg;
static pthread_barrier_t start;
static void check(int ok) { if (!ok) abort(); }
__attribute__((visibility("default"), noinline))
void translator_memory_hole(void *address) { __asm__ volatile("" : : "r"(address) : "memory"); }
static void *mapper(void *arg)
{
    char *p = base + (uintptr_t)arg * 2 * pg;
    pthread_barrier_wait(&start);
    for (int i = 0; i < ROUNDS; ++i) {
        check(munmap(p, pg) == 0);
        check(mmap(p, pg, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED, -1, 0) == p);
    }
    return NULL;
}
static void *querier(void *arg)
{
    unsigned s = (uintptr_t)arg + 1;
    pthread_barrier_wait(&start);
    for (int i = 0; i < ROUNDS; ++i) {
        s = s * 1103515245 + 12345;
        errno = 0;
        int result = mprotect(base + ((s >> 8) % NREG) * 2 * pg, pg, PROT_READ | PROT_WRITE);
        check(result == 0 || (result == -1 && errno == ENOMEM));
    }
    return NULL;
}
int main(void)
{
    pg = sysconf(_SC_PAGESIZE);
    char *p = mmap(NULL, 3 * pg, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    check(p != MAP_FAILED && munmap(p + pg, pg) == 0);
    for (int i = 0; i < 2; ++i) {
        errno = 0;
        check(mprotect(p + pg, pg, PROT_READ) == -1 && errno == ENOMEM);
        translator_memory_hole(p + pg);
    }
    check(munmap(p, 3 * pg) == 0);
    puts("DR:linux.memcache_unmap_race/unmapped-mprotect-twice: PASS");
    base = mmap(NULL, 2 * NREG * pg, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    check(base != MAP_FAILED);
    check(pthread_barrier_init(&start, NULL, NREG + NQUERY) == 0);
    pthread_t workers[NREG + NQUERY];
    for (uintptr_t i = 0; i < NREG + NQUERY; ++i)
        check(pthread_create(&workers[i], NULL, i < NREG ? mapper : querier, (void *)i) == 0);
    for (int i = 0; i < NREG + NQUERY; ++i) check(pthread_join(workers[i], NULL) == 0);
    for (int i = 0; i < NREG; ++i) {
        char *hole = base + i * 2 * pg;
        check(munmap(hole, pg) == 0);
        errno = 0;
        check(mprotect(hole, pg, PROT_READ) == -1 && errno == ENOMEM);
        translator_memory_hole(hole);
    }
    check(pthread_barrier_destroy(&start) == 0);
    check(munmap(base, 2 * NREG * pg) == 0);
    puts("DR:linux.memcache_unmap_race/concurrent-unmap-mprotect: PASS finite=4096");
    return 0;
}
