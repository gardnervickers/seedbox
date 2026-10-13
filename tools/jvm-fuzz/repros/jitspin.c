/* jitspin WRITERS EXECUTORS MILLIS [PERIOD_US]: WRITERS threads each rewrite one-instruction
 * "movz w0, #v; ret" slots of their own page, flush and sleep PERIOD_US (default 150);
 * EXECUTORS threads call random slots of every page in a tight loop without syscalls.
 * Main stops everything after MILLIS ms. Natively prints "OK" (and the flush count on stderr).
 * A watchdog prints "STUCK" when the writers make no progress for 20 s. */
#define _GNU_SOURCE
#include <pthread.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/mman.h>
#include <time.h>
#include <unistd.h>
#define MAXW 8
static int nw, stop, bad;
static long flushes;
static uint32_t *page[MAXW];
static void *writer(void *a) {
    long id = (long)a;
    long period = 150;
    uint32_t *code = page[id];
    for (uint32_t i = 1; !__atomic_load_n(&stop, __ATOMIC_ACQUIRE); i++) {
        int slot = i % 64;
        __atomic_store_n(&code[slot * 2], 0x52800000u | (uint32_t)(id << 15 | (i & 0x1ff) << 6 | slot) << 5, __ATOMIC_RELAXED);
        __builtin___clear_cache((char *)&code[slot * 2], (char *)&code[slot * 2 + 1]);
        __atomic_add_fetch(&flushes, 1, __ATOMIC_RELAXED);
        usleep(period);
    }
    return NULL;
}
static void *executor(void *a) {
    uint64_t x = (uintptr_t)a * 0x9E3779B97F4A7C15ULL | 1;
    while (!__atomic_load_n(&stop, __ATOMIC_ACQUIRE)) {
        x ^= x << 13; x ^= x >> 7; x ^= x << 17;
        int w = (int)(x % nw), slot = (int)((x >> 8) % 64);
        uint32_t v = ((uint32_t(*)(void))&page[w][slot * 2])();
        if ((v & 63) != (uint32_t)slot || v >> 15 != (uint32_t)w) __atomic_add_fetch(&bad, 1, __ATOMIC_RELAXED);
    }
    return NULL;
}
static void *watchdog(void *a) {
    (void)a;
    long last = -1;
    for (;;) {
        sleep(20);
        long f = __atomic_load_n(&flushes, __ATOMIC_RELAXED);
        if (f == last) { printf("STUCK after %ld flushes\n", f); fflush(stdout); _exit(3); }
        last = f;
    }
    return NULL;
}
int main(int argc, char **argv) {
    nw = atoi(argv[1]);
    int ne = atoi(argv[2]), ms = atoi(argv[3]);
    if (nw < 1 || nw > MAXW || ne > 16) return 2;
    pthread_t w[MAXW], e[16], wd;
    for (int k = 0; k < nw; k++) {
        page[k] = mmap(NULL, 4096, PROT_READ | PROT_WRITE | PROT_EXEC, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
        for (int s = 0; s < 64; s++) { page[k][s * 2] = 0x52800000u | (uint32_t)(k << 15 | s) << 5; page[k][s * 2 + 1] = 0xd65f03c0u; }
        __builtin___clear_cache((char *)page[k], (char *)page[k] + 4096);
    }
    pthread_create(&wd, NULL, watchdog, NULL);
    for (long k = 0; k < nw; k++) pthread_create(&w[k], NULL, writer, (void *)k);
    for (long k = 0; k < ne; k++) pthread_create(&e[k], NULL, executor, (void *)(k + 1));
    struct timespec t = {ms / 1000, (ms % 1000) * 1000000L};
    nanosleep(&t, NULL);
    __atomic_store_n(&stop, 1, __ATOMIC_RELEASE);
    for (int k = 0; k < nw; k++) pthread_join(w[k], NULL);
    for (int k = 0; k < ne; k++) pthread_join(e[k], NULL);
    fprintf(stderr, "flushes %ld\n", flushes);
    puts(bad ? "BAD" : "OK");
    return 0;
}
