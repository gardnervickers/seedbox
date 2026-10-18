#include "control.h"
#include <assert.h>
int main(void) {
    char *action; uint32_t id; int64_t step;
    /* Pure ABI bounds: execute no privileged port I/O on the test host. */
    assert(SEEDBOX_READY_ID == 0);
    assert(SEEDBOX_EFFECT_PORT == 0xf6 && SEEDBOX_FAILURE_PORT == 0xf7);
    assert(SEEDBOX_EFFECT_PORT + sizeof(uint32_t) <= SEEDBOX_EFFECT_PORT + SEEDBOX_EFFECT_PORT_COUNT);
    assert(SEEDBOX_FAILURE_PORT + sizeof(uint32_t) <= SEEDBOX_EFFECT_PORT + SEEDBOX_EFFECT_PORT_COUNT);
    char good[][32] = {"stop 1", "start 4294967295", "stop", "start", "freeze 7", "thaw 8"};
    for (unsigned i=0;i<6;i++) { assert(!control_parse(good[i],&action,&id,&step)); assert(id == (i==0 ? 1 : i==1 ? UINT32_MAX : i>=4 ? i+3 : 0) && !step); }
    char clocks[][64] = {"clock_step 7 1500000000", "clock_step 8 -1", "clock_step 9 0",
                         "clock_step 10 9223372036854775807", "clock_step 11 -9223372036854775808"};
    int64_t steps[] = {1500000000, -1, 0, INT64_MAX, INT64_MIN};
    for (unsigned i=0;i<5;i++) {
        assert(!control_parse(clocks[i],&action,&id,&step));
        assert(!strcmp(action, "clock_step") && id == 7 + i && step == steps[i]);
    }
    char rates[][64] = {"clock_rate 3 -100000000", "clock_rate 4 0", "clock_rate 5 25"};
    int64_t ppbs[] = {-100000000, 0, 25};
    for (unsigned i=0;i<3;i++) {
        assert(!control_parse(rates[i],&action,&id,&step));
        assert(!strcmp(action, "clock_rate") && id == 3 + i && step == ppbs[i]);
    }
    char bad[][64] = {"stop 0","stop 01","start -1","start 4294967296","stop 1 extra","stop  1","go 1","start ","start 184467440737095516160",
                      "clock_step","clock_step 1","clock_step 1 ","clock_step 1 -0","clock_step 1 +5","clock_step 1 05","clock_step 1 1x",
                      "clock_step 1 9223372036854775808","clock_step 1 -9223372036854775809","clock_step 0 5","clock_step 1 5 6","stop 1 5",
                      "clock_rate","clock_rate 1","clock_rate 1 -0","clock_rate 1 +5","clock_rate 0 5","clock_rate 1 5 6",
                      "freez 1","thaw 0","freeze 1 5"};
    for (unsigned i=0;i<sizeof(bad)/sizeof(*bad);i++) assert(control_parse(bad[i],&action,&id,&step));
    /* exec ID HEX: "cat\0/data/a b" and a lone "ls". */
    char *argv[8];
    char exec1[] = "exec 3 636174002f646174612f612062";
    assert(control_exec(exec1, &id, argv, 8) == 1 && id == 3);
    assert(!strcmp(argv[0], "cat") && !strcmp(argv[1], "/data/a b") && !argv[2]);
    char exec2[] = "exec 9 6c73";
    assert(control_exec(exec2, &id, argv, 8) == 1 && id == 9 && !strcmp(argv[0], "ls") && !argv[1]);
    char other[] = "stop 1";
    assert(control_exec(other, &id, argv, 8) == 0);
    char bad_exec[][32] = {"exec 1", "exec 0 6c73", "exec 1 6c7", "exec 1 6C73", "exec 1 zz", "exec 1 006c", "exec 1 "};
    for (unsigned i=0;i<sizeof(bad_exec)/sizeof(*bad_exec);i++) assert(control_exec(bad_exec[i],&id,argv,8) == -1);
    char many[] = "exec 1 610061006100610061006100610061";
    assert(control_exec(many, &id, argv, 8) == -1);
}
