/* Strict, libc-independent parsing for the retained PV instrumentation options,
 * and the seeded progress jitter draw. */
#ifndef SEEDBOX_PROGRESS_BUDGET_H
#define SEEDBOX_PROGRESS_BUDGET_H
#include <stdint.h>
#define SEEDBOX_PROGRESS_INSTRUCTIONS "--instructions-per-progress-call"
#define SEEDBOX_PROGRESS_POINTS "--progress-points-per-block"
#define SEEDBOX_PROGRESS_JITTER "--progress-jitter-ns"
/* Nominal virtual time of one progress call. Each machine's seeded speed and
 * any throttle scale the real charge, and jitter with it. */
#define SEEDBOX_PROGRESS_CALL_NS 1000u
#define SEEDBOX_PROGRESS_JITTER_MAX_NS 1000000u
/* Parse `option value` when `option` is exactly `expected` and `value` is a
 * decimal u32 from `min` to `max`, with no sign, spaces or other characters. */
static int seedbox_progress_option(const char *expected, const char *option,
                                   const char *value, uint32_t min, uint32_t max,
                                   uint32_t *parsed)
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
    if (result < min || result > max) return 0;
    *parsed = result;
    return 1;
}
/* The client takes exactly these options, in this order. A zero jitter is off. */
static int seedbox_progress_arguments(int argc, const char *argv[], uint32_t *instructions,
                                      uint32_t *points, uint32_t *jitter_ns)
{
    return argc == 7
        && seedbox_progress_option(SEEDBOX_PROGRESS_INSTRUCTIONS, argv[1], argv[2], 1,
                                   UINT32_MAX, instructions)
        && seedbox_progress_option(SEEDBOX_PROGRESS_POINTS, argv[3], argv[4], 1, UINT32_MAX,
                                   points)
        && seedbox_progress_option(SEEDBOX_PROGRESS_JITTER, argv[5], argv[6], 0,
                                   SEEDBOX_PROGRESS_JITTER_MAX_NS, jitter_ns);
}
/* splitmix64: one 64-bit word of state per thread. */
static inline uint64_t seedbox_progress_next(uint64_t *state)
{
    uint64_t z = (*state += 0x9e3779b97f4a7c15ull);
    z = (z ^ (z >> 30)) * 0xbf58476d1ce4e5b9ull;
    z = (z ^ (z >> 27)) * 0x94d049bb133111ebull;
    return z ^ (z >> 31);
}
/* Extra progress calls for one progress call: draw d uniformly from
 * [0, jitter_ns] and charge d / SEEDBOX_PROGRESS_CALL_NS calls, plus one more
 * with probability equal to the remainder's fraction, so the expected extra
 * time is exactly d. Always two draws, so the stream never depends on d. */
static inline uint32_t seedbox_progress_extra_calls(uint64_t *state, uint32_t jitter_ns)
{
    uint32_t d = (uint32_t)(seedbox_progress_next(state) % ((uint64_t)jitter_ns + 1));
    uint32_t round = (uint32_t)(seedbox_progress_next(state) % SEEDBOX_PROGRESS_CALL_NS);
    return d / SEEDBOX_PROGRESS_CALL_NS + (round < d % SEEDBOX_PROGRESS_CALL_NS);
}
#endif
