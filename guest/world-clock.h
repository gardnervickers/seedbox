/* Guest wall time is the recorded epoch plus the world's elapsed time.
 * A new kernel's CLOCK_MONOTONIC starts at boot, so it cannot supply this
 * offset after a machine reboot. Both Seedbox kernels emulate userspace
 * counter reads as global PV nanoseconds (their counter rate is 1 GHz). */
#ifndef SEEDBOX_WORLD_CLOCK_H
#define SEEDBOX_WORLD_CLOCK_H
#include <stdint.h>
#include <time.h>

static inline uint64_t seedbox_world_time_ns(void) {
#if defined(__x86_64__)
    uint32_t lo, hi;
    __asm__ volatile("rdtsc" : "=a"(lo), "=d"(hi) :: "memory");
    return ((uint64_t)hi << 32) | lo;
#elif defined(__aarch64__)
    uint64_t value;
    __asm__ volatile("mrs %0, cntvct_el0" : "=r"(value) :: "memory");
    return value;
#else
#error Seedbox requires x86-64 or AArch64
#endif
}

static inline struct timespec seedbox_wall_clock_at(uint32_t epoch, uint64_t world_ns) {
    return (struct timespec) {
        .tv_sec = (time_t)((uint64_t)epoch + world_ns / 1000000000ULL),
        .tv_nsec = (long)(world_ns % 1000000000ULL),
    };
}
#endif
