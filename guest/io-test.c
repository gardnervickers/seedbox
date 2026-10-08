#define _GNU_SOURCE
#define SEEDBOX_TEST_ARM_IO
#include "io.h"
#include <assert.h>
#include <string.h>
#include <sys/wait.h>

int main(int argc, char **argv) {
    if (argc == 3) {
        /* Both bootstrap and active descriptors disappear at exec. */
        assert(fcntl(atoi(argv[1]), F_GETFD) == -1 && errno == EBADF);
        assert(fcntl(atoi(argv[2]), F_GETFD) == -1 && errno == EBADF);
        return 0;
    }
    char path[] = "/tmp/seedbox-io-XXXXXX";
    int original = mkstemp(path); assert(original >= 0 && !close(original));
    assert(!seedbox_retain_io(path));
    assert(fcntl(seedbox_io_boot_fd, F_GETFD) & FD_CLOEXEC);
    /* The application dev mount makes the original pathname unavailable. */
    assert(!unlink(path));
    assert(!ioperm(0xf8, 4, 1));
    int active = seedbox_io_fd;
    assert(fcntl(active, F_GETFD) & FD_CLOEXEC);
    assert(!ioperm(0xf8, 4, 0) && seedbox_io_fd == -1);
    assert(fcntl(active, F_GETFD) == -1 && errno == EBADF);
    assert(!ioperm(0xf6, 4, 1));
    assert(write(seedbox_io_fd, "retained", 8) == 8);
    char bytes[8]; assert(pread(seedbox_io_boot_fd, bytes, sizeof(bytes), 0) == 8);
    assert(!memcmp(bytes, "retained", 8));
    char boot[24], grant[24];
    snprintf(boot, sizeof(boot), "%d", seedbox_io_boot_fd);
    snprintf(grant, sizeof(grant), "%d", seedbox_io_fd);
    pid_t child = fork(); assert(child >= 0);
    if (!child) { execl(argv[0], argv[0], boot, grant, NULL); _exit(127); }
    int status; assert(waitpid(child, &status, 0) == child);
    assert(WIFEXITED(status) && WEXITSTATUS(status) == 0);
    assert(!ioperm(0xf6, 4, 0) && !close(seedbox_io_boot_fd));
}
