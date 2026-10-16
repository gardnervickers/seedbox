/* Native Linux fixture for common preparation. Build this source separately for
 * ARM64 and x86-64; equal configuration shapes do not imply equal ELF bytes. */
#define _GNU_SOURCE
#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/un.h>
#include <unistd.h>
static int failure(const char *what) { dprintf(2, "prepared fixture: %s errno=%d\n", what, errno); return 71; }
static int private_launch_state_closed(const char *executable) {
    if (getenv("SEEDBOX_DR_EXECUTABLE_FD")) return failure("private executable environment leaked");
    uid_t real, effective, saved;
    if (getresuid(&real, &effective, &saved) || real != 1234 || effective != 1234 || saved != 1234)
        return failure("irreversible credentials");
    struct stat selected;
    if (stat(executable, &selected)) return failure("selected executable metadata");
    DIR *directory = opendir("/proc/self/fd");
    if (!directory) return failure("descriptor inventory");
    struct dirent *entry;
    while ((entry = readdir(directory))) {
        char *end;
        long number = strtol(entry->d_name, &end, 10);
        if (!*entry->d_name || *end || number < 0 || number > 0x7fffffff) continue;
        char path[64], target[4096];
        snprintf(path, sizeof(path), "/proc/self/fd/%ld", number);
        if (readlink(path, target, sizeof(target)) < 0) continue;
        struct stat opened;
        if (stat(path, &opened) || opened.st_dev != selected.st_dev || opened.st_ino != selected.st_ino) continue;
        int flags = fcntl((int)number, F_GETFL);
        char byte;
        ssize_t count = read((int)number, &byte, 1);
        if ((flags >= 0 && !(flags & O_PATH) && (flags & O_ACCMODE) != O_WRONLY) || count >= 0) {
            closedir(directory);
            return failure("readable executable descriptor leaked");
        }
    }
    closedir(directory);
    return 0;
}
int main(int argc, char **argv) {
    if (argc > 1 && !strcmp(argv[1], "--wrapper")) {
        if (argc < 5 || strcmp(argv[2], "--")) return failure("wrapper argv");
        int fd = open("/seedbox/progress/lib/runtime-data", O_RDONLY);
        char data[8];
        if (fd < 0 || read(fd, data, sizeof(data)) != 7 || memcmp(data, "runtime", 7)) return failure("runtime tree");
        close(fd); puts("WRAPPER"); fflush(stdout);
        execvp(argv[3], argv + 3); return failure("wrapper exec");
    }
    int script = strstr(argv[0], "interpreter") != NULL;
    const char *first, *empty;
    if (script) {
        if (argc != 5 || strcmp(argv[1], "--script-option") || strcmp(argv[2], "/opt/bin/script")) return failure("script argv");
        int fd = open(argv[2], O_RDONLY); char source[128];
        if (fd < 0) return failure("script file");
        ssize_t length = read(fd, source, sizeof(source) - 1); close(fd);
        if (length < 0) return failure("script read");
        source[length] = 0;
        if (!strstr(source, "fixture-script\n")) return failure("script contents");
        first = argv[3]; empty = argv[4];
    } else {
        if (argc != 3) return failure("program argv");
        first = argv[1]; empty = argv[2];
    }
    if (strcmp(first, "one two") || *empty) return failure("argument boundaries");
    if (getuid() != 1234 || getgid() != 2345 || getgroups(0, NULL) != 0) return failure("credentials");
    int launch_state = private_launch_state_closed(script ? "/usr/bin/interpreter" : "/opt/bin/program");
    if (launch_state) return launch_state;
    char cwd[256]; if (!getcwd(cwd, sizeof(cwd)) || strcmp(cwd, "/work")) return failure("cwd");
    const char *value = getenv("VALUE"); if (!value || strcmp(value, "a=b c")) return failure("environment");
    struct stat file, hard, link;
    if (stat("/data/file", &file) || stat("/data/hard", &hard) || lstat("/data/link", &link)) return failure("metadata");
    if (file.st_uid != 1234 || file.st_gid != 2345 || (file.st_mode & 07777) != 0640 ||
        file.st_ino != hard.st_ino || file.st_dev != hard.st_dev || !S_ISLNK(link.st_mode)) return failure("links/mode/owner");
    char target[16]; ssize_t target_length = readlink("/data/link", target, sizeof(target));
    if (target_length != 4 || memcmp(target, "file", 4)) return failure("symlink bytes");
    char bytes[64]; int fd = open("/data/link", O_RDONLY);
    if (fd < 0) return failure("image file");
    ssize_t length = read(fd, bytes, sizeof(bytes)); close(fd);
    const char payload[] = "retained payload\n";
    if (length != (ssize_t)strlen(payload) || memcmp(bytes, payload, strlen(payload))) return failure("image contents");
    int control = socket(AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC, 0);
    struct sockaddr_un address = {.sun_family = AF_UNIX};
    strcpy(address.sun_path, "/run/seedbox/control.sock");
    if (control < 0 || connect(control, (struct sockaddr *)&address, sizeof(address))) return failure("control socket in image root");
    close(control);
    printf("MODE=%s\nARGV0=%s\nUID=1234 GID=2345\nCWD=%s\nVALUE=%s\nFILE=retained payload\nLINKS=ok\nCONTROL=ok\n", script ? "SCRIPT" : "ELF", argv[0], cwd, value);
    return 0;
}
