/* Two observable clock samples surround a finite CPU-only region. No library
 * calls, syscalls or memory polling occur inside the assembly loop. */
#define _GNU_SOURCE
#include <inttypes.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/syscall.h>
#include <time.h>
#include <unistd.h>
static uint64_t observed_time(void) {
    struct timespec time;
    if (syscall(SYS_clock_gettime, CLOCK_MONOTONIC, &time)) _exit(81);
    return (uint64_t)time.tv_sec * UINT64_C(1000000000) + (uint64_t)time.tv_nsec;
}
int main(int argc, char **argv) {
    if (argc != 2) return 82;
    char *end;
    unsigned long count = strtoul(argv[1], &end, 10);
    if (*end || !count) return 83;
    uint64_t start = observed_time();
    printf("START=%" PRIu64 "\n", start);
    fflush(stdout);
#ifdef __aarch64__
    __asm__ volatile("1: subs %0, %0, #1\n b.ne 1b" : "+r"(count) : : "cc");
#else
#error This fixture measures ARM64 PV instrumentation only
#endif
    uint64_t finish = observed_time();
    printf("END=%" PRIu64 " DELTA=%" PRIu64 "\n", finish, finish - start);
    return 0;
}
