/* Generic per-machine OCI application supervisor. Compile statically for amd64.
 * No shell parses image arguments, environment or configuration values.
 * Filesystems are fresh RAM-backed initramfs images for each machine boot;
 * process stop/start preserves their writable contents within that run. */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <grp.h>
#include <poll.h>
#include <pwd.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/io.h>
#include <sys/mount.h>
#include <sys/stat.h>
#include <sys/wait.h>
#include <time.h>
#include <termios.h>
#include <unistd.h>

static int recipe_fd, management = -1;
static int launch_error_fd = -1;
static char **arguments, **environment;
static char *user, *working_dir;

static void finish(int status, int error) {
    fflush(NULL);
    if (ioperm(0xf4, 2, 1)) { perror("seedbox ioperm"); for (;;) pause(); }
    outb(status, error ? 0xf5 : 0xf4);
    for (;;) pause();
}
static void die(const char *what) {
    fprintf(stderr, "SEEDBOX_INIT_ERROR %s: %s\n", what, strerror(errno));
    if (getpid() != 1) {
        if (launch_error_fd >= 0) { char error = 1; (void)write(launch_error_fd, &error, 1); }
        _exit(125);
    }
    finish(1, 1);
}
static void read_exact(void *buffer, size_t size) {
    char *p = buffer;
    while (size) {
        ssize_t n = read(recipe_fd, p, size);
        if (n < 0 && errno == EINTR) continue;
        if (n <= 0) { errno = EINVAL; die("truncated launch recipe"); }
        p += n; size -= n;
    }
}
static uint32_t count(void) {
    unsigned char b[4]; read_exact(b, 4);
    return (uint32_t)b[0] | (uint32_t)b[1]<<8 | (uint32_t)b[2]<<16 | (uint32_t)b[3]<<24;
}
static char *field(uint32_t *size) {
    *size = count();
    if (*size > 16*1024*1024) { errno = E2BIG; die("launch field"); }
    char *s = malloc((size_t)*size + 1);
    if (!s) die("allocate launch field");
    read_exact(s, *size); s[*size] = 0;
    return s;
}
static char *text(void) {
    uint32_t size; char *s = field(&size);
    if (memchr(s, 0, size)) { errno = EINVAL; die("NUL in launch text"); }
    return s;
}
static char **list(void) {
    uint32_t n = count();
    if (n > 65536) { errno = E2BIG; die("launch list"); }
    char **list = calloc((size_t)n + 1, sizeof(char *));
    if (!list) die("allocate launch list");
    for (uint32_t i=0; i<n; i++) list[i] = text();
    return list;
}
static void directories(const char *path, mode_t mode) {
    char *copy = strdup(path);
    if (!copy) die("directory path");
    for (char *p = copy + 1; ; p++) {
        if (*p != '/' && *p) continue;
        char save = *p; *p = 0;
        if (*copy && mkdir(copy, save ? 0755 : mode) && errno != EEXIST) die(copy);
        *p = save;
        if (!save) break;
    }
    free(copy);
}
static void write_file(const char *path, const void *data, size_t size, mode_t mode) {
    char *parent = strdup(path), *slash;
    if (!parent) die("config parent");
    slash = strrchr(parent, '/');
    if (slash && slash != parent) { *slash = 0; directories(parent, 0755); }
    free(parent);
    int fd = open(path, O_WRONLY|O_CREAT|O_TRUNC, mode);
    if (fd < 0) die(path);
    const char *p = data;
    while (size) {
        ssize_t n = write(fd, p, size);
        if (n < 0 && errno == EINTR) continue;
        if (n <= 0) die("write config");
        p += n; size -= n;
    }
    if (fchmod(fd, mode) || fchown(fd, 0, 0) || close(fd)) die("finish config");
}
static void busybox(char *const args[]) {
    pid_t pid = fork();
    if (pid < 0) die("fork network setup");
    if (!pid) { execv("/seedbox/busybox", args); _exit(127); }
    int status;
    while (waitpid(pid, &status, 0) < 0) { if (errno != EINTR) die("wait network setup"); }
    if (!WIFEXITED(status) || WEXITSTATUS(status)) { errno = EIO; die("network setup failed"); }
}
static void credentials(void) {
    if (!*user) return;
    char *copy = strdup(user), *group;
    if (!copy) die("user");
    group = strchr(copy, ':'); if (group) *group++ = 0;
    struct passwd *pw = getpwnam(copy);
    char *end = NULL; unsigned long number = strtoul(copy, &end, 10);
    uid_t uid; gid_t gid;
    if (pw) { uid = pw->pw_uid; gid = pw->pw_gid; }
    else if (*copy && end && !*end && number <= UINT32_MAX) { uid = number; pw = getpwuid(uid); gid = pw ? pw->pw_gid : 0; }
    else { errno = EINVAL; die("unknown image user"); }
    if (group) {
        struct group *gr = getgrnam(group);
        number = strtoul(group, &end, 10);
        if (gr) gid = gr->gr_gid;
        else if (*group && !*end && number <= UINT32_MAX) gid = number;
        else { errno = EINVAL; die("unknown image group"); }
    }
    if (pw && !group) { if (initgroups(pw->pw_name, gid)) die("supplementary groups"); }
    else if (setgroups(0, NULL)) die("clear groups");
    if (setgid(gid) || setuid(uid)) die("set image user");
    free(copy);
}
static pid_t start(void) {
    int launch_error[2];
    if (pipe2(launch_error, O_CLOEXEC)) die("launch error pipe");
    pid_t pid = fork();
    if (pid < 0) die("fork application");
    if (!pid) {
        close(launch_error[0]); launch_error_fd = launch_error[1];
        if (setpgid(0, 0)) die("application process group");
        if (management >= 0) close(management);
        int input = open("/dev/null", O_RDONLY);
        if (input < 0 || dup2(input, STDIN_FILENO) < 0) die("application stdin");
        if (input > 2) close(input);
        if (chdir(working_dir)) die("working directory");
        credentials();
        environ = environment;
        execvp(arguments[0], arguments);
        die("exec application");
    }
    close(launch_error[1]);
    char error; ssize_t received;
    do { received = read(launch_error[0], &error, 1); } while (received < 0 && errno == EINTR);
    close(launch_error[0]);
    if (received != 0) { errno = EIO; die("application launch failed"); }
    if (setpgid(pid, pid) && errno != EACCES && errno != ESRCH) die("parent process group");
    return pid;
}
static void acknowledge(const char *action) {
    struct timespec t;
    if (clock_gettime(CLOCK_MONOTONIC, &t)) die("fault clock");
    unsigned long long ns = (unsigned long long)t.tv_sec*1000000000ULL + t.tv_nsec;
    dprintf(management, "SEEDBOX_EFFECT %s guest_ns=%llu\n", action, ns);
}
int main(void) {
    directories("/dev", 0755);
    if (mount("devtmpfs", "/dev", "devtmpfs", 0, NULL)) die("mount bootstrap dev");
    int console = open("/dev/console", O_RDWR);
    if (console >= 0) { dup2(console, 0); dup2(console, 1); dup2(console, 2); if (console > 2) close(console); }
    management = open("/dev/ttyS1", O_RDWR|O_NONBLOCK|O_CLOEXEC);
    if (management < 0) die("management serial port");
    struct termios serial;
    if (tcgetattr(management, &serial)) die("management terminal");
    cfmakeraw(&serial);
    if (tcsetattr(management, TCSANOW, &serial)) die("raw management terminal");
    recipe_fd = open("/seedbox/launch.bin", O_RDONLY|O_CLOEXEC);
    if (recipe_fd < 0) die("launch recipe");
    char magic[10]; read_exact(magic, sizeof(magic));
    if (memcmp(magic, "SBXLAUNCH1", 10)) { errno = EINVAL; die("launch version"); }
    char *ip = text(), *hosts = text(); user = text(); working_dir = text();
    arguments = list(); environment = list(); char **volumes = list();
    if (!arguments[0] || !*arguments[0]) { errno = EINVAL; die("empty executable"); }
    busybox((char *const[]){"busybox", "ip", "link", "set", "lo", "up", NULL});
    busybox((char *const[]){"busybox", "ip", "addr", "add", ip, "dev", "eth0", NULL});
    busybox((char *const[]){"busybox", "ip", "link", "set", "eth0", "up", NULL});
    if (chdir("/rootfs") || chroot(".") || chdir("/")) die("enter image root");
    directories("/dev", 0755); directories("/proc", 0755); directories("/sys", 0755); directories("/tmp", 01777);
    if (mount("devtmpfs", "/dev", "devtmpfs", 0, NULL) || mount("proc", "/proc", "proc", 0, NULL) || mount("sysfs", "/sys", "sysfs", 0, NULL)) die("mount guest filesystems");
    write_file("/etc/hosts", hosts, strlen(hosts), 0644);
    for (char **volume = volumes; *volume; volume++) directories(*volume, 0755);
    directories(working_dir, 0755);
    uint32_t configs = count();
    if (configs > 65536) { errno = E2BIG; die("config count"); }
    for (uint32_t i=0; i<configs; i++) {
        char *path = text(); uint32_t size; char *content = field(&size);
        write_file(path, content, size, 0444);
        if (mount(path, path, NULL, MS_BIND, NULL) || mount(NULL, path, NULL, MS_BIND|MS_REMOUNT|MS_RDONLY, NULL)) die("read-only config mount");
        free(path); free(content);
    }
    close(recipe_fd);
    pid_t child = start();
    char command[64]; size_t used = 0;
    for (;;) {
        int status;
        pid_t done = waitpid(-1, &status, WNOHANG);
        if (done == child && child > 0) finish(WIFEXITED(status) ? WEXITSTATUS(status) : 128 + WTERMSIG(status), 0);
        struct pollfd pfd = { .fd = management, .events = POLLIN };
        if (poll(&pfd, 1, 1) < 0 && errno != EINTR) die("poll management");
        char byte;
        while (read(management, &byte, 1) == 1) {
            if (byte == '\n') {
                command[used] = 0; used = 0;
                if (!strcmp(command, "stop") && child > 0) {
                    if (kill(-child, SIGKILL) && errno != ESRCH) die("stop application");
                    while (waitpid(child, &status, 0) < 0) { if (errno != EINTR) die("wait stopped application"); }
                    child = -1; acknowledge("stop");
                } else if (!strcmp(command, "start") && child < 0) { child = start(); acknowledge("start"); }
                else { errno = EINVAL; die("invalid management action/state"); }
            } else if (used + 1 < sizeof(command)) command[used++] = byte;
            else { errno = E2BIG; die("management command"); }
        }
    }
}
