/* Launch barrier: the point where a booted machine waits for its world's seed.
 * With seedbox.barrier=1 on the kernel command line, the guest announces the
 * barrier with an OUTL to SEEDBOX_BARRIER_PORT just before it starts its
 * application. The VMM parks the machine there until every machine of the
 * world has arrived, which is the world's fork point. On release the guest reads
 * eight 32-bit words of per-seed entropy from the same port and reseeds the
 * kernel CRNG with them, so everything drawn from it after the barrier
 * (application keys, AT_RANDOM, ASLR) differs per seed even when every seed
 * forks from one booted image. Without the flag this is a no-op. */
#ifndef SEEDBOX_BARRIER_H
#define SEEDBOX_BARRIER_H
#include <errno.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/io.h>
#include <sys/ioctl.h>
#include <unistd.h>

#ifndef RNDADDENTROPY
#define RNDADDENTROPY _IOW('R', 0x03, int[2])
#endif
#ifndef RNDRESEEDCRNG
#define RNDRESEEDCRNG _IO('R', 0x07)
#endif

enum { SEEDBOX_BARRIER_PORT = 0xf8, SEEDBOX_BARRIER_WORDS = 8 };

static inline int seedbox_barrier_enabled(const char *cmdline_path) {
    FILE *cmdline = fopen(cmdline_path, "r");
    if (!cmdline) return -1;
    char *line = NULL; size_t capacity = 0;
    ssize_t size = getline(&line, &capacity, cmdline);
    fclose(cmdline);
    if (size < 0) { free(line); errno = EIO; return -1; }
    int enabled = 0;
    char *save;
    for (char *token = strtok_r(line, " \t\r\n", &save); token; token = strtok_r(NULL, " \t\r\n", &save)) {
        if (!strcmp(token, "seedbox.barrier=1")) enabled = 1;
    }
    free(line);
    return enabled;
}

/* Returns 0 after the barrier (or when it is disabled), -1 with errno set. */
static inline int seedbox_launch_barrier(const char *cmdline_path, const char *urandom_path) {
    int enabled = seedbox_barrier_enabled(cmdline_path);
    if (enabled <= 0) return enabled;
    if (ioperm(SEEDBOX_BARRIER_PORT, sizeof(uint32_t), 1)) return -1;
    outl(1, SEEDBOX_BARRIER_PORT);
    struct { int entropy_count; int buf_size; uint32_t buf[SEEDBOX_BARRIER_WORDS]; } pool = {
        .entropy_count = 8 * (int)sizeof(pool.buf), .buf_size = (int)sizeof(pool.buf)
    };
    for (int i = 0; i < SEEDBOX_BARRIER_WORDS; i++) pool.buf[i] = inl(SEEDBOX_BARRIER_PORT);
    int fd = open(urandom_path, O_WRONLY|O_CLOEXEC);
    if (fd < 0) return -1;
    if (ioctl(fd, RNDADDENTROPY, &pool) || ioctl(fd, RNDRESEEDCRNG)) { int e = errno; close(fd); errno = e; return -1; }
    return close(fd);
}
#endif
