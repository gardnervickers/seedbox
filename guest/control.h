/* Management ABI: one command per line from the VMM, fields separated by
 * single spaces:
 *   stop ID SERVICE | start ID SERVICE | partition ID HANDLE LEFT RIGHT | heal ID HANDLE
 * ID is a nonzero decimal request ID without leading zeros. LEFT and RIGHT are
 * comma-separated service names. A success-port OUTL with ID zero announces
 * supervisor readiness once the management terminal and initial applications
 * are ready for commands; later OUTLs acknowledge one request ID each. */
#ifndef SEEDBOX_CONTROL_H
#define SEEDBOX_CONTROL_H
#include <stdint.h>
#include <string.h>
/* OUTL accesses four consecutive port bytes. The two possible start ports
 * therefore require the union [0xf6, 0xfa], five bytes in total. */
enum { SEEDBOX_READY_ID = 0, SEEDBOX_EFFECT_PORT = 0xf6, SEEDBOX_FAILURE_PORT = 0xf7,
       SEEDBOX_EFFECT_PORT_COUNT = 5, SEEDBOX_COMMAND_ARGS = 3 };
_Static_assert(SEEDBOX_EFFECT_PORT_COUNT >= sizeof(uint32_t) +
               SEEDBOX_FAILURE_PORT - SEEDBOX_EFFECT_PORT,
               "effect port permissions must cover both complete OUTL writes");
/* Split COMMAND in place. Returns 0 for a known action with its exact number
 * of nonempty arguments, -1 otherwise. */
static int control_parse(char *command, char **action, uint32_t *id,
                         char *args[SEEDBOX_COMMAND_ARGS], int *count) {
    char *fields[2 + SEEDBOX_COMMAND_ARGS];
    int n = 0;
    for (char *p = command;; p++) {
        if (n == 2 + SEEDBOX_COMMAND_ARGS || !*p || *p == ' ') return -1;
        fields[n++] = p;
        p = strchr(p, ' ');
        if (!p) break;
        *p = 0;
    }
    if (n < 3 || *fields[1] == '0') return -1;
    uint64_t value = 0;
    for (char *p = fields[1]; *p; p++) {
        if (*p < '0' || *p > '9') return -1;
        value = value * 10 + (uint64_t)(*p - '0');
        if (value > UINT32_MAX) return -1;
    }
    *action = fields[0]; *id = (uint32_t)value; *count = n - 2;
    for (int i = 2; i < n; i++) args[i - 2] = fields[i];
    int expected = !strcmp(*action, "partition") ? 3
        : (!strcmp(*action, "stop") || !strcmp(*action, "start") || !strcmp(*action, "heal")) ? 1 : -1;
    return *count == expected ? 0 : -1;
}
#endif
