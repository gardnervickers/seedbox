#include <stdint.h>
#include <stdio.h>
int main(void) {
    uint64_t value = 0, failures = 0;
    for (uint64_t i = 0; i < 100000000; ++i) {
        uint64_t loaded;
        unsigned failed;
        do {
            __asm__ volatile("ldxr %0, [%2]\n\tcbnz %0, 1f\n\tnop\n1:\tadd %0, %0, #1\n\tstxr %w1, %0, [%2]"
                : "=&r"(loaded), "=&r"(failed) : "r"(&value) : "memory");
            failures += failed;
        } while (failed);
    }
    printf("value=%llu failures=%llu\n", (unsigned long long)value,
           (unsigned long long)failures);
}
