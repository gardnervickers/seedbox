#include "control.h"
#include <assert.h>
int main(void) {
    char *action, *args[SEEDBOX_COMMAND_ARGS]; uint32_t id; int count;
    /* Pure ABI bounds: execute no privileged port I/O on the test host. */
    assert(SEEDBOX_READY_ID == 0);
    assert(SEEDBOX_EFFECT_PORT == 0xf6 && SEEDBOX_FAILURE_PORT == 0xf7);
    assert(SEEDBOX_EFFECT_PORT + sizeof(uint32_t) <= SEEDBOX_EFFECT_PORT + SEEDBOX_EFFECT_PORT_COUNT);
    assert(SEEDBOX_FAILURE_PORT + sizeof(uint32_t) <= SEEDBOX_EFFECT_PORT + SEEDBOX_EFFECT_PORT_COUNT);
    char stop[] = "stop 1 server";
    assert(!control_parse(stop, &action, &id, args, &count));
    assert(!strcmp(action, "stop") && id == 1 && count == 1 && !strcmp(args[0], "server"));
    char start[] = "start 4294967295 a";
    assert(!control_parse(start, &action, &id, args, &count) && id == UINT32_MAX);
    char partition[] = "partition 7 p3 a,b c";
    assert(!control_parse(partition, &action, &id, args, &count));
    assert(count == 3 && !strcmp(args[0], "p3") && !strcmp(args[1], "a,b") && !strcmp(args[2], "c"));
    char heal[] = "heal 8 p3";
    assert(!control_parse(heal, &action, &id, args, &count) && !strcmp(args[0], "p3"));
    char bad[][64] = {"stop", "start", "stop 1", "stop 0 a", "stop 01 a", "start -1 a", "start 4294967296 a",
                      "stop 1 a extra", "stop  1 a", "stop 1 a ", " stop 1 a", "go 1 a", "start ",
                      "heal 1", "partition 1 p a", "partition 1 p a b c", "start 184467440737095516160 a", ""};
    for (unsigned i = 0; i < sizeof(bad)/sizeof(*bad); i++) assert(control_parse(bad[i], &action, &id, args, &count));
}
