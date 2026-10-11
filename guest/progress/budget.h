/* Strict, libc-independent parsing for the retained PV instrumentation budget,
 * and the seeded progress jitter draw. */
#ifndef SEEDBOX_PROGRESS_BUDGET_H
#define SEEDBOX_PROGRESS_BUDGET_H
#include <stdint.h>

/* Nominal virtual time of one progress call. Each machine's seeded speed and
 * any throttle scale the real charge, and jitter with it. */
#define SEEDBOX_PROGRESS_CALL_NS 1000u
#define SEEDBOX_PROGRESS_JITTER_MAX_NS 1000000u

static inline int seedbox_progress_option(const char *option, const char *expected,
                                          const char *value, uint32_t max, uint32_t *parsed)
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
    if (!result || result > max) return 0;
    *parsed = result;
    return 1;
}

static inline int seedbox_progress_budget(const char *option, const char *value, uint32_t *budget)
{
    return seedbox_progress_option(option, "--blocks-per-progress-call", value, UINT32_MAX, budget);
}

/* Present only when jitter is on, so zero is rejected like an absent option. */
static inline int seedbox_progress_jitter(const char *option, const char *value, uint32_t *jitter_ns)
{
    return seedbox_progress_option(option, "--progress-jitter-ns", value,
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

/* Extra progress calls for one checkpoint: draw d uniformly from
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
