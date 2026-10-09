/* A weak compare-and-swap whose failures the program observes: it counts
 * them and backs off, exposing retry differences in both output and state. */
#include <stdatomic.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>

int main(int argc, char **argv)
{
    uint64_t iterations = argc > 1 ? strtoull(argv[1], NULL, 10) : 10000000;
    _Atomic uint64_t value = 0;
    uint64_t failures = 0, backoff = 0;
    for (uint64_t i = 0; i < iterations; ++i) {
        uint64_t seen = atomic_load_explicit(&value, memory_order_relaxed);
        while (!atomic_compare_exchange_weak_explicit(&value, &seen, seen + 1,
                                                      memory_order_relaxed,
                                                      memory_order_relaxed)) {
            ++failures;
            for (volatile unsigned k = 0; k < 16; ++k) ++backoff;
        }
    }
    printf("WEAKCAS value=%llu failures=%llu backoff=%llu\n",
           (unsigned long long)value, (unsigned long long)failures,
           (unsigned long long)backoff);
    return value == iterations ? 0 : 1;
}
