// Host-time cost of guest user-mode clock reads (issue #11).
// Usage: tscbench MODE N   MODE = loop | rdtsc | clock
// Each mode runs N iterations of the same loop; only the body differs.
// Run under SEEDBOX_TRACE_CONSOLE=1: the VMM stamps every console line with
// host time, so (rdtsc - loop) / N is the per-read cost of trap + emulation.
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>

static inline uint64_t rdtsc(void) {
    uint32_t lo, hi;
    __asm__ volatile("rdtsc" : "=a"(lo), "=d"(hi));
    return (uint64_t)hi << 32 | lo;
}

int main(int argc, char **argv) {
    if (argc != 3) return 2;
    const char *mode = argv[1];
    long n = atol(argv[2]);
    volatile uint64_t sink = 0;
    struct timespec ts;
    printf("BENCH %s %ld start\n", mode, n);
    fflush(stdout);
    for (long i = 0; i < n; i++) {
        if (!strcmp(mode, "rdtsc")) {
            sink += rdtsc();
        } else if (!strcmp(mode, "clock")) {
            clock_gettime(CLOCK_MONOTONIC, &ts);
            sink += (uint64_t)ts.tv_nsec;
        } else {
            sink += (uint64_t)i;
        }
    }
    printf("BENCH %s %ld done %llu\n", mode, n, (unsigned long long)(sink & 1));
    fflush(stdout);
    return 0;
}
