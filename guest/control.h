/* Management ABI: action SP nonzero decimal request ID LF, where `clock_step`
 * adds SP and a signed decimal nanosecond step and `clock_rate` adds SP and a
 * signed decimal rate in parts per billion. Uncorrelated legacy
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
static int control_decimal(const char *p, uint64_t limit, uint64_t *out) {
    uint64_t value = 0;
    if (!*p || (*p == '0' && p[1])) return -1;
    for (; *p; p++) {
        if (*p < '0' || *p > '9') return -1;
        value = value * 10 + (uint64_t)(*p - '0');
        if (value > limit) return -1;
    }
    *out = value;
    return 0;
}
/* `clock_step ID NS` also carries a signed decimal nanosecond step for the
 * guest's CLOCK_REALTIME, and `clock_rate ID PPB` the system clock's rate
 * error; other commands carry no argument. */
static int control_parse(char *command, char **action, uint32_t *id, int64_t *step) {
    *id = 0; *step = 0; *action = command;
    char *space = strchr(command, ' '), *argument = NULL;
    if (space) {
        *space++ = 0;
        argument = strchr(space, ' ');
        if (argument) *argument++ = 0;
        uint64_t value;
        if (control_decimal(space, UINT32_MAX, &value) || !value) return -1;
        *id = (uint32_t)value;
    }
    if (!strcmp(*action, "clock_step") || !strcmp(*action, "clock_rate")) {
        if (!*id || !argument) return -1;
        int negative = *argument == '-';
        uint64_t magnitude;
        if (control_decimal(argument + negative, negative ? (uint64_t)INT64_MAX + 1 : INT64_MAX, &magnitude)) return -1;
        if (negative && !magnitude) return -1;
        *step = negative ? (int64_t)(0 - magnitude) : (int64_t)magnitude;
        return 0;
    }
    if (argument) return -1;
    static const char *const actions[] = {"stop", "start", "freeze", "thaw"};
    for (unsigned i = 0; i < sizeof(actions) / sizeof(*actions); i++)
        if (!strcmp(*action, actions[i])) return 0;
    return -1;
}
/* `exec ID HEX`: run the argv HEX encodes (its arguments joined by NUL, in
 * lowercase hex) beside the application. Returns 1 for an exec command,
 * decoding the argv in place into `*argv` (at most `max` pointers, NULL
 * terminated); 0 for any other command; -1 for a malformed exec. */
static int control_exec(char *command, uint32_t *id, char **argv, unsigned max) {
    if (strncmp(command, "exec ", 5)) return 0;
    char *hex = strchr(command + 5, ' ');
    if (!hex) return -1;
    *hex++ = 0;
    uint64_t value;
    if (control_decimal(command + 5, UINT32_MAX, &value) || !value) return -1;
    *id = (uint32_t)value;
    size_t length = strlen(hex);
    if (!length || length % 2) return -1;
    size_t bytes = length / 2;
    for (size_t i = 0; i < bytes; i++) {
        int byte = 0;
        for (int half = 0; half < 2; half++) {
            char c = hex[2 * i + half];
            int nibble = c >= '0' && c <= '9' ? c - '0' : c >= 'a' && c <= 'f' ? c - 'a' + 10 : -1;
            if (nibble < 0) return -1;
            byte = byte * 16 + nibble;
        }
        hex[i] = (char)byte;
    }
    hex[bytes] = 0;
    /* Arguments are separated by NUL, with none after the last. */
    unsigned count = 0;
    argv[count++] = hex;
    for (size_t i = 0; i < bytes; i++) {
        if (hex[i]) continue;
        if (count + 1 >= max) return -1;
        argv[count++] = hex + i + 1;
    }
    argv[count] = 0;
    return argv[0][0] ? 1 : -1;
}
#endif
