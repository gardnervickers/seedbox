
#include "world-clock.h"
#include <assert.h>
int main(void) {
    struct timespec initial = seedbox_wall_clock_at(86400, 999999999);
    struct timespec reboot = seedbox_wall_clock_at(86400, 990560000001ULL);
    assert(initial.tv_sec == 86400 && initial.tv_nsec == 999999999);
    assert(reboot.tv_sec == 87390 && reboot.tv_nsec == 560000001);
    assert(reboot.tv_sec > initial.tv_sec);
    struct timespec rollover = seedbox_wall_clock_at(UINT32_MAX, 1000000001);
    assert((uint64_t)rollover.tv_sec == 4294967296ULL && rollover.tv_nsec == 1);
    struct timespec late = seedbox_wall_clock_at(UINT32_MAX, UINT64_MAX);
    assert((uint64_t)late.tv_sec == 22741711368ULL && late.tv_nsec == 709551615);
}
