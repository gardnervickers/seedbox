/* Generic per-machine OCI application supervisor. Compile statically for amd64.
 * No shell parses image arguments, environment or configuration values.
 * Filesystems are fresh RAM-backed initramfs images for each machine boot;
 * process stop/start preserves their writable contents within that run. */
#define _GNU_SOURCE
#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <grp.h>
#include <limits.h>
#include <poll.h>
#include <pwd.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#ifndef SEEDBOX_SUPERVISOR_TEST
#include <sys/io.h>
#endif
#include "control.h"
#include "barrier.h"
#include <sys/mount.h>
#include <sys/stat.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

static int recipe_fd, management = -1;
#define RPC_FRAME_MAX 16384
static int control_listener = -1, control_client = -1;
static uint32_t relay_id, waiting_relay, session_id;
static char request_frame[RPC_FRAME_MAX];
static size_t request_used;
static void die(const char *what);
static void directories(const char *path, mode_t mode);
static void send_all(int fd, const char *bytes, size_t size) {
    while (size) {
        ssize_t n = write(fd, bytes, size);
        if (n < 0 && errno == EINTR) continue;
        if (n < 0 && (errno == EAGAIN || errno == EWOULDBLOCK)) {
            struct pollfd ready = {.fd=fd,.events=POLLOUT};
            int status = poll(&ready, 1, -1);
            if (status < 0 && errno != EINTR) die("poll control relay");
            if (status > 0 && (ready.revents & (POLLHUP|POLLERR|POLLNVAL))) { errno = EPIPE; die("control relay disconnected"); }
            continue;
        }
        if (n<=0) die("write control relay");
        bytes+=n; size-=n;
    }
}
static int controller_enabled(const char *cmdline_path) {
    FILE *cmdline = fopen(cmdline_path, "r");
    if (!cmdline) die("controller kernel command line");
    char *line = NULL; size_t capacity = 0;
    ssize_t size = getline(&line, &capacity, cmdline);
    int error = ferror(cmdline) ? errno : EINVAL;
    fclose(cmdline);
    if (size < 0) { free(line); errno = error; die("read controller kernel command line"); }
    int enabled = 0;
    char *save;
    for (char *token = strtok_r(line, " \t\r\n", &save); token; token = strtok_r(NULL, " \t\r\n", &save)) {
        if (!strcmp(token, "seedbox.control=1")) enabled = 1;
    }
    free(line);
    return enabled;
}
static void relay_socket(const char *cmdline_path, const char *socket_path) {
    /* The kernel command line is supplied by the VMM, outside the image's
     * launch environment. Only the selected controller receives this socket. */
    if (!controller_enabled(cmdline_path)) return;
    struct sockaddr_un address = {.sun_family = AF_UNIX};
    if (strlen(socket_path) >= sizeof(address.sun_path)) { errno = ENAMETOOLONG; die("control socket path"); }
    char *parent = strdup(socket_path), *slash;
    if (!parent) die("control socket parent");
    slash = strrchr(parent, '/');
    if (slash && slash != parent) { *slash = 0; directories(parent, 0755); }
    free(parent);
    control_listener = socket(AF_UNIX, SOCK_STREAM|SOCK_NONBLOCK|SOCK_CLOEXEC, 0);
    if (control_listener < 0) die("control socket");
    strcpy(address.sun_path, socket_path);
    if (bind(control_listener, (struct sockaddr *)&address, sizeof(address)) || chmod(socket_path, 0666) || listen(control_listener, 1)) die("control socket bind");
}
static void relay_accept(void) {
    if (control_client >= 0) return;
    int client = accept4(control_listener, NULL, NULL, SOCK_NONBLOCK|SOCK_CLOEXEC);
    if (client < 0) {
        if (errno != EAGAIN && errno != EWOULDBLOCK && errno != EINTR) die("accept control socket");
        return;
    }
    if (session_id == UINT32_MAX) { close(client); errno = EOVERFLOW; die("control session IDs exhausted"); }
    session_id++;
    control_client = client;
}
static void relay_requests(void) {
    if (control_client < 0) return;
    char bytes[4096];
    for (;;) {
        ssize_t received = read(control_client, bytes, sizeof(bytes));
        if (received < 0 && errno == EINTR) continue;
        if (received < 0 && (errno == EAGAIN || errno == EWOULDBLOCK)) return;
        if (received < 0) die("read control socket");
        if (!received) {
            if (waiting_relay || request_used) { errno = EPIPE; die("controller disconnected during RPC"); }
            close(control_client); control_client = -1;
            return;
        }
        for (ssize_t i = 0; i < received; i++) {
            char byte = bytes[i];
            if (waiting_relay) { errno = EBUSY; die("more than one outstanding RPC"); }
            if (byte == '\n') {
                if (!request_used || relay_id == UINT32_MAX) { errno = EINVAL; die("control RPC frame/ID"); }
                waiting_relay = ++relay_id;
                char frame[RPC_FRAME_MAX+64];
                int prefix = snprintf(frame, sizeof(frame), "SBX_RPC1 %u %u ", waiting_relay, session_id);
                if (prefix < 0 || (size_t)prefix + request_used + 1 > sizeof(frame)) { errno = E2BIG; die("control relay frame"); }
                memcpy(frame + prefix, request_frame, request_used);
                frame[prefix + request_used] = '\n';
                send_all(management, frame, (size_t)prefix + request_used + 1);
                request_used = 0;
            } else {
                if (!byte || request_used + 1 >= sizeof(request_frame)) { errno = E2BIG; die("control RPC frame size"); }
                request_frame[request_used++] = byte;
            }
        }
    }
}

/* Linux exposes named non-console virtio ports in sysfs. devtmpfs supplies the
 * corresponding /dev/vportNpM nodes; no udev-created symlink is required. */
static int find_management(const char *sysfs, const char *devices) {
    DIR *directory = opendir(sysfs);
    if (!directory) {
        if (errno == ENOENT) return -1;
        die("virtio management class");
    }
    struct dirent *entry;
    int channel = -1;
    while ((entry = readdir(directory))) {
        if (strncmp(entry->d_name, "vport", 5)) continue;
        char path[PATH_MAX], name[64];
        int length = snprintf(path, sizeof(path), "%s/%s/name", sysfs, entry->d_name);
        if (length < 0 || (size_t)length >= sizeof(path)) { errno = ENAMETOOLONG; die("virtio port name path"); }
        int attribute = open(path, O_RDONLY|O_CLOEXEC);
        if (attribute < 0) {
            if (errno == ENOENT) continue; /* Port naming can follow creation. */
            die("virtio port name");
        }
        ssize_t size;
        do { size = read(attribute, name, sizeof(name) - 1); } while (size < 0 && errno == EINTR);
        int error = errno;
        close(attribute);
        if (size < 0) { errno = error; die("read virtio port name"); }
        name[size] = 0;
        if (size > 0 && name[size - 1] == '\n') name[size - 1] = 0;
        if (strcmp(name, "seedbox")) continue;
        length = snprintf(path, sizeof(path), "%s/%s", devices, entry->d_name);
        if (length < 0 || (size_t)length >= sizeof(path)) { errno = ENAMETOOLONG; die("virtio port device path"); }
        channel = open(path, O_RDWR|O_NONBLOCK|O_CLOEXEC);
        if (channel < 0 && errno != ENOENT && errno != ENXIO) die("open virtio management port");
        break;
    }
    closedir(directory);
    return channel;
}
static int open_management(const char *sysfs, const char *devices) {
    struct timespec start_time;
    if (clock_gettime(CLOCK_MONOTONIC, &start_time)) die("management startup clock");
    int channel = -1;
    for (;;) {
        if (channel < 0) channel = find_management(sysfs, devices);
        if (channel >= 0) {
            struct pollfd ready = {.fd = channel, .events = POLLOUT};
            int status = poll(&ready, 1, 0);
            if (status < 0 && errno != EINTR) die("poll management startup");
            if (status > 0 && (ready.revents & POLLOUT) && !(ready.revents & (POLLHUP|POLLERR|POLLNVAL))) return channel;
        }
        struct timespec now;
        if (clock_gettime(CLOCK_MONOTONIC, &now)) die("management startup clock");
        if (now.tv_sec - start_time.tv_sec >= 10) { errno = ETIMEDOUT; die("virtio management port startup"); }
        struct timespec remaining = {.tv_sec = 0, .tv_nsec = 5000000};
        while (nanosleep(&remaining, &remaining)) { if (errno != EINTR) die("wait management startup"); }
    }
}

static int launch_error_fd = -1;
static char **arguments, **environment;
static char *user, *working_dir;

static void finish(int status, int error) {
    fflush(NULL);
#ifdef SEEDBOX_SUPERVISOR_TEST
    _exit(error ? 125 : status);
#else
    if (ioperm(0xf4, 2, 1)) { perror("seedbox ioperm"); for (;;) pause(); }
    outb(status, error ? 0xf5 : 0xf4);
#endif
    for (;;) pause();
}
static void die(const char *what) {
    fprintf(stderr, "SEEDBOX_INIT_ERROR %s: %s\n", what, strerror(errno));
    if (getpid() != 1) {
        if (launch_error_fd >= 0) { char error = 1; ssize_t sent; do { sent = write(launch_error_fd, &error, 1); } while (sent < 0 && errno == EINTR); }
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
    if (received != 0) {
        int status; while (waitpid(pid, &status, 0) < 0 && errno == EINTR) {}
        errno = EIO; return -1;
    }
    if (setpgid(pid, pid) && errno != EACCES && errno != ESRCH) die("parent process group");
    return pid;
}
/* Test builds replace only these privileged I/O operations; the management
 * channel and initial application launch still use the production code. */
#ifndef SEEDBOX_READY_IOPERM
#define SEEDBOX_READY_IOPERM ioperm
#define SEEDBOX_READY_OUTL outl
#endif
static void management_ready(void) {
#ifdef SEEDBOX_TEST_READY_DELAY_NS
    /* Acceptance fixture only: retained per-service opt-in prevents delaying
     * the controller or other applications that share this supervisor. */
    for (char **entry = environment; *entry; entry++) {
        if (strcmp(*entry, "SEEDBOX_TEST_DELAY_READY=1")) continue;
        struct timespec remaining = {
            .tv_sec = SEEDBOX_TEST_READY_DELAY_NS / 1000000000ULL,
            .tv_nsec = SEEDBOX_TEST_READY_DELAY_NS % 1000000000ULL
        };
        while (nanosleep(&remaining, &remaining)) {
            if (errno != EINTR) die("test readiness delay");
        }
        break;
    }
#endif
    if (SEEDBOX_READY_IOPERM(SEEDBOX_EFFECT_PORT, SEEDBOX_EFFECT_PORT_COUNT, 1)) die("fault effect port");
    /* Negative effect controls intentionally retain the bootstrap handshake. */
    SEEDBOX_READY_OUTL(SEEDBOX_READY_ID, SEEDBOX_EFFECT_PORT);
}
static void acknowledge(const char *action, uint32_t id, int success) {
#ifdef SEEDBOX_REJECT_EFFECTS
    success = 0; /* Live negative control for the real 32-bit failure port. */
#endif
    struct timespec t;
    if (clock_gettime(CLOCK_MONOTONIC, &t)) die("fault clock");
    unsigned long long ns = (unsigned long long)t.tv_sec*1000000000ULL + t.tv_nsec;
    char frame[256];
    int length = snprintf(frame, sizeof(frame), "SEEDBOX_%s id=%u action=%s guest_ns=%llu\n", success ? "EFFECT" : "FAILURE", id, action, ns);
    if (length < 0 || (size_t)length >= sizeof(frame)) { errno = E2BIG; die("fault effect frame"); }
    send_all(management, frame, (size_t)length);
#if !defined(SEEDBOX_SUPERVISOR_TEST) && !defined(SEEDBOX_DROP_EFFECTS)
    if (id) {
        if (ioperm(SEEDBOX_EFFECT_PORT, SEEDBOX_EFFECT_PORT_COUNT, 1)) die("fault effect port");
        outl(id, success ? SEEDBOX_EFFECT_PORT : SEEDBOX_FAILURE_PORT);
    }
#endif
}
static int stop(pid_t *child) {
    if (*child <= 0) { errno = EINVAL; return -1; }
    pid_t group = *child;
    if (kill(-group, SIGKILL) && errno != ESRCH) return -1;
    int status;
    while (waitpid(group, &status, 0) < 0) { if (errno != EINTR) return -1; }
    *child = -1;
    /* Reap killed descendants adopted by PID 1 in the application group. */
    while (waitpid(-group, &status, 0) >= 0 || errno == EINTR) {}
    return errno == ECHILD ? 0 : -1;
}
static void control(char *command, pid_t *child) {
    if (!strncmp(command, "SBX_REPLY1 ", 11)) {
        char *payload = command + 11, *end; errno = 0;
        unsigned long id = strtoul(payload, &end, 10);
        if (errno || !id || id > UINT32_MAX || *end != ' ' || id != waiting_relay || control_client < 0) { errno = EINVAL; die("stale/malformed control reply"); }
        payload = end + 1; errno = 0;
        unsigned long session = strtoul(payload, &end, 10);
        if (errno || !session || session > UINT32_MAX || *end != ' ' || session != session_id) { errno = EINVAL; die("stale/malformed control session"); }
        send_all(control_client, end + 1, strlen(end + 1)); send_all(control_client, "\n", 1); waiting_relay = 0;
        return;
    }
    char *action; uint32_t id;
    if (control_parse(command, &action, &id)) { errno = EINVAL; die("invalid management command"); }
    int success = 0;
    if (!strcmp(action, "stop")) success = stop(child) == 0;
    else if (*child < 0) { *child = start(); success = *child > 0; }
    if (!id && !success) { errno = EINVAL; die("invalid legacy management action/state"); }
    acknowledge(action, id, success);
}
static char command_frame[RPC_FRAME_MAX+64];
static size_t command_used;
static void relay_management(pid_t *child) {
    char bytes[4096];
    for (;;) {
        ssize_t received = read(management, bytes, sizeof(bytes));
        if (received < 0 && errno == EINTR) continue;
        if (received < 0 && (errno == EAGAIN || errno == EWOULDBLOCK)) return;
        if (received < 0) die("read virtio management port");
        if (!received) { errno = EPIPE; die("virtio management port disconnected"); }
        for (ssize_t i = 0; i < received; i++) {
            char byte = bytes[i];
            if (!byte) { errno = EINVAL; die("NUL in management frame"); }
            if (byte == '\n') {
                command_frame[command_used] = 0;
                command_used = 0;
                control(command_frame, child);
            } else if (command_used + 1 < sizeof(command_frame)) command_frame[command_used++] = byte;
            else { errno = E2BIG; die("management command"); }
        }
    }
}
/* Read and accept only descriptors that poll reports ready. In particular an
 * idle disconnect releases the current session; a partial or outstanding RPC
 * still fails closed. The listener stays open for a later connection. */
static void service_channels(pid_t *child, int timeout) {
    struct pollfd pfds[3] = {
        {.fd = management, .events = POLLIN},
        {.fd = control_client < 0 ? control_listener : -1, .events = POLLIN},
        {.fd = control_client, .events = POLLIN}
    };
    int ready = poll(pfds, 3, timeout);
    if (ready < 0) { if (errno != EINTR) die("poll management"); return; }
    if (!ready) return;
    if (pfds[1].revents & (POLLERR|POLLHUP|POLLNVAL)) { errno = EIO; die("control listener poll"); }
    if (pfds[1].revents & POLLIN) relay_accept();
    if (pfds[2].revents & (POLLERR|POLLNVAL)) { errno = EIO; die("control client poll"); }
    if (pfds[2].revents & (POLLIN|POLLHUP)) relay_requests();
    if (pfds[0].revents & (POLLERR|POLLNVAL)) { errno = EIO; die("management port poll"); }
    if (pfds[0].revents & (POLLIN|POLLHUP)) relay_management(child);
}
#ifndef SEEDBOX_SUPERVISOR_TEST
int main(void) {
    if (signal(SIGPIPE,SIG_IGN)==SIG_ERR) die("ignore relay SIGPIPE");
    directories("/dev", 0755);
    if (mount("devtmpfs", "/dev", "devtmpfs", 0, NULL)) die("mount bootstrap dev");
    int console = open("/dev/console", O_RDWR);
    if (console >= 0) { dup2(console, 0); dup2(console, 1); dup2(console, 2); if (console > 2) close(console); }
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
    management = open_management("/sys/class/virtio-ports", "/dev");
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
    relay_socket("/proc/cmdline", "/run/seedbox/control.sock");
    /* Everything before this point is fixed by the boot seed; the application
     * starts after the world is released under the run seed. */
    if (seedbox_launch_barrier("/proc/cmdline", "/dev/urandom")) die("launch barrier");
    pid_t child = start();
    if (child < 0) die("initial application launch failed");
    management_ready();
    for (;;) {
        int status;
        pid_t done = waitpid(-1, &status, WNOHANG);
        if (done == child && child > 0) finish(WIFEXITED(status) ? WEXITSTATUS(status) : 128 + WTERMSIG(status), 0);
        service_channels(&child, 1);
    }
}

#endif
