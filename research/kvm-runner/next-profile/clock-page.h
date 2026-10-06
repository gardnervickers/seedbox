/* Proposed read-only ABI, not yet installed as a Linux/vDSO clocksource.
 * These loads contain no host/guest timer reads, PIO, syscall or time charge.
 * Progress requires qualified counter preemption/page publication. A passive
 * page never provides autonomous time to a non-preempted polling loop.
 */
#ifndef SEEDBOX_CLOCK_PAGE_H
#define SEEDBOX_CLOCK_PAGE_H
#include <stdint.h>
#include <stddef.h>
struct __attribute__((aligned(64))) seedbox_clock_page {
 uint32_t version, flags;
 uint64_t sequence, machine, incarnation, epoch, conditional_branches, monotonic_ns;
 uint64_t native_cpu_ns, native_branches;
 uint8_t profile_digest[32], reserved[24];
};
_Static_assert(sizeof(struct seedbox_clock_page)==128,"clock ABI size");
_Static_assert(offsetof(struct seedbox_clock_page,monotonic_ns)==48,"clock ABI time");
_Static_assert(offsetof(struct seedbox_clock_page,profile_digest)==72,"clock ABI profile");
static inline int seedbox_clock_read(const struct seedbox_clock_page *p,
 uint64_t machine,uint64_t incarnation,uint64_t epoch,uint64_t *ns) {
 uint64_t before=__atomic_load_n(&p->sequence,__ATOMIC_ACQUIRE);
 if(before&1) return -1; /* Publisher must never resume with odd sequence. */
 if(p->version!=1 || !(p->flags&1) || p->machine!=machine ||
    p->incarnation!=incarnation || p->epoch!=epoch) return -2;
 uint64_t value=__atomic_load_n(&p->monotonic_ns,__ATOMIC_RELAXED);
 __atomic_thread_fence(__ATOMIC_ACQUIRE);
 if(before!=__atomic_load_n(&p->sequence,__ATOMIC_RELAXED)) return -1;
 *ns=value; return 0;
}
#endif
