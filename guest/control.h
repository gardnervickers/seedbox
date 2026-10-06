/* Management ABI: action SP nonzero decimal request ID LF. Uncorrelated legacy
 * commands remain available for existing raw-world fixtures, with ID zero.
 * A success-port OUTL with ID zero announces supervisor readiness once the
 * management terminal and initial application are ready for commands. */
#ifndef SEEDBOX_CONTROL_H
#define SEEDBOX_CONTROL_H
#include <stdint.h>
#include <string.h>
/* OUTL accesses four consecutive port bytes. The two possible start ports
 * therefore require the union [0xf6, 0xfa], five bytes in total. */
enum { SEEDBOX_READY_ID = 0, SEEDBOX_EFFECT_PORT = 0xf6, SEEDBOX_FAILURE_PORT = 0xf7,
       SEEDBOX_EFFECT_PORT_COUNT = 5 };
_Static_assert(SEEDBOX_EFFECT_PORT_COUNT >= sizeof(uint32_t) +
               SEEDBOX_FAILURE_PORT - SEEDBOX_EFFECT_PORT,
               "effect port permissions must cover both complete OUTL writes");
static int control_parse(char *command, char **action, uint32_t *id) {
    *id = 0; *action = command;
    char *space = strchr(command, ' ');
    if (space) {
        *space++ = 0;
        if (!*space || *space == '0') return -1;
        uint64_t value = 0;
        for (char *p = space; *p; p++) {
            if (*p < '0' || *p > '9') return -1;
            value = value * 10 + (*p - '0');
            if (value > UINT32_MAX) return -1;
        }
        *id = (uint32_t)value;
    }
    return strcmp(*action, "stop") && strcmp(*action, "start") ? -1 : 0;
}
#endif
