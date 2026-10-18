/* Portable real-child test of retained recipe loading and execution. Native
 * guest tests separately establish chroot and Linux UID/GID behavior. */
#define _GNU_SOURCE
#include "guest_application.h"
#include "guest_supervisor.h"
#include <errno.h>
#include <stdio.h>
#include <unistd.h>
#include <string.h>
#include <stdlib.h>
static int launch(void *opaque, int error_fd) {
    (void)error_fd;
    return sbx_application_exec(opaque);
}
static int output(void *opaque, unsigned stream, const unsigned char *bytes, size_t length) {
    (void)opaque;
    int fd = stream ? STDERR_FILENO : STDOUT_FILENO;
    while (length) {
        ssize_t count = write(fd, bytes, length);
        if (count < 0 && errno == EINTR) continue;
        if (count <= 0) return -1;
        bytes += count; length -= (size_t)count;
    }
    return 0;
}
int main(int argc, char **argv) {
    if (argc > 1 && !strcmp(argv[1], "--wrapper")) {
        if (argc != 7 || strcmp(argv[2], "--") || strcmp(argv[3], "/bin/sh") ||
            strcmp(argv[5], "one two") || argv[6][0]) return 6;
        execvp(argv[3], argv + 3); return 7;
    }
    if (argc != 2 && argc != 3) return 2;
    struct sbx_application application;
    if (sbx_application_load(&application, argv[1])) return 3;
    if (argc == 3 && !strcmp(argv[2], "--inspect")) {
        for (char **item = application.arguments; *item; ++item) printf("arg:%zu:%s\n", strlen(*item), *item);
        for (char **item = application.execution_prefix; *item; ++item) printf("prefix:%zu:%s\n", strlen(*item), *item);
        for (char **item = application.environment; *item; ++item) printf("env:%zu:%s\n", strlen(*item), *item);
        printf("cwd:%s\nuser:%s\n", application.working_directory, application.user);
        sbx_application_destroy(&application); return 0;
    }
    struct sbx_supervisor supervisor;
    sbx_supervisor_init(&supervisor);
    for (unsigned start = 0; start < 2; ++start) {
        if (sbx_supervisor_start(&supervisor, launch, &application)) return 4;
        int code, status;
        do {
            status = sbx_supervisor_collect(&supervisor, &code, output, NULL);
            if (!status) usleep(1000);
        } while (!status);
        if (status < 0 || code) return 5;
    }
    sbx_application_destroy(&application);
    return 0;
}
