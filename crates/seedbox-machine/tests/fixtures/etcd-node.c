/* Mount a machine's disk, then run its program: the etcd acceptance wrapper.
 *
 *   etcd-node DEVICE DIRECTORY PROGRAM [ARGUMENT...]
 *
 * Mounts DEVICE (ext4, default options) on DIRECTORY, prints
 * "ETCDNODE mounted DIRECTORY", then executes PROGRAM with PROGRAM and the
 * arguments as its argv. Any failure prints "ETCDNODE fail ..." with errno
 * and exits nonzero; nothing is retried. */
#define _GNU_SOURCE
#include <errno.h>
#include <stdio.h>
#include <string.h>
#include <sys/mount.h>
#include <sys/stat.h>
#include <unistd.h>

static int fail(const char *what) {
    printf("ETCDNODE fail %s errno=%d (%s)\n", what, errno, strerror(errno));
    fflush(stdout);
    return 1;
}

int main(int argc, char **argv) {
    if (argc < 4) {
        errno = EINVAL;
        return fail("usage");
    }
    if (mkdir(argv[2], 0700) && errno != EEXIST) return fail("mkdir");
    if (mount(argv[1], argv[2], "ext4", 0, NULL)) return fail("mount");
    printf("ETCDNODE mounted %s\n", argv[2]);
    fflush(stdout);
    execv(argv[3], &argv[3]);
    return fail("exec");
}
