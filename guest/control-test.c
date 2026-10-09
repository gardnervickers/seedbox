#include "control.h"
#include <assert.h>
int main(void) {
    char *action; uint32_t id;
    /* Pure ABI bounds: execute no privileged port I/O on the test host. */
    assert(SEEDBOX_READY_ID == 0);
    assert(SEEDBOX_EFFECT_PORT == 0xf6 && SEEDBOX_FAILURE_PORT == 0xf7);
    assert(SEEDBOX_EFFECT_PORT + sizeof(uint32_t) <= SEEDBOX_EFFECT_PORT + SEEDBOX_EFFECT_PORT_COUNT);
    assert(SEEDBOX_FAILURE_PORT + sizeof(uint32_t) <= SEEDBOX_EFFECT_PORT + SEEDBOX_EFFECT_PORT_COUNT);
    char good[][32] = {"stop 1", "start 4294967295", "stop", "start", "freeze 7", "thaw 8"};
    for (unsigned i=0;i<6;i++) { assert(!control_parse(good[i],&action,&id)); assert(id == (i==0 ? 1 : i==1 ? UINT32_MAX : i>=4 ? i+3 : 0)); }
    char bad[][64] = {"stop 0","stop 01","start -1","start 4294967296","stop 1 extra","stop  1","go 1","start ","start 184467440737095516160","freez 1","thaw 0"};
    for (unsigned i=0;i<sizeof(bad)/sizeof(*bad);i++) assert(control_parse(bad[i],&action,&id));
}
