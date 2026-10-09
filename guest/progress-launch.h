/* Resolve execvp-style paths and Linux script interpreters before handing an
 * executable to DynamoRIO. The caller has already installed the image's user,
 * working directory and environment. Return NULL with errno on launch errors.
 */
#ifndef SEEDBOX_PROGRESS_LAUNCH_H
#define SEEDBOX_PROGRESS_LAUNCH_H
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

static char *seedbox_executable(const char *name, int search) {
    if (!search || strchr(name, '/')) return strdup(name);
    const char *path = getenv("PATH");
    if (!path) path = "/bin:/usr/bin";
    int denied = 0;
    while (1) {
        const char *end = strchr(path, ':');
        size_t size = end ? (size_t)(end - path) : strlen(path);
        char *candidate = malloc(size + strlen(name) + 2);
        if (!candidate) return NULL;
        memcpy(candidate, path, size);
        if (size) candidate[size++] = '/';
        strcpy(candidate + size, name);
        if (!access(candidate, X_OK)) return candidate;
        if (errno == EACCES) denied = 1;
        else if (errno != ENOENT && errno != ENOTDIR) { free(candidate); return NULL; }
        free(candidate);
        if (!end) break;
        path = end + 1;
    }
    errno = denied ? EACCES : ENOENT;
    return NULL;
}

static char **seedbox_script_arguments(char **args) {
    for (unsigned depth = 0; depth <= 4; ++depth) {
        char *path = seedbox_executable(args[0], depth == 0);
        if (!path) return NULL;
        if (access(path, X_OK)) { free(path); return NULL; }
        int fd = open(path, O_RDONLY | O_CLOEXEC);
        if (fd < 0) { free(path); return NULL; }
        char header[257];
        ssize_t size;
        do { size = read(fd, header, 256); } while (size < 0 && errno == EINTR);
        int saved = errno;
        close(fd);
        if (size < 0) { free(path); errno = saved; return NULL; }
        header[size] = 0;
        if (size >= 4 && !memcmp(header, "\177ELF", 4)) { free(path); return args; }
        if (depth == 4) { free(path); errno = ELOOP; return NULL; }
        char *interpreter = "/bin/sh", *argument = NULL;
        if (size >= 2 && header[0] == '#' && header[1] == '!') {
            char *end = memchr(header, '\n', (size_t)size);
            if (!end) {
                if (size == 256) { free(path); errno = ENOEXEC; return NULL; }
                end = header + size;
            }
            while (end > header + 2 && (end[-1] == ' ' || end[-1] == '\t')) --end;
            *end = 0;
            interpreter = header + 2;
            while (*interpreter == ' ' || *interpreter == '\t') ++interpreter;
            if (!*interpreter) { free(path); errno = ENOEXEC; return NULL; }
            char *tail = interpreter;
            while (*tail && *tail != ' ' && *tail != '\t') ++tail;
            if (*tail) {
                *tail++ = 0;
                while (*tail == ' ' || *tail == '\t') ++tail;
                if (*tail) argument = tail;
            }
        } else if (depth != 0) {
            free(path); errno = ENOEXEC; return NULL;
        }
        size_t count = 0;
        while (args[count]) ++count;
        char **next = calloc(count + 3, sizeof(*next));
        if (!next) { free(path); return NULL; }
        size_t prefix = 0;
        next[prefix++] = strdup(interpreter);
        if (!next[0]) { free(path); free(next); return NULL; }
        if (argument) {
            next[prefix++] = strdup(argument);
            if (!next[1]) { free(path); free(next[0]); free(next); return NULL; }
        }
        next[prefix++] = path;
        for (size_t i = 1; i < count; ++i) next[prefix++] = args[i];
        args = next;
    }
    errno = ELOOP;
    return NULL;
}
#endif
