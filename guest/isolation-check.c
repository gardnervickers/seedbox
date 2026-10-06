/* Live OCI guest fixture: root/device authority and independent disk capacity. */
#define _GNU_SOURCE
#include <arpa/inet.h>
#include <assert.h>
#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <linux/capability.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mount.h>
#include <sys/prctl.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/statvfs.h>
#include <sys/syscall.h>
#include <sys/sysmacros.h>
#include <unistd.h>

static void isolation(void) {
    assert(mkdir("/data", 0755) == 0);
    assert(access("/seedbox/launch.bin", F_OK) == -1 && errno == ENOENT);
    assert(access("/images", F_OK) == -1 && errno == ENOENT);
    struct stat input, null;
    assert(fstat(STDIN_FILENO, &input) == 0 && stat("/dev/null", &null) == 0);
    assert((input.st_dev == null.st_dev && input.st_ino == null.st_ino) && "stdin must use private /dev");
    DIR *devices = opendir("/dev");
    assert(devices);
    struct dirent *entry;
    while ((entry = readdir(devices))) assert(strncmp(entry->d_name, "vport", 5));
    assert(closedir(devices) == 0);
    assert(mknod("/data/forged", S_IFCHR|0600, makedev(1, 3)) == -1 && errno == EPERM);
    assert(mount("devtmpfs", "/dev", "devtmpfs", 0, NULL) == -1 && errno == EPERM);
    assert(open("/proc/1/mem", O_RDONLY) == -1 && errno == EACCES);
    struct __user_cap_header_struct header = {.version = _LINUX_CAPABILITY_VERSION_3};
    struct __user_cap_data_struct data[2];
    assert(syscall(SYS_capget, &header, data) == 0);
    const int denied[] = {CAP_SYS_ADMIN, CAP_MKNOD, CAP_SYS_RAWIO, CAP_SYS_BOOT, CAP_SYS_MODULE, CAP_SYS_TIME, CAP_SYS_PTRACE};
    for (size_t i = 0; i < sizeof(denied)/sizeof(denied[0]); i++) {
        unsigned int word = (unsigned int)denied[i] / 32, bit = (unsigned int)denied[i] % 32;
        assert(!((data[word].effective|data[word].permitted|data[word].inheritable) & (1U << bit)));
        assert(prctl(PR_CAPBSET_READ, denied[i], 0, 0, 0) == 0);
    }
    assert(prctl(PR_GET_NO_NEW_PRIVS, 0, 0, 0, 0) == 1);
    /* A root process can leave a nested chroot, but only back into its own
     * detached root. There is no ancestor containing the supervisor/images. */
    int root = open("/", O_RDONLY|O_DIRECTORY);
    assert(root >= 0 && mkdir("/data/jail", 0755) == 0);
    assert(chroot("/data/jail") == 0 && fchdir(root) == 0);
    for (int ancestor = 0; ancestor < 256; ancestor++) assert(chdir("..") == 0);
    assert(chroot(".") == 0 && chdir("/") == 0);
    close(root);
    assert(access("/seedbox/launch.bin", F_OK) == -1 && errno == ENOENT);
    assert((access("/images", F_OK) == -1 && errno == ENOENT) && "detached root has no bootstrap ancestors");
    struct statvfs disk;
    assert(statvfs("/data", &disk) == 0 && disk.f_blocks * disk.f_frsize == 8 * 1024 * 1024);
}

int main(int argc, char **argv) {
    assert(argc == 2);
    isolation();
    struct sockaddr_in address = {.sin_family = AF_INET, .sin_port = htons(7000)};
    if (!strcmp(argv[1], "fill")) {
        char *block = calloc(1, 1024 * 1024);
        assert(block);
        int disk = open("/data/full", O_WRONLY|O_CREAT, 0600);
        assert(disk >= 0);
        for (;;) {
            ssize_t written = write(disk, block, 1024 * 1024);
            if (written == -1) { assert(errno == ENOSPC); break; }
            assert(written > 0);
        }
        close(disk); free(block);
        puts("DISK_FULL ENOSPC"); fflush(stdout);
        int listener = socket(AF_INET, SOCK_STREAM, 0), reuse = 1;
        assert(listener >= 0 && setsockopt(listener, SOL_SOCKET, SO_REUSEADDR, &reuse, sizeof(reuse)) == 0);
        assert(bind(listener, (struct sockaddr *)&address, sizeof(address)) == 0 && listen(listener, 1) == 0);
        for (;;) {
            int peer = accept(listener, NULL, NULL);
            assert(peer >= 0 && write(peer, "FULL\n", 5) == 5);
            close(peer);
        }
    }
    assert(!strcmp(argv[1], "check") && inet_pton(AF_INET, "10.0.0.2", &address.sin_addr) == 1);
    int peer = -1;
    for (int attempt = 0; attempt < 1000; attempt++) {
        peer = socket(AF_INET, SOCK_STREAM, 0);
        assert(peer >= 0);
        if (connect(peer, (struct sockaddr *)&address, sizeof(address)) == 0) break;
        close(peer); peer = -1; usleep(10000);
    }
    char reply[5]; size_t received = 0;
    assert(peer >= 0);
    while (received < sizeof(reply)) {
        ssize_t count = read(peer, reply + received, sizeof(reply) - received);
        assert(count > 0); received += (size_t)count;
    }
    assert(!memcmp(reply, "FULL\n", 5)); close(peer);
    char *block = calloc(1, 1024 * 1024);
    int disk = open("/data/own", O_WRONLY|O_CREAT, 0600);
    assert(block && disk >= 0 && write(disk, block, 1024 * 1024) == 1024 * 1024);
    close(disk); free(block);
    puts("RESULT pass root=detached devices=private capabilities=restricted disk=independent");
    return 0;
}
