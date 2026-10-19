/* Original DR:a64_prop_signal.c:2617-2633 expiration accounting oracle.
 * Copyright (c) 2026 Google, Inc.; see LICENSE. Explicit clock inputs from live observations or pure model cases. */
#ifndef SIGNAL_STORM_MODEL_H
#define SIGNAL_STORM_MODEL_H
#include "common.h"
static inline int storm_accounted(uint64_t start, uint64_t armed, uint64_t cut,
                                 uint64_t end, uint64_t period, long accounted, int got)
{
    long expirations = (long)((end - start) / period);
    long expirations_min = (long)((cut - armed) / period);
    return got && accounted >= expirations_min - 2 && accounted <= expirations + 2;
}
#endif
