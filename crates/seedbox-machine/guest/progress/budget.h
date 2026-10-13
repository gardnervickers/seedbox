/* Strict, libc-independent parsing for the retained PV instrumentation budget. */
#ifndef SEEDBOX_PROGRESS_BUDGET_H
#define SEEDBOX_PROGRESS_BUDGET_H
#include <stdint.h>
static int seedbox_progress_budget(const char *option, const char *value, uint32_t *budget)
{
    const char *expected = "--blocks-per-progress-call";
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
    *budget = result;
    return 1;
}
#endif
