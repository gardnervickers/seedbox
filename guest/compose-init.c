/* Generic per-machine OCI application supervisor. Compile statically for Linux.
 * No shell parses image arguments, environment or configuration values.
 * Filesystems are fresh RAM-backed initramfs images for each machine boot;
 * process stop/start preserves their writable contents within that run.
 * Named volumes are the exception: they live on the machine's retained disk
 * (/dev/vda, ext4) and keep what was flushed across a machine crash. */
#define _GNU_SOURCE
#include <arpa/inet.h>
#include <net/if.h>
#include <net/route.h>
#include <sys/ioctl.h>
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
#include "io.h"
#endif
#include "control.h"
#include "application-output.h"
#include "barrier.h"
#include "progress-launch.h"
#include "world-clock.h"
#include <sys/mount.h>
#include <sys/resource.h>
#include <sys/stat.h>
#include <sys/sysmacros.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <sys/timex.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

static int recipe_fd, management = -1;
static uint32_t instructions_per_progress_call = 50000, progress_points_per_block = 1;
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
static int diagnostic_fd = STDERR_FILENO;
/* SIGCHLD stays blocked except inside ppoll, so a child's exit wakes the
 * supervisor and cannot land between the reap and the wait. Waiting needs no
 * timeout: an idle supervisor costs its machine no timer interrupts. */
static int watching;
static sigset_t wait_mask;
static void child_exited(int signal) { (void)signal; }
static void watch_children(void) {
    struct sigaction action = {.sa_handler = child_exited, .sa_flags = SA_NOCLDSTOP};
    sigset_t blocked;
    sigemptyset(&blocked); sigaddset(&blocked, SIGCHLD);
    if (sigemptyset(&action.sa_mask) || sigaction(SIGCHLD, &action, NULL) || sigprocmask(SIG_BLOCK, &blocked, &wait_mask)) die("watch children");
    sigdelset(&wait_mask, SIGCHLD);
    watching = 1;
}
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
    dprintf(diagnostic_fd, "SEEDBOX_INIT_ERROR %s: %s\n", what, strerror(errno));
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
/* Open the supervisor's management channel and mount retained volumes before
 * replacing devtmpfs. Applications get only ordinary container devices. */
static void application_devices(void) {
    if (mount("tmpfs", "/dev", "tmpfs", MS_NOSUID|MS_NOEXEC, "mode=0755,size=65536")) die("mount application dev");
    const struct { const char *name; unsigned minor; } nodes[] = {
        {"null", 3}, {"zero", 5}, {"full", 7}, {"random", 8}, {"urandom", 9}
    };
    for (size_t i = 0; i < sizeof(nodes)/sizeof(*nodes); i++) {
        char path[64]; snprintf(path, sizeof(path), "/dev/%s", nodes[i].name);
        if (mknod(path, S_IFCHR|0666, makedev(1, nodes[i].minor)) || chmod(path, 0666)) die(path);
    }
    if (mknod("/dev/tty", S_IFCHR|0666, makedev(5, 0)) || chmod("/dev/tty", 0666)) die("application tty");
    directories("/dev/shm", 01777); directories("/dev/pts", 0755);
    if (mount("tmpfs", "/dev/shm", "tmpfs", MS_NOSUID|MS_NODEV|MS_NOEXEC, "mode=1777,size=67108864")) die("mount application shm");
    if (mount("devpts", "/dev/pts", "devpts", MS_NOSUID|MS_NOEXEC, "newinstance,ptmxmode=0666,mode=0620,gid=5")) die("mount application pts");
    if (symlink("pts/ptmx", "/dev/ptmx") || symlink("/proc/self/fd", "/dev/fd")
        || symlink("/proc/self/fd/0", "/dev/stdin") || symlink("/proc/self/fd/1", "/dev/stdout")
        || symlink("/proc/self/fd/2", "/dev/stderr")) die("application device links");
}
static void application_hostname(const char *hostname, const char *hosts) {
    if (sethostname(hostname, strlen(hostname))) die("application hostname");
    char line[65];
    int size = snprintf(line, sizeof(line), "%s\n", hostname);
    if (size < 0 || (size_t)size >= sizeof(line)) { errno = EINVAL; die("application hostname length"); }
    write_file("/etc/hostname", line, (size_t)size, 0644);
    /* Preparation already includes this service's address and name. */
    write_file("/etc/hosts", hosts, strlen(hosts), 0644);
    static const char resolv[] = "nameserver 10.0.1.1\n";
    write_file("/etc/resolv.conf", resolv, sizeof(resolv) - 1, 0644);
}
/* Docker's default nofile limit, which applications see unchanged. */
#define APPLICATION_NOFILE 1048576
/* DynamoRIO reserves this many descriptors at the top of the range for itself
 * (its -steal_fds default) and reports the limit minus them to the
 * application, so the real limit sits that much higher. */
#define INSTRUMENTATION_FDS 96
static void write_sysctl(const char *path, unsigned long value) {
    char text[32];
    int size = snprintf(text, sizeof(text), "%lu\n", value);
    int fd = open(path, O_WRONLY|O_CLOEXEC);
    if (fd < 0) die(path);
    /* proc sysctls are kernel interfaces, not config files: no chmod/chown. */
    ssize_t written;
    do { written = write(fd, text, (size_t)size); } while (written < 0 && errno == EINTR);
    if (written != size) { if (written >= 0) errno = EIO; die(path); }
    if (close(fd)) die(path);
}
static void application_sysctls(uint32_t max_map_count) {
    write_sysctl("/proc/sys/vm/max_map_count", max_map_count);
    write_sysctl("/proc/sys/fs/nr_open", APPLICATION_NOFILE + INSTRUMENTATION_FDS);
}
static void application_limits(void) {
    struct rlimit limit = {.rlim_cur = APPLICATION_NOFILE + INSTRUMENTATION_FDS,
                           .rlim_max = APPLICATION_NOFILE + INSTRUMENTATION_FDS};
    if (setrlimit(RLIMIT_NOFILE, &limit)) die("application nofile limit");
}
static void interface_up(int fd, const char *name) {
    struct ifreq request = {0};
    snprintf(request.ifr_name, sizeof(request.ifr_name), "%s", name);
    if (ioctl(fd, SIOCGIFFLAGS, &request)) die("read guest interface flags");
    request.ifr_flags |= IFF_UP;
    if (ioctl(fd, SIOCSIFFLAGS, &request)) die("bring up guest interface");
}
static int network_address(const char *cidr, struct in_addr *address, struct in_addr *mask) {
    char text[INET_ADDRSTRLEN + 4];
    if (strlen(cidr) >= sizeof(text)) return -1;
    strcpy(text, cidr);
    char *slash = strchr(text, '/');
    if (!slash || !slash[1]) return -1;
    *slash++ = 0;
    unsigned prefix = 0;
    for (const char *p = slash; *p; p++) {
        if (*p < '0' || *p > '9') return -1;
        prefix = prefix * 10 + (unsigned)(*p - '0');
        if (prefix > 32) return -1;
    }
    if (inet_pton(AF_INET, text, address) != 1) return -1;
    mask->s_addr = htonl(prefix ? UINT32_MAX << (32 - prefix) : 0);
    return 0;
}
static void configure_network(const char *cidr) {
    struct in_addr address, mask;
    if (network_address(cidr, &address, &mask)) { errno = EINVAL; die("guest IPv4 address/prefix"); }
    int fd = socket(AF_INET, SOCK_DGRAM | SOCK_CLOEXEC, 0);
    if (fd < 0) die("open guest network control");
    interface_up(fd, "lo");
    struct ifreq request = {0};
    snprintf(request.ifr_name, sizeof(request.ifr_name), "%s", "eth0");
    struct sockaddr_in value = {.sin_family = AF_INET, .sin_addr = address};
    memcpy(&request.ifr_addr, &value, sizeof(value));
    if (ioctl(fd, SIOCSIFADDR, &request)) die("set guest IPv4 address");
    value.sin_addr = mask;
    memcpy(&request.ifr_netmask, &value, sizeof(value));
    if (ioctl(fd, SIOCSIFNETMASK, &request)) die("set guest IPv4 netmask");
    interface_up(fd, "eth0");
    /* The simulated DNS service is outside the machine address pool. */
    struct rtentry dns_route = {0};
    struct sockaddr_in destination = {.sin_family = AF_INET};
    if (inet_pton(AF_INET, "10.0.1.1", &destination.sin_addr) != 1) die("DNS service address");
    memcpy(&dns_route.rt_dst, &destination, sizeof(destination));
    destination.sin_addr.s_addr = INADDR_BROADCAST;
    memcpy(&dns_route.rt_genmask, &destination, sizeof(destination));
    dns_route.rt_flags = RTF_UP;
    dns_route.rt_dev = "eth0";
    if (ioctl(fd, SIOCADDRT, &dns_route)) die("route guest DNS service");
    if (close(fd)) die("close guest network control");
}
/* ext4 on the retained disk, formatted on the machine's first boot. Fixed
 * UUID and hash seed; the disk starts zeroed, so inode tables and the journal
 * need no initialization. */
static void format_disk(const char *device) {
    int fd = open(device, O_RDONLY|O_CLOEXEC);
    if (fd < 0) die("open retained disk");
    unsigned char magic[2];
    if (pread(fd, magic, 2, 1024 + 56) != 2) die("read retained disk superblock");
    close(fd);
    if (magic[0] == 0x53 && magic[1] == 0xef) return;
    pid_t pid = fork();
    if (pid < 0) die("fork mke2fs");
    if (!pid) {
        execv("/seedbox/mke2fs", (char *const[]){"mke2fs", "-q", "-F", "-t", "ext4",
            "-U", "5eedb0c5-0000-4000-8000-000000000001",
            "-E", "hash_seed=5eedb0c5-0000-4000-8000-000000000002,lazy_itable_init=1,lazy_journal_init=1,nodiscard",
            (char *)device, NULL});
        _exit(127);
    }
    int status;
    while (waitpid(pid, &status, 0) < 0) { if (errno != EINTR) die("wait mke2fs"); }
    if (!WIFEXITED(status) || WEXITSTATUS(status)) { errno = EIO; die("format retained disk"); }
}
/* Refuse a target reached through a symlink in the image: resolved here,
 * before the chroot, it could point outside the image root. */
static void plain_path(const char *path) {
    char *copy = strdup(path);
    if (!copy) die("volume path");
    for (char *p = copy + 1; ; p++) {
        if (*p != '/' && *p) continue;
        char save = *p; *p = 0;
        struct stat st;
        if (!lstat(copy, &st) && S_ISLNK(st.st_mode)) { errno = ELOOP; die(copy); }
        *p = save;
        if (!save) break;
    }
    free(copy);
}
/* Mount each named volume (name, target pairs) from the retained disk onto
 * its target in the image root. A new volume directory takes the mode and
 * owner of the image's target directory, if there is one; its contents are
 * not copied in. */
static void mount_volumes(char **volumes) {
    if (!*volumes) return;
    directories("/proc", 0755);
    if (mount("proc", "/proc", "proc", 0, NULL)) die("mount bootstrap proc");
    format_disk("/dev/vda");
    directories("/seedbox/disk", 0700);
    if (mount("/dev/vda", "/seedbox/disk", "ext4", 0, "noinit_itable")) die("mount retained disk");
    int created = 0;
    for (char **volume = volumes; volume[0] && volume[1]; volume += 2) {
        char source[PATH_MAX], target[PATH_MAX];
        if (snprintf(source, sizeof source, "/seedbox/disk/%s", volume[0]) >= (int)sizeof source
            || snprintf(target, sizeof target, "/rootfs%s", volume[1]) >= (int)sizeof target) { errno = ENAMETOOLONG; die("volume path"); }
        plain_path(target);
        struct stat image = {.st_mode = S_IFDIR|0755};
        if (stat(target, &image) == 0 && !S_ISDIR(image.st_mode)) { errno = ENOTDIR; die(target); }
        if (mkdir(source, image.st_mode & 07777) == 0) {
            if (chown(source, image.st_uid, image.st_gid) || chmod(source, image.st_mode & 07777)) die(source);
            created = 1;
        } else if (errno != EEXIST) die(source);
        directories(target, 0755);
        if (mount(source, target, NULL, MS_BIND, NULL)) die("bind volume");
    }
    if (created) {
        int root = open("/seedbox/disk", O_RDONLY|O_DIRECTORY|O_CLOEXEC);
        if (root < 0 || fsync(root)) die("sync volume directories");
        close(root);
    }
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
static struct application_output application_outputs[2] = {{-1, -1}, {-1, -1}};
static void relay_output(unsigned stream, size_t limit) {
    char frame[2 * APPLICATION_OUTPUT_BYTES + 16];
    ssize_t size = application_output_frame(&application_outputs[stream], stream, limit, frame);
    if (size < 0) die("read application output");
    if (size) send_all(management, frame, (size_t)size);
}
/* Drain the bytes already queued at termination, without waiting for inherited
 * writers in descendants. A descendant cannot delay the oracle's final exit. */
static void drain_output(void) {
    for (unsigned stream = 0; stream < 2; ++stream) {
        int pending = 0;
        if (application_outputs[stream].read_fd < 0) continue;
        if (ioctl(application_outputs[stream].read_fd, FIONREAD, &pending)) die("pending application output");
        while (pending > 0) {
            size_t count = (size_t)pending < APPLICATION_OUTPUT_BYTES ? (size_t)pending : APPLICATION_OUTPUT_BYTES;
            relay_output(stream, count); pending -= (int)count;
        }
    }
}
static pid_t start(void) {
    for (unsigned stream = 0; stream < 2; ++stream) {
        application_output_close(&application_outputs[stream]);
        if (application_output_open(&application_outputs[stream])) die("application output pipe");
    }
    int launch_error[2];
    if (pipe2(launch_error, O_CLOEXEC)) die("launch error pipe");
    pid_t pid = fork();
    if (pid < 0) die("fork application");
    if (!pid) {
        close(launch_error[0]); launch_error_fd = launch_error[1];
        diagnostic_fd = fcntl(STDERR_FILENO, F_DUPFD_CLOEXEC, 3);
        if (diagnostic_fd < 0) { diagnostic_fd = STDERR_FILENO; die("supervisor diagnostic descriptor"); }
        if (setpgid(0, 0)) die("application process group");
        if (watching && sigprocmask(SIG_SETMASK, &wait_mask, NULL)) die("application signal mask");
        if (management >= 0) close(management);
        for (unsigned stream = 0; stream < 2; ++stream) {
            if (dup2(application_outputs[stream].write_fd, (int)stream + 1) < 0) die("application output");
            application_output_close(&application_outputs[stream]);
        }
        int input = open("/dev/null", O_RDONLY);
        if (input < 0 || dup2(input, STDIN_FILENO) < 0) die("application stdin");
        if (input > 2) close(input);
        if (chdir(working_dir)) die("working directory");
        application_limits();
        credentials();
        environ = environment;
        arguments = seedbox_script_arguments(arguments);
        if (!arguments) die("resolve application interpreter");
        size_t count = 0;
        while (arguments[count]) ++count;
        /* Translator arguments, the application's own, and execv's NULL. */
        enum { PREFIX_MAX = 13 };
        char **instrumented = calloc(PREFIX_MAX + count + 1, sizeof(*instrumented));
        if (!instrumented) die("instrumentation arguments");
        size_t prefix = 0;
        instrumented[prefix++] = "/seedbox/progress/bin64/drrun";
        instrumented[prefix++] = "-opt_speed";
        instrumented[prefix++] = "-disable_traces";
#if defined(__aarch64__)
        /* The patched ARM translator implements guest-epoch reservations and
         * hides host-only exclusive-store retries from application execution. */
        instrumented[prefix++] = "-ldstex2cas";
        /* Detect rewritten code through the application's IC IVAU, rather
         * than write protection and sandboxing; the latter exhausted memory
         * in the observed Kafka startup. Clear CTR_EL0.DIC so applications
         * cannot legally omit the IC IVAU that invalidates cached code. */
        instrumented[prefix++] = "-no_hw_cache_consistency";
        instrumented[prefix++] = "-fake_ctr_dic";
#endif
        instrumented[prefix++] = "-c";
        instrumented[prefix++] = "/seedbox/progress/libseedbox_progress.so";
        char progress_budget[11], progress_points[11];
        snprintf(progress_budget, sizeof(progress_budget), "%u", instructions_per_progress_call);
        snprintf(progress_points, sizeof(progress_points), "%u", progress_points_per_block);
        instrumented[prefix++] = "--instructions-per-progress-call";
        instrumented[prefix++] = progress_budget;
        instrumented[prefix++] = "--progress-points-per-block";
        instrumented[prefix++] = progress_points;
        instrumented[prefix++] = "--";
        if (prefix > PREFIX_MAX) die("instrumentation prefix");
        for (size_t i = 0; i < count; ++i) instrumented[i + prefix] = arguments[i];
        instrumented[prefix + count] = NULL;
        execv(instrumented[0], instrumented);
        die("exec application");
    }
    close(launch_error[1]);
    for (unsigned stream = 0; stream < 2; ++stream) {
        close(application_outputs[stream].write_fd);
        application_outputs[stream].write_fd = -1;
    }
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
    if (errno != ECHILD) return -1;
    drain_output();
    for (unsigned stream = 0; stream < 2; ++stream) application_output_close(&application_outputs[stream]);
    return 0;
}
/* Test builds replace the privileged clock adjustment. */
#ifndef SEEDBOX_ADJTIMEX
#define SEEDBOX_ADJTIMEX adjtimex
#endif
/* Step CLOCK_REALTIME by a signed offset in one kernel operation. The
 * timekeeper moves only the wall-to-monotonic offset, so monotonic and boot
 * time, the TSC and pending relative timers are unaffected; absolute
 * CLOCK_REALTIME timers are re-evaluated by the kernel. */
static int step_wall_clock(int64_t ns) {
    struct timex adjustment = {.modes = ADJ_SETOFFSET | ADJ_NANO};
    adjustment.time.tv_sec = ns / 1000000000;
    adjustment.time.tv_usec = ns % 1000000000;
    if (adjustment.time.tv_usec < 0) { adjustment.time.tv_usec += 1000000000; adjustment.time.tv_sec--; }
    return SEEDBOX_ADJTIMEX(&adjustment) < 0 ? -1 : 0;
}
/* Run the system clock `ppb` parts per billion fast (slow if negative), like
 * an oscillator error. The NTP tick length scales CLOCK_REALTIME, MONOTONIC
 * and BOOTTIME together; MONOTONIC_RAW, the TSC and the simulated time base
 * keep their rate. ADJ_TICK moves in steps of one microsecond per USER_HZ
 * tick and ADJ_FREQUENCY covers the remainder, so the result is absolute:
 * zero restores the nominal rate. */
static int set_clock_rate(int64_t ppb) {
    if (ppb > 1000000000 || ppb < -1000000000) { errno = EINVAL; return -1; }
    long hz = sysconf(_SC_CLK_TCK);
    if (hz <= 0 || 1000000 % hz) { errno = EINVAL; return -1; }
    int64_t nominal = 1000000 / hz, per_tick = 1000000000 / nominal;
    int64_t ticks = (ppb + (ppb < 0 ? -per_tick : per_tick) / 2) / per_tick;
    int64_t rest = ppb - ticks * per_tick;
    struct timex adjustment = {.modes = ADJ_TICK | ADJ_FREQUENCY};
    adjustment.tick = nominal + ticks;
    /* freq is parts per million with a 16-bit fraction. */
    adjustment.freq = (rest * 65536 + (rest < 0 ? -500 : 500)) / 1000;
    return SEEDBOX_ADJTIMEX(&adjustment) < 0 ? -1 : 0;
}
/* SIGSTOP the application's process group, as a debugger or GC pause would;
 * its kernel keeps ACKing and accepting connections. Confirm once the group
 * leader reports the stop, leaving any exit for supervise() to reap. */
static int freeze(pid_t *child) {
    if (*child <= 0) { errno = EINVAL; return -1; }
    if (kill(-*child, SIGSTOP)) return -1;
    siginfo_t info = {0};
    while (waitid(P_PID, *child, &info, WSTOPPED|WEXITED|WNOWAIT) < 0) { if (errno != EINTR) return -1; }
    if (info.si_code != CLD_STOPPED) { errno = ESRCH; return -1; }
    return 0;
}
/* SIGCONT resumes the stopped group as it is sent; there is nothing to wait for. */
static int thaw(pid_t *child) {
    if (*child <= 0) { errno = EINVAL; return -1; }
    return kill(-*child, SIGCONT);
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
    char *action; uint32_t id; int64_t step;
    if (control_parse(command, &action, &id, &step)) { errno = EINVAL; die("invalid management command"); }
    int success = 0;
    if (!strcmp(action, "clock_step")) success = step_wall_clock(step) == 0;
    else if (!strcmp(action, "clock_rate")) success = set_clock_rate(step) == 0;
    else if (!strcmp(action, "stop")) success = stop(child) == 0;
    else if (!strcmp(action, "freeze")) success = freeze(child) == 0;
    else if (!strcmp(action, "thaw")) success = thaw(child) == 0;
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
    struct pollfd pfds[5] = {
        {.fd = management, .events = POLLIN},
        {.fd = control_client < 0 ? control_listener : -1, .events = POLLIN},
        {.fd = control_client, .events = POLLIN},
        {.fd = application_outputs[0].read_fd, .events = POLLIN},
        {.fd = application_outputs[1].read_fd, .events = POLLIN}
    };
    struct timespec limit = {.tv_sec = timeout / 1000, .tv_nsec = timeout % 1000 * 1000000L};
    int ready = ppoll(pfds, 5, timeout < 0 ? NULL : &limit, watching ? &wait_mask : NULL);
    if (ready < 0) { if (errno != EINTR) die("poll management"); return; }
    if (!ready) return;
    if (pfds[1].revents & (POLLERR|POLLHUP|POLLNVAL)) { errno = EIO; die("control listener poll"); }
    if (pfds[1].revents & POLLIN) relay_accept();
    if (pfds[2].revents & (POLLERR|POLLNVAL)) { errno = EIO; die("control client poll"); }
    if (pfds[2].revents & (POLLIN|POLLHUP)) relay_requests();
    for (unsigned stream = 0; stream < 2; ++stream) {
        if (pfds[3 + stream].revents & (POLLERR|POLLNVAL)) { errno = EIO; die("application output poll"); }
        if (pfds[3 + stream].revents & (POLLIN|POLLHUP)) relay_output(stream, APPLICATION_OUTPUT_BYTES);
    }
    if (pfds[0].revents & (POLLERR|POLLNVAL)) { errno = EIO; die("management port poll"); }
    if (pfds[0].revents & (POLLIN|POLLHUP)) relay_management(child);
}
static void supervise(pid_t *child) {
    for (;;) {
        int status;
        pid_t done;
        while ((done = waitpid(-1, &status, WNOHANG)) > 0) {
            if (done == *child) {
                drain_output();
                finish(WIFEXITED(status) ? WEXITSTATUS(status) : 128 + WTERMSIG(status), 0);
            }
        }
        service_channels(child, -1);
    }
}
#ifndef SEEDBOX_SUPERVISOR_TEST
int main(void) {
    if (signal(SIGPIPE,SIG_IGN)==SIG_ERR) die("ignore relay SIGPIPE");
    directories("/dev", 0755);
    if (mount("devtmpfs", "/dev", "devtmpfs", 0, NULL)) die("mount bootstrap dev");
    int console = open("/dev/console", O_RDWR);
    if (console >= 0) {
        /* /proc/self/fd/1 rechecks this inode's permissions on reopen, even
         * after the console node is hidden. Non-root images need stdout too. */
        if (fchmod(console, 0666)) die("console permissions");
        dup2(console, 0); dup2(console, 1); dup2(console, 2); if (console > 2) close(console);
    }
#if defined(__aarch64__)
    if (seedbox_retain_io("/dev/seedbox-io")) die("retain supervisor I/O");
#endif
    recipe_fd = open("/seedbox/launch.bin", O_RDONLY|O_CLOEXEC);
    if (recipe_fd < 0) die("launch recipe");
    char magic[10]; read_exact(magic, sizeof(magic));
    if (memcmp(magic, "SBXLAUNCH6", 10)) { errno = EINVAL; die("launch version"); }
    char *ip = text(), *hosts = text(), *hostname = text(); user = text(); working_dir = text();
    uint32_t max_map_count = count();
    uint32_t wall_clock_epoch = count();
    instructions_per_progress_call = count();
    if (!instructions_per_progress_call) { errno = EINVAL; die("zero progress instruction budget"); }
    progress_points_per_block = count();
    if (!progress_points_per_block) { errno = EINVAL; die("zero progress points per block"); }
    arguments = list(); environment = list(); char **volumes = list(); char **disk_volumes = list();
    if (!arguments[0] || !*arguments[0]) { errno = EINVAL; die("empty executable"); }
    configure_network(ip);
    mount_volumes(disk_volumes);
    directories("/sys", 0755);
    if (mount("sysfs", "/sys", "sysfs", 0, NULL)) die("mount bootstrap sys");
    management = open_management("/sys/class/virtio-ports", "/dev");
    plain_path("/rootfs/seedbox/progress");
    directories("/rootfs/seedbox/progress", 0755);
    if (mount("/seedbox/progress", "/rootfs/seedbox/progress", NULL, MS_BIND, NULL)
        || mount(NULL, "/rootfs/seedbox/progress", NULL,
                 MS_BIND|MS_REMOUNT|MS_RDONLY|MS_NOSUID|MS_NODEV, NULL)) die("mount progress runtime");
    /* Linux rejects CLONE_NEWUSER from a chroot. Cover the mount namespace's
     * root with the image first, including its already-mounted named volumes;
     * then make the process root refer to that same mount root. Bootstrap
     * device/management handles are already retained above. */
    if (mount("/rootfs", "/rootfs", NULL, MS_BIND|MS_REC, NULL)
        || chdir("/rootfs") || mount(".", "/", NULL, MS_MOVE, NULL)
        || chroot(".") || chdir("/")) die("enter image mount root");
    directories("/dev", 0755); directories("/proc", 0755); directories("/sys", 0755); directories("/tmp", 01777);
    if (mount("proc", "/proc", "proc", 0, NULL) || mount("sysfs", "/sys", "sysfs", 0, NULL)) die("mount guest filesystems");
    application_devices();
    application_hostname(hostname, hosts);
    application_sysctls(max_map_count);
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
    struct timespec wall_clock = seedbox_wall_clock_at(wall_clock_epoch, seedbox_world_time_ns());
    if (clock_settime(CLOCK_REALTIME, &wall_clock)) die("set recorded wall clock");
    if (seedbox_launch_barrier("/proc/cmdline", "/dev/urandom")) die("launch barrier");
    watch_children();
    pid_t child = start();
    if (child < 0) die("initial application launch failed");
    management_ready();
    supervise(&child);
}

#endif
