/* An exclusive sequence with a conditional branch between its load and its
 * store, so DynamoRIO's unpacked form has a block (the add) that contains no
 * exclusive instruction. With PACKED the sequence stays packable; otherwise
 * an adr inside keeps DynamoRIO from packing it. Prints the final value and
 * how many stores failed. */
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>

int main(int argc, char **argv)
{
    uint64_t iterations = argc > 1 ? strtoull(argv[1], NULL, 10) : 10000000;
    uint64_t value = 0, failures = 0;
    for (uint64_t i = 0; i < iterations; ++i) {
        uint64_t loaded, sum;
        unsigned failed;
        __asm__ volatile(
            "0:\n\t"
            "ldxr %0, [%4]\n\t"
#ifndef PACKED
            "adr x9, 0b\n\t"
#endif
            "tbz %w5, #0, 1f\n\t"
            "add %1, %0, #2\n\t"
            "b 2f\n\t"
            "1:\n\t"
            "add %1, %0, #1\n\t"
            "2:\n\t"
            "stxr %w2, %1, [%4]\n\t"
            "cbnz %w2, 3f\n\t"
            "b 4f\n\t"
            "3:\n\t"
            "add %3, %3, #1\n\t"
            "b 0b\n\t"
            "4:"
            : "=&r"(loaded), "=&r"(sum), "=&r"(failed), "+r"(failures)
            : "r"(&value), "r"((unsigned)i)
            : "x9", "memory", "cc");
    }
    uint64_t expect = iterations / 2 * 3 + (iterations % 2 ? 1 : 0);
    printf("SPLIT value=%llu expect=%llu failures=%llu\n", (unsigned long long)value,
           (unsigned long long)expect, (unsigned long long)failures);
    return value == expect ? 0 : 1;
}
