/* Public flush contracts derived from api/a64_prop_flush.c at 356c1821.
 * This finite shared-ISA case does not replace the source multiworker epochs,
 * reset observation, generated fault-state or presyscall issuer populations. */
#include <stdio.h>
#include <stdint.h>
__attribute__((visibility("default"), noinline))
uint64_t translator_target(uint64_t x) { return x * 17 + 3; }
__attribute__((visibility("default"), noinline))
void translator_boundary(void) { __asm__ volatile("" ::: "memory"); }
int main(void)
{
    uint64_t sum = 0;
    for (uint64_t i = 0; i < 256; ++i) {
        sum += translator_target(i);
        translator_boundary();
    }
    if (sum != 555648) return 1;
    puts("flush: 256 values checked");
    return 0;
}
