/* Architectural supervisor I/O. ARM uses a dedicated PID-1-only guest device;
 * x86 retains its existing port ABI. There is no userspace physical mapping. */
#ifndef SEEDBOX_GUEST_IO_H
#define SEEDBOX_GUEST_IO_H
#if defined(__aarch64__) || defined(SEEDBOX_TEST_ARM_IO)
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/ioctl.h>
#include <unistd.h>
#include "../kernel-arm64/supervisor-io.h"
static int seedbox_io_fd = -1;
static int seedbox_io_boot_fd = -1;
/* Keep a private, close-on-exec handle before hiding bootstrap devtmpfs.
 * Active grants still close at the barrier; a later grant duplicates this
 * handle without exposing a supervisor device node in the application root. */
static inline int seedbox_retain_io(const char *path) {
    seedbox_io_boot_fd = open(path, O_RDWR | O_CLOEXEC);
    return seedbox_io_boot_fd < 0 ? -1 : 0;
}
static inline int ioperm(unsigned long port, unsigned long count, int enable) {
    if (port < 0xf4 || port > 0xf8 || !count || count > 5) { errno = EINVAL; return -1; }
    if (!enable) {
        int fd = seedbox_io_fd; seedbox_io_fd = -1;
        return fd < 0 ? 0 : close(fd);
    }
    if (seedbox_io_fd < 0) seedbox_io_fd = seedbox_io_boot_fd < 0
        ? open("/dev/seedbox-io", O_RDWR | O_CLOEXEC)
        : fcntl(seedbox_io_boot_fd, F_DUPFD_CLOEXEC, 3);
    return seedbox_io_fd < 0 ? -1 : 0;
}
static inline unsigned int seedbox_io_transfer(unsigned short port, unsigned char width,
                                              unsigned char write, unsigned int value) {
    struct seedbox_io request = {.port = port, .width = width, .write = write, .value = value};
    int status;
    do { status = ioctl(seedbox_io_fd, SEEDBOX_IO_TRANSFER, &request); } while (status < 0 && errno == EINTR);
    if (status < 0) { perror("seedbox supervisor I/O"); abort(); }
    return request.value;
}
static inline void outb(unsigned char value, unsigned short port) { (void)seedbox_io_transfer(port, 1, 1, value); }
static inline void outl(unsigned int value, unsigned short port) { (void)seedbox_io_transfer(port, 4, 1, value); }
static inline unsigned int inl(unsigned short port) { return seedbox_io_transfer(port, 4, 0, 0); }
#else
#include <sys/io.h>
#endif
#endif
