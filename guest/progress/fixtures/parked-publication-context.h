/* Shared diagnostic data, not an application publication API. */
#ifndef PARKED_PUBLICATION_CONTEXT_H
#define PARKED_PUBLICATION_CONTEXT_H
#include <stdint.h>
struct parked_state {
    uint64_t gpr[31], sp, nzcv, fpsr, fpcr, tls;
    uint8_t q[32][16];
};
struct parked_progress_diagnostic {
    uint64_t old_charge, fresh_charge;
    int64_t initial_budget, after_add, before_signal, zero_budget;
    uint64_t result;
    int iteration, armed, payments, deferred, released;
};
#endif
