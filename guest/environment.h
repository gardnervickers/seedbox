#ifndef SEEDBOX_ENVIRONMENT_H
#define SEEDBOX_ENVIRONMENT_H
#include <errno.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

/* The launch recipe has already merged image and Compose environment entries.
 * Keep explicit values, including empty ones, and append only a missing default. */
static int seedbox_default_gomaxprocs(char ***environment, long online_cpus) {
    size_t count = 0;
    while ((*environment)[count]) {
        if (!strncmp((*environment)[count], "GOMAXPROCS=", 11)) return 0;
        count++;
    }
    if (online_cpus < 1) { errno = EINVAL; return -1; }
    char value[11 + sizeof(long)*3 + 1];
    int length = snprintf(value, sizeof(value), "GOMAXPROCS=%ld", online_cpus);
    if (length < 0 || (size_t)length >= sizeof(value)) { errno = EOVERFLOW; return -1; }
    char *entry = malloc((size_t)length + 1);
    if (!entry) return -1;
    memcpy(entry, value, (size_t)length + 1);
    char **entries = realloc(*environment, (count + 2)*sizeof(char *));
    if (!entries) { free(entry); return -1; }
    entries[count] = entry;
    entries[count + 1] = NULL;
    *environment = entries;
    return 0;
}
#endif
