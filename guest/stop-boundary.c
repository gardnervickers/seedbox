/* Focused clock/syscall probe; no privileged port or host-machine access. */
#include <stdint.h>
#include <stdio.h>
#include <sys/syscall.h>
#include <time.h>
#include <unistd.h>

static uint64_t now(void)
{
    struct timespec t;
    if (clock_gettime(CLOCK_MONOTONIC, &t)) return UINT64_MAX;
    return (uint64_t)t.tv_sec * 1000000000ULL + (uint64_t)t.tv_nsec;
}

int main(void)
{
    volatile uint64_t sum = 0;
    uint64_t previous = now();
    if (previous == UINT64_MAX) return 1;
    for (unsigned round = 0; round < 16; ++round) {
        for (uint64_t i = 0; i < 5000000; ++i) sum += i ^ round;
        if (syscall(SYS_getpid) <= 0) return 1;
        uint64_t current = now();
        if (current == UINT64_MAX || current < previous) return 1;
        previous = current;
    }
    printf("STOP_BOUNDARY pass sum=%llu samples=16 end_ns=%llu\n",
           (unsigned long long)sum, (unsigned long long)previous);
    return 0;
}
