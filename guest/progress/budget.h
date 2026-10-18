/* Strict, libc-independent parsing for the retained PV instrumentation options. */
#ifndef SEEDBOX_PROGRESS_BUDGET_H
#define SEEDBOX_PROGRESS_BUDGET_H
#include <stdint.h>
/* First argument of the progress getpid. The guest kernels (kernel/seedbox.h,
 * kernel-arm64/seedbox.c) recognize it and charge progress jitter there. */
#define SEEDBOX_PROGRESS_MARK 0x5eedb0c5u
#define SEEDBOX_PROGRESS_INSTRUCTIONS "--instructions-per-progress-call"
#define SEEDBOX_PROGRESS_POINTS "--progress-points-per-block"
/* Optional: record edge coverage into files in this directory. A resolved
 * input (the machine's SEEDBOX_COVERAGE=1); its instrumentation and file
 * writes change timing, so runs replay only within the same setting. */
#define SEEDBOX_PROGRESS_COVERAGE "--coverage-dir"
/* Coverage counters per process file: one byte (wrapping) per hashed edge
 * (previous block, block), a power of two. The supervisor's buckets turn
 * each count into one of SEEDBOX_COVERAGE_BUCKETS features. */
#define SEEDBOX_COVERAGE_EDGES 8192
#define SEEDBOX_COVERAGE_BUCKETS 4
/* Parse `option value` when `option` is exactly `expected` and `value` is a
 * decimal u32 above zero, with no sign, spaces or other characters. */
static int seedbox_progress_option(const char *expected, const char *option,
                                   const char *value, uint32_t *parsed)
{
    while (*expected && *option == *expected) { ++option; ++expected; }
    if (*option || *expected || !*value) return 0;
    uint32_t result = 0;
    for (; *value; ++value) {
        if (*value < '0' || *value > '9') return 0;
        uint32_t digit = (uint32_t)(*value - '0');
        if (result > (UINT32_MAX - digit) / 10) return 0;
        result = result * 10 + digit;
    }
    if (!result) return 0;
    *parsed = result;
    return 1;
}
static int seedbox_progress_same(const char *a, const char *b)
{
    while (*a && *a == *b) { ++a; ++b; }
    return !*a && !*b;
}
/* The client takes both options, in this order, then optionally a coverage
 * directory; `coverage` stays NULL without it. */
static int seedbox_progress_arguments(int argc, const char *argv[], uint32_t *instructions,
                                      uint32_t *points, const char **coverage)
{
    *coverage = 0;
    if (argc == 7) {
        if (!seedbox_progress_same(SEEDBOX_PROGRESS_COVERAGE, argv[5]) || !*argv[6]) return 0;
        *coverage = argv[6];
    } else if (argc != 5) {
        return 0;
    }
    return seedbox_progress_option(SEEDBOX_PROGRESS_INSTRUCTIONS, argv[1], argv[2], instructions)
        && seedbox_progress_option(SEEDBOX_PROGRESS_POINTS, argv[3], argv[4], points);
}
#endif
