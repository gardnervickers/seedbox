/* Shared diagnostic data, not an application publication API. */
#ifndef PARKED_PUBLICATION_CONTEXT_H
#define PARKED_PUBLICATION_CONTEXT_H
#include <stdint.h>
struct parked_state {
    uint64_t gpr[31], sp, nzcv, fpsr, fpcr, tls;
    uint8_t q[32][16];
};
#endif
