/* Ordinary single-threaded computation: no instrumentation or yield hooks. */
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>

__attribute__((noinline)) static uint64_t work(uint64_t n) {
    uint64_t x = 123456789;
    for (uint64_t i = 0; i < n; ++i) {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x += i;
    }
    return x;
}
int main(int argc, char **argv) {
    uint64_t n = argc == 2 ? strtoull(argv[1], NULL, 10) : 200000000;
    printf("%016llx\n", (unsigned long long)work(n));
    return 0;
}
