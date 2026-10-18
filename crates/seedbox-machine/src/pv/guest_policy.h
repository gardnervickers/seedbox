/* Specific named-backend controller override. Archive recipe bytes stay owned
 * by preparation; absent override preserves the legacy/environment decision. */
#ifndef SEEDBOX_GUEST_POLICY_H
#define SEEDBOX_GUEST_POLICY_H
#include <ctype.h>
#include <errno.h>
#include <stddef.h>
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
#endif
