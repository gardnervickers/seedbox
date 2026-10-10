/* Application output is framed by the supervisor, never parsed as management
 * commands. Each frame carries at most 2048 arbitrary bytes as lowercase hex.
 * Capture is unconditional: host subscribers cannot change guest execution. */
#ifndef SEEDBOX_APPLICATION_OUTPUT_H
#define SEEDBOX_APPLICATION_OUTPUT_H
#include <errno.h>
#include <fcntl.h>
#include <stddef.h>
#include <sys/ioctl.h>
#include <sys/stat.h>
#include <unistd.h>

enum { APPLICATION_OUTPUT_BYTES = 2048 };
struct application_output { int read_fd, write_fd; };

static void application_output_close(struct application_output *output) {
    if (output->read_fd >= 0) close(output->read_fd);
    if (output->write_fd >= 0) close(output->write_fd);
    output->read_fd = output->write_fd = -1;
}

static int application_output_open(struct application_output *output) {
    int fds[2];
    if (pipe(fds)) return -1;
    output->read_fd = fds[0]; output->write_fd = fds[1];
    if (fcntl(fds[0], F_SETFD, FD_CLOEXEC) || fcntl(fds[1], F_SETFD, FD_CLOEXEC)
        || fcntl(fds[0], F_SETFL, O_NONBLOCK)
#ifdef __linux__
        /* Linux rechecks the pipe inode mode when a non-root application opens
         * /dev/stdout through /proc/self/fd. Darwin has no procfs pipe reopen. */
        || fchmod(fds[1], 0666)
#endif
    ) {
        int saved = errno;
        application_output_close(output); errno = saved; return -1;
    }
    return 0;
}

/* Returns zero for an empty/closed pipe, a byte count for a frame, or -1.
 * Only the reader is nonblocking: application writes retain pipe semantics. */
static ssize_t application_output_frame(struct application_output *output,
                                       unsigned stream, size_t limit,
                                       char frame[2 * APPLICATION_OUTPUT_BYTES + 16]) {
    unsigned char bytes[APPLICATION_OUTPUT_BYTES];
    if (output->read_fd < 0 || !limit) return 0;
    if (limit > sizeof(bytes)) limit = sizeof(bytes);
    ssize_t count;
    do { count = read(output->read_fd, bytes, limit); } while (count < 0 && errno == EINTR);
    if (count < 0 && (errno == EAGAIN || errno == EWOULDBLOCK)) return 0;
    if (count <= 0) {
        if (!count) { close(output->read_fd); output->read_fd = -1; }
        return count;
    }
    static const char prefix[] = "SBX_LOG1 ";
    static const char hex[] = "0123456789abcdef";
    size_t used = 0;
    for (size_t i = 0; i < sizeof(prefix) - 1; ++i) frame[used++] = prefix[i];
    frame[used++] = stream ? '2' : '1'; frame[used++] = ' ';
    for (ssize_t i = 0; i < count; ++i) {
        frame[used++] = hex[bytes[i] >> 4]; frame[used++] = hex[bytes[i] & 15];
    }
    frame[used++] = '\n';
    return (ssize_t)used;
}
#endif
