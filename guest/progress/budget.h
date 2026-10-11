/* Strict, libc-independent parsing for the retained PV instrumentation options. */
#ifndef SEEDBOX_PROGRESS_BUDGET_H
#define SEEDBOX_PROGRESS_BUDGET_H
#include <stdint.h>
/* First argument of the progress getpid. The guest kernels (kernel/seedbox.h,
 * kernel-arm64/seedbox.c) recognize it and charge progress jitter there. */
#define SEEDBOX_PROGRESS_MARK 0x5eedb0c5u
#define SEEDBOX_PROGRESS_INSTRUCTIONS "--instructions-per-progress-call"
#define SEEDBOX_PROGRESS_POINTS "--progress-points-per-block"
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
/* The client takes exactly both options, in this order. */
static int seedbox_progress_arguments(int argc, const char *argv[], uint32_t *instructions,
                                      uint32_t *points)
{
    return argc == 5
        && seedbox_progress_option(SEEDBOX_PROGRESS_INSTRUCTIONS, argv[1], argv[2], instructions)
        && seedbox_progress_option(SEEDBOX_PROGRESS_POINTS, argv[3], argv[4], points);
}
#endif
