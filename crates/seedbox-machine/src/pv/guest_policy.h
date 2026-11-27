/* Per-boot kernel arguments the host appends: the named-backend controller
 * override and the boot's realtime offset. Archive recipe bytes stay owned
 * by preparation; an absent override keeps the guest environment's decision. */
#ifndef SEEDBOX_GUEST_POLICY_H
#define SEEDBOX_GUEST_POLICY_H
#include <ctype.h>
#include <errno.h>
#include <stddef.h>
#include <stdint.h>
#include <string.h>
static inline int sbx_control_mode(const char *command, int fallback, int *enabled) {
    static const char key[] = "seedbox.control=";
    int found = 0, value = fallback;
    while (*command) {
        while (isspace((unsigned char)*command)) ++command;
        const char *end = command;
        while (*end && !isspace((unsigned char)*end)) ++end;
        size_t length = (size_t)(end - command);
        if (length >= sizeof(key) - 1 && !memcmp(command, key, sizeof(key) - 1)) {
            if (found || length != sizeof(key) || (command[sizeof(key)-1] != '0' && command[sizeof(key)-1] != '1')) {
                errno = EINVAL; return -1;
            }
            found = 1; value = command[sizeof(key)-1] == '1';
        }
        command = end;
    }
    *enabled = value;
    return 0;
}
/* The boot's realtime offset in nanoseconds, seedbox.clock_offset_ns=<signed
 * decimal>, at most one second either way. Every host boot appends exactly
 * one, so an absent or repeated argument is a host/init mismatch (EINVAL). */
static inline int sbx_clock_offset(const char *command, int64_t *offset) {
    static const char key[] = "seedbox.clock_offset_ns=";
    int found = 0;
    int64_t value = 0;
    while (*command) {
        while (isspace((unsigned char)*command)) ++command;
        const char *end = command;
        while (*end && !isspace((unsigned char)*end)) ++end;
        size_t length = (size_t)(end - command);
        if (length >= sizeof(key) - 1 && !memcmp(command, key, sizeof(key) - 1)) {
            const char *digit = command + sizeof(key) - 1;
            int negative = digit < end && *digit == '-';
            if (negative) ++digit;
            if (found || digit == end) { errno = EINVAL; return -1; }
            int64_t magnitude = 0;
            for (; digit < end; ++digit) {
                if (*digit < '0' || *digit > '9') { errno = EINVAL; return -1; }
                magnitude = magnitude * 10 + (*digit - '0');
                if (magnitude > 1000000000) { errno = EINVAL; return -1; }
            }
            found = 1; value = negative ? -magnitude : magnitude;
        }
        command = end;
    }
    if (!found) { errno = EINVAL; return -1; }
    *offset = value;
    return 0;
}
#endif
