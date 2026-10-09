#include "control.h"
#include <assert.h>
int main(void) {
    char *action; uint32_t id; int64_t step;
    /* Pure ABI bounds: execute no privileged port I/O on the test host. */
    assert(SEEDBOX_READY_ID == 0);
    assert(SEEDBOX_EFFECT_PORT == 0xf6 && SEEDBOX_FAILURE_PORT == 0xf7);
    assert(SEEDBOX_EFFECT_PORT + sizeof(uint32_t) <= SEEDBOX_EFFECT_PORT + SEEDBOX_EFFECT_PORT_COUNT);
    assert(SEEDBOX_FAILURE_PORT + sizeof(uint32_t) <= SEEDBOX_EFFECT_PORT + SEEDBOX_EFFECT_PORT_COUNT);
    char good[][32] = {"stop 1", "start 4294967295", "stop", "start"};
    for (unsigned i=0;i<4;i++) { assert(!control_parse(good[i],&action,&id,&step)); assert(id == (i==0 ? 1 : i==1 ? UINT32_MAX : 0) && !step); }
    char clocks[][64] = {"clock_step 7 1500000000", "clock_step 8 -1", "clock_step 9 0",
                         "clock_step 10 9223372036854775807", "clock_step 11 -9223372036854775808"};
    int64_t steps[] = {1500000000, -1, 0, INT64_MAX, INT64_MIN};
    for (unsigned i=0;i<5;i++) {
        assert(!control_parse(clocks[i],&action,&id,&step));
        assert(!strcmp(action, "clock_step") && id == 7 + i && step == steps[i]);
    }
    char bad[][64] = {"stop 0","stop 01","start -1","start 4294967296","stop 1 extra","stop  1","go 1","start ","start 184467440737095516160",
                      "clock_step","clock_step 1","clock_step 1 ","clock_step 1 -0","clock_step 1 +5","clock_step 1 05","clock_step 1 1x",
                      "clock_step 1 9223372036854775808","clock_step 1 -9223372036854775809","clock_step 0 5","clock_step 1 5 6","stop 1 5"};
    for (unsigned i=0;i<sizeof(bad)/sizeof(*bad);i++) assert(control_parse(bad[i],&action,&id,&step));
}
