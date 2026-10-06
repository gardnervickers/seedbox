/* Generic OCI application supervisor: PID 1 of every guest machine. Compile
 * statically for amd64. No shell parses image arguments, environment or
 * configuration values.
 *
 * Each distinct image is unpacked once, read-only, at /images/ID. Each service
 * runs in its own detached overlay of its image (/services/NAME, writable layer
 * /seedbox/services/NAME) with its own mount, PID and UTS namespaces. With a machine NIC, the machine's single
 * service uses eth0 directly. When services share the kernel, each also has
 * its own network namespace, joined to the root namespace by a veth pair. The
 * root namespace routes between services, delays every packet once on egress
 * towards its destination (netem) and applies partitions as blackhole routing
 * rules. Filesystems are fresh RAM-backed initramfs images for each machine
 * boot; process stop/start preserves their writable contents within that run. */
#define _GNU_SOURCE
#include <arpa/inet.h>
#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <grp.h>
#include <limits.h>
#include <linux/capability.h>
#include <linux/fib_rules.h>
#include <linux/netlink.h>
#include <linux/pkt_sched.h>
#include <linux/rtnetlink.h>
#include <linux/veth.h>
#include <net/if.h>
#include <poll.h>
#include <pwd.h>
#include <sched.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#ifndef SEEDBOX_SUPERVISOR_TEST
#include <sys/io.h>
#endif
#include "control.h"
#include <sys/mount.h>
#include <sys/prctl.h>
#include <sys/signalfd.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/sysmacros.h>
#include <sys/un.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

/* Test builds run services as ordinary children of the test process. */
#ifdef SEEDBOX_SUPERVISOR_TEST
#define SERVICE_NAMESPACES 0
#else
#define SERVICE_NAMESPACES (CLONE_NEWNS|CLONE_NEWPID|CLONE_NEWUTS)
#endif
#define RPC_FRAME_MAX 16384
#define MAX_SERVICES 254
#define CONTROL_DIRECTORY "/seedbox/control"
#define CONTROL_SOCKET CONTROL_DIRECTORY "/control.sock"

enum state { RUNNING, STOPPED, EXITED };
struct service {
    char *name, *image, *address, *user, *working_dir;
    char **arguments, **environment, **volumes;
    uint32_t configs;
    char **config_paths, **config_contents;
    uint32_t *config_sizes;
    int check, controller;
    enum state state;
    pid_t init;     /* Service init, PID 1 of the service's PID namespace. */
    int netns;      /* Shared-kernel network namespace, or -1. */
    int output;     /* Read end of the stdout/stderr relay, or -1. */
    char line[4096];
    size_t used;
};
struct cut { char *handle; uint32_t count; uint8_t (*pairs)[2]; };

static struct service services[MAX_SERVICES];
static uint32_t service_count;
static int namespaces; /* Services share this kernel, each with its own netns. */
static uint64_t latency_ns, jitter_ns;
static uint32_t disk_mb;
static char *hosts;
static struct cut *cuts;
static size_t cut_count;
static int recipe_fd, management = -1, children = -1, root_netlink = -1;
static int supervisor = 1, launch_error_fd = -1;
static int control_listener = -1, control_client = -1;
static uint32_t relay_id, waiting_relay, session_id;
static char request_frame[RPC_FRAME_MAX];
static size_t request_used;

static void die(const char *what);
static void directories(const char *path, mode_t mode);

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
/* Only the supervisor reports bootstrap failure for the machine. A service
 * init (PID 1 of its own namespace) or application reports a launch error. */
static void die(const char *what) {
    fprintf(stderr, "SEEDBOX_INIT_ERROR %s: %s\n", what, strerror(errno));
    if (!supervisor) {
        if (launch_error_fd >= 0) { char error = 1; ssize_t sent; do { sent = write(launch_error_fd, &error, 1); } while (sent < 0 && errno == EINTR); }
        _exit(125);
    }
    finish(1, 1);
}
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

/* ---- Launch recipe (SBXLAUNCH3, little-endian length-prefixed fields) ---- */
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
static uint64_t count64(void) { uint64_t low = count(); return low | (uint64_t)count() << 32; }
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
/* Service names (DNS labels), image IDs (64 hex digits) and handles. */
static int token(const char *name) {
    return *name && !strpbrk(name, " ,\t\r\n/") && strlen(name) <= 64;
}
static void read_recipe(const char *path) {
    recipe_fd = open(path, O_RDONLY|O_CLOEXEC);
    if (recipe_fd < 0) die("launch recipe");
    char magic[10]; read_exact(magic, sizeof(magic));
    if (memcmp(magic, "SBXLAUNCH3", 10)) { errno = EINVAL; die("launch version"); }
    uint32_t mode = count();
    if (mode > 1) { errno = EINVAL; die("network mode"); }
    namespaces = mode == 1;
    latency_ns = count64(); jitter_ns = count64();
    disk_mb = count();
    if (!disk_mb) { errno = EINVAL; die("disk capacity"); }
    hosts = text();
    service_count = count();
    if (!service_count || service_count > MAX_SERVICES || (!namespaces && service_count != 1)) { errno = EINVAL; die("service count"); }
    for (uint32_t i = 0; i < service_count; i++) {
        struct service *s = &services[i];
        s->name = text(); s->image = text(); s->address = text();
        uint32_t flags = count();
        struct in_addr address;
        if (!token(s->name) || !token(s->image) || inet_pton(AF_INET, s->address, &address) != 1 || flags > 3) { errno = EINVAL; die("service identity"); }
        for (uint32_t j = 0; j < i; j++) if (!strcmp(services[j].name, s->name)) { errno = EINVAL; die("duplicate service"); }
        s->check = flags & 1; s->controller = flags >> 1 & 1;
        s->user = text(); s->working_dir = text();
        s->arguments = list(); s->environment = list(); s->volumes = list();
        if (!s->arguments[0] || !*s->arguments[0]) { errno = EINVAL; die("empty executable"); }
        s->configs = count();
        if (s->configs > 65536) { errno = E2BIG; die("config count"); }
        s->config_paths = calloc(s->configs + 1, sizeof(char *));
        s->config_contents = calloc(s->configs + 1, sizeof(char *));
        s->config_sizes = calloc(s->configs + 1, sizeof(uint32_t));
        if (!s->config_paths || !s->config_contents || !s->config_sizes) die("allocate configs");
        for (uint32_t c = 0; c < s->configs; c++) {
            s->config_paths[c] = text();
            s->config_contents[c] = field(&s->config_sizes[c]);
        }
        s->state = STOPPED; s->init = -1; s->netns = -1; s->output = -1;
    }
    close(recipe_fd);
}
static struct service *find_service(const char *name) {
    for (uint32_t i = 0; i < service_count; i++) if (!strcmp(services[i].name, name)) return &services[i];
    return NULL;
}

/* ---- Runtime control relay (controller service only) ---- */
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

/* ---- Filesystem helpers (inside a service root) ---- */
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
    int fd = open(path, O_WRONLY|O_CREAT|O_TRUNC|O_CLOEXEC, mode);
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
static void credentials(const char *user) {
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

/* ---- Netlink (shared-kernel network) ---- */
#define NETLINK_BUFFER 4096
struct netlink_request { struct nlmsghdr header; char body[NETLINK_BUFFER - sizeof(struct nlmsghdr)]; };
static void netlink_start(struct netlink_request *r, uint16_t type, uint16_t flags, const void *message, size_t size) {
    memset(r, 0, sizeof(*r));
    r->header.nlmsg_type = type;
    r->header.nlmsg_flags = NLM_F_REQUEST|NLM_F_ACK|flags;
    r->header.nlmsg_len = NLMSG_LENGTH(size);
    memcpy(NLMSG_DATA(&r->header), message, size);
}
static struct rtattr *netlink_attribute(struct netlink_request *r, unsigned short type, const void *data, size_t size) {
    size_t offset = NLMSG_ALIGN(r->header.nlmsg_len);
    if (offset + RTA_SPACE(size) > sizeof(*r)) { errno = E2BIG; die("netlink request"); }
    struct rtattr *attribute = (struct rtattr *)((char *)r + offset);
    attribute->rta_type = type;
    attribute->rta_len = RTA_LENGTH(size);
    if (size) memcpy(RTA_DATA(attribute), data, size);
    r->header.nlmsg_len = offset + RTA_SPACE(size);
    return attribute;
}
static void netlink_raw(struct netlink_request *r, const void *data, size_t size) {
    size_t offset = NLMSG_ALIGN(r->header.nlmsg_len);
    if (offset + NLMSG_ALIGN(size) > sizeof(*r)) { errno = E2BIG; die("netlink request"); }
    memcpy((char *)r + offset, data, size);
    r->header.nlmsg_len = offset + NLMSG_ALIGN(size);
}
static void netlink_end(struct netlink_request *r, struct rtattr *nest) {
    nest->rta_len = (unsigned short)((char *)r + r->header.nlmsg_len - (char *)nest);
}
/* Returns 0 or a negative errno from the kernel's acknowledgement. */
static int netlink_send(int fd, struct netlink_request *r) {
    static uint32_t sequence;
    r->header.nlmsg_seq = ++sequence;
    struct sockaddr_nl kernel = {.nl_family = AF_NETLINK};
    ssize_t sent;
    do { sent = sendto(fd, r, r->header.nlmsg_len, 0, (struct sockaddr *)&kernel, sizeof(kernel)); } while (sent < 0 && errno == EINTR);
    if (sent != (ssize_t)r->header.nlmsg_len) die("netlink send");
    for (;;) {
        char reply[8192] __attribute__((aligned(NLMSG_ALIGNTO)));
        ssize_t n = recv(fd, reply, sizeof(reply), 0);
        if (n < 0 && errno == EINTR) continue;
        if (n <= 0) die("netlink receive");
        for (size_t offset = 0; offset + sizeof(struct nlmsghdr) <= (size_t)n;) {
            struct nlmsghdr *h = (struct nlmsghdr *)(reply + offset);
            if (h->nlmsg_len < sizeof(*h) || offset + h->nlmsg_len > (size_t)n) break;
            if (h->nlmsg_seq == r->header.nlmsg_seq && h->nlmsg_type == NLMSG_ERROR)
                return ((struct nlmsgerr *)NLMSG_DATA(h))->error;
            offset += NLMSG_ALIGN(h->nlmsg_len);
        }
    }
}
static void netlink(int fd, struct netlink_request *r, const char *what) {
    int error = netlink_send(fd, r);
    if (error) { errno = -error; die(what); }
}
static int netlink_socket(void) {
    int fd = socket(AF_NETLINK, SOCK_RAW|SOCK_CLOEXEC, NETLINK_ROUTE);
    if (fd < 0) die("netlink socket");
    return fd;
}
static void link_up(int fd, int index) {
    struct netlink_request r;
    struct ifinfomsg link = {.ifi_family = AF_UNSPEC, .ifi_index = index, .ifi_flags = IFF_UP, .ifi_change = IFF_UP};
    netlink_start(&r, RTM_NEWLINK, 0, &link, sizeof(link));
    netlink(fd, &r, "link up");
}
static void address_add(int fd, int index, const char *text_address, int prefix, uint32_t flags) {
    struct netlink_request r;
    struct in_addr address;
    if (inet_pton(AF_INET, text_address, &address) != 1) { errno = EINVAL; die("address"); }
    struct ifaddrmsg message = {.ifa_family = AF_INET, .ifa_prefixlen = prefix, .ifa_scope = RT_SCOPE_UNIVERSE, .ifa_index = index};
    netlink_start(&r, RTM_NEWADDR, NLM_F_CREATE|NLM_F_EXCL, &message, sizeof(message));
    netlink_attribute(&r, IFA_LOCAL, &address, 4);
    netlink_attribute(&r, IFA_ADDRESS, &address, 4);
    if (flags) netlink_attribute(&r, IFA_FLAGS, &flags, 4);
    netlink(fd, &r, "address add");
}
static void route_add(int fd, int index, const char *destination, int prefix, const char *gateway) {
    struct netlink_request r;
    struct in_addr dst, via;
    if (inet_pton(AF_INET, destination, &dst) != 1 || (gateway && inet_pton(AF_INET, gateway, &via) != 1)) { errno = EINVAL; die("route"); }
    struct rtmsg message = {.rtm_family = AF_INET, .rtm_dst_len = prefix, .rtm_table = RT_TABLE_MAIN,
        .rtm_protocol = RTPROT_BOOT, .rtm_scope = gateway ? RT_SCOPE_UNIVERSE : RT_SCOPE_LINK,
        .rtm_type = RTN_UNICAST, .rtm_flags = gateway ? RTNH_F_ONLINK : 0};
    netlink_start(&r, RTM_NEWROUTE, NLM_F_CREATE|NLM_F_EXCL, &message, sizeof(message));
    netlink_attribute(&r, RTA_DST, &dst, 4);
    if (gateway) netlink_attribute(&r, RTA_GATEWAY, &via, 4);
    netlink_attribute(&r, RTA_OIF, &index, 4);
    netlink(fd, &r, "route add");
}
/* Static neighbours: no ARP traffic or timers between namespaces. */
static void neighbour_add(int fd, int index, const char *text_address, const unsigned char mac[6]) {
    struct netlink_request r;
    struct in_addr address;
    if (inet_pton(AF_INET, text_address, &address) != 1) { errno = EINVAL; die("neighbour"); }
    struct ndmsg message = {.ndm_family = AF_INET, .ndm_ifindex = index, .ndm_state = NUD_PERMANENT};
    netlink_start(&r, RTM_NEWNEIGH, NLM_F_CREATE|NLM_F_REPLACE, &message, sizeof(message));
    netlink_attribute(&r, NDA_DST, &address, 4);
    netlink_attribute(&r, NDA_LLADDR, mac, 6);
    netlink(fd, &r, "neighbour add");
}
static void veth_add(int fd, const char *name, int index, const unsigned char mac[6], int peer_index, const unsigned char peer_mac[6], int netns) {
    struct netlink_request r;
    struct ifinfomsg host = {.ifi_family = AF_UNSPEC, .ifi_index = index};
    netlink_start(&r, RTM_NEWLINK, NLM_F_CREATE|NLM_F_EXCL, &host, sizeof(host));
    netlink_attribute(&r, IFLA_IFNAME, name, strlen(name) + 1);
    netlink_attribute(&r, IFLA_ADDRESS, mac, 6);
    struct rtattr *info = netlink_attribute(&r, IFLA_LINKINFO, NULL, 0);
    netlink_attribute(&r, IFLA_INFO_KIND, "veth", 4);
    struct rtattr *data = netlink_attribute(&r, IFLA_INFO_DATA, NULL, 0);
    struct rtattr *peer = netlink_attribute(&r, VETH_INFO_PEER, NULL, 0);
    struct ifinfomsg peer_link = {.ifi_family = AF_UNSPEC, .ifi_index = peer_index};
    netlink_raw(&r, &peer_link, sizeof(peer_link));
    netlink_attribute(&r, IFLA_IFNAME, "eth0", 5);
    netlink_attribute(&r, IFLA_ADDRESS, peer_mac, 6);
    uint32_t namespace = (uint32_t)netns;
    netlink_attribute(&r, IFLA_NET_NS_FD, &namespace, 4);
    netlink_end(&r, peer); netlink_end(&r, data); netlink_end(&r, info);
    netlink(fd, &r, "veth add");
}
/* Delay latency + U[0, jitter), like the engine's fabric: netem draws from
 * [mu - sigma, mu + sigma). With jitter, the rate path orders each packet
 * after the previous one, so a destination's packets never reorder. Its PRNG
 * is seeded from the guest kernel's (seeded) random source. */
static void netem_add(int fd, int index) {
    struct netlink_request r;
    struct tcmsg message = {.tcm_family = AF_UNSPEC, .tcm_ifindex = index, .tcm_handle = 0x10000, .tcm_parent = TC_H_ROOT};
    int64_t mu = (int64_t)(latency_ns + jitter_ns / 2), sigma = (int64_t)(jitter_ns / 2);
    struct tc_netem_qopt options = {.latency = (uint32_t)((uint64_t)mu >> 6 > UINT32_MAX ? UINT32_MAX : (uint64_t)mu >> 6),
        .limit = 10000, .jitter = (uint32_t)((uint64_t)sigma >> 6 > UINT32_MAX ? UINT32_MAX : (uint64_t)sigma >> 6)};
    netlink_start(&r, RTM_NEWQDISC, NLM_F_CREATE|NLM_F_EXCL, &message, sizeof(message));
    netlink_attribute(&r, TCA_KIND, "netem", 6);
    struct rtattr *nest = netlink_attribute(&r, TCA_OPTIONS, &options, sizeof(options));
    netlink_attribute(&r, TCA_NETEM_LATENCY64, &mu, 8);
    netlink_attribute(&r, TCA_NETEM_JITTER64, &sigma, 8);
    if (jitter_ns) {
        struct tc_netem_rate rate = {0};
        uint64_t bytes_per_second = 1000000000000ULL;
        netlink_attribute(&r, TCA_NETEM_RATE, &rate, sizeof(rate));
        netlink_attribute(&r, TCA_NETEM_RATE64, &bytes_per_second, 8);
    }
    netlink_end(&r, nest);
    netlink(fd, &r, "netem qdisc");
}
static int rule(int add, uint32_t from, uint32_t to) {
    struct netlink_request r;
    struct fib_rule_hdr message = {.family = AF_INET, .dst_len = 32, .src_len = 32, .action = FR_ACT_BLACKHOLE};
    struct in_addr source, destination;
    if (inet_pton(AF_INET, services[from].address, &source) != 1 || inet_pton(AF_INET, services[to].address, &destination) != 1) return -EINVAL;
    /* Identical rules may repeat, one per handle: each heal removes one copy. */
    netlink_start(&r, add ? RTM_NEWRULE : RTM_DELRULE, add ? NLM_F_CREATE : 0, &message, sizeof(message));
    uint32_t priority = 100;
    netlink_attribute(&r, FRA_SRC, &source, 4);
    netlink_attribute(&r, FRA_DST, &destination, 4);
    netlink_attribute(&r, FRA_PRIORITY, &priority, 4);
    return netlink_send(root_netlink, &r);
}
static void write_sysctl(const char *path, const char *value) {
    int fd = open(path, O_WRONLY|O_CLOEXEC);
    if (fd < 0 || write(fd, value, strlen(value)) != (ssize_t)strlen(value) || close(fd)) die(path);
}
#define GATEWAY "10.0.1.1"
/* Service i: eth0 is ifindex 2 in its namespace with address 10.0.0.(i+1)/24
 * and MAC 52:54:00:00:00:(i+1); the root side is svc<i>, ifindex 100+i. */
static void setup_network(void) {
    int root = open("/proc/self/ns/net", O_RDONLY|O_CLOEXEC);
    if (root < 0) die("root network namespace");
    root_netlink = netlink_socket();
    link_up(root_netlink, 1);
    if (!namespaces) {
        int eth0 = if_nametoindex("eth0");
        if (!eth0) die("machine network device");
        address_add(root_netlink, eth0, services[0].address, 24, 0);
        link_up(root_netlink, eth0);
        return;
    }
    write_sysctl("/proc/sys/net/ipv4/ip_forward", "1");
    address_add(root_netlink, 1, GATEWAY, 32, 0);
    for (uint32_t i = 0; i < service_count; i++) {
        struct service *s = &services[i];
        if (unshare(CLONE_NEWNET)) die("service network namespace");
        s->netns = open("/proc/self/ns/net", O_RDONLY|O_CLOEXEC);
        if (s->netns < 0) die("open service network namespace");
        int inside = netlink_socket();
        if (setns(root, CLONE_NEWNET)) die("return to root network namespace");
        unsigned char mac[6] = {0x52, 0x54, 0, 0, 0, (unsigned char)(i + 1)}, host[6] = {0x52, 0x54, 0, 0, 1, (unsigned char)(i + 1)};
        char name[IFNAMSIZ];
        snprintf(name, sizeof(name), "svc%u", i);
        veth_add(root_netlink, name, 100 + (int)i, host, 2, mac, s->netns);
        link_up(inside, 1);
        address_add(inside, 2, s->address, 24, IFA_F_NOPREFIXROUTE);
        link_up(inside, 2);
        neighbour_add(inside, 2, GATEWAY, host);
        route_add(inside, 2, "10.0.0.0", 24, GATEWAY);
        close(inside);
        link_up(root_netlink, 100 + (int)i);
        neighbour_add(root_netlink, 100 + (int)i, s->address, mac);
        route_add(root_netlink, 100 + (int)i, s->address, 32, NULL);
        netem_add(root_netlink, 100 + (int)i);
    }
    close(root);
}

/* ---- Partitions (shared kernel) ---- */
static int parse_side(char *list, uint8_t *members, uint32_t *size, uint8_t *used) {
    *size = 0;
    for (char *save, *name = strtok_r(list, ",", &save); name; name = strtok_r(NULL, ",", &save)) {
        struct service *s = find_service(name);
        if (!s) return -1;
        uint8_t index = (uint8_t)(s - services);
        if (used[index]) return -1;
        used[index] = 1;
        members[(*size)++] = index;
    }
    return *size ? 0 : -1;
}
static int partition(const char *handle, char *left, char *right) {
    if (!namespaces || !token(handle)) return -1;
    for (size_t i = 0; i < cut_count; i++) if (!strcmp(cuts[i].handle, handle)) return -1;
    uint8_t a[MAX_SERVICES], b[MAX_SERVICES], used[MAX_SERVICES] = {0};
    uint32_t na, nb;
    if (parse_side(left, a, &na, used) || parse_side(right, b, &nb, used)) return -1;
    struct cut cut = {.handle = strdup(handle), .count = 0, .pairs = calloc((size_t)na * nb * 2, 2)};
    struct cut *grown = realloc(cuts, (cut_count + 1) * sizeof(*cuts));
    if (!cut.handle || !cut.pairs || !grown) die("allocate partition");
    cuts = grown;
    for (uint32_t i = 0; i < na; i++) for (uint32_t j = 0; j < nb; j++) {
        uint8_t pairs[2][2] = {{a[i], b[j]}, {b[j], a[i]}};
        for (int k = 0; k < 2; k++) {
            if (rule(1, pairs[k][0], pairs[k][1])) return -1;
            memcpy(cut.pairs[cut.count++], pairs[k], 2);
        }
    }
    cuts[cut_count++] = cut;
    return 0;
}
static int heal(const char *handle) {
    for (size_t i = 0; i < cut_count; i++) {
        if (strcmp(cuts[i].handle, handle)) continue;
        for (uint32_t k = 0; k < cuts[i].count; k++) if (rule(0, cuts[i].pairs[k][0], cuts[i].pairs[k][1])) return -1;
        free(cuts[i].handle); free(cuts[i].pairs);
        cuts[i] = cuts[--cut_count];
        return 0;
    }
    return -1;
}

/* ---- Service lifecycle ---- */
static int exit_code(int status) { return WIFEXITED(status) ? WEXITSTATUS(status) : 128 + WTERMSIG(status); }
/* Allocate each service's persistent writable layer in its own sized tmpfs.
 * Capacity exhaustion produces ENOSPC for that service, while physical memory
 * remains shared. Mount in the supervisor so the layer survives restarts. */
static void setup_storage(void) {
#ifndef SEEDBOX_SUPERVISOR_TEST
    for (uint32_t i = 0; i < service_count; i++) {
        char path[PATH_MAX], options[64];
        snprintf(path, sizeof(path), "/seedbox/services/%s", services[i].name);
        snprintf(options, sizeof(options), "size=%llu,mode=0755", (unsigned long long)disk_mb * 1024 * 1024);
        directories(path, 0755);
        if (mount("tmpfs", path, "tmpfs", MS_NOSUID|MS_NODEV, options)) die("service disk");
    }
#endif
}
/* Mount, detach and prepare the image root inside the service namespaces.
 * Paths resolve inside the image, so its absolute symlinks stay inside it. */
static void enter_root(struct service *s) {
#ifdef SEEDBOX_SUPERVISOR_TEST
    (void)s;
#else
    int control = -1;
    if (s->controller && control_listener >= 0) {
        control = open(CONTROL_DIRECTORY, O_PATH|O_DIRECTORY|O_CLOEXEC);
        if (control < 0) die("control socket directory");
    }
    char root[PATH_MAX], image[PATH_MAX], disk[PATH_MAX], source[PATH_MAX], upper[PATH_MAX], work[PATH_MAX];
    snprintf(root, sizeof(root), "/services/%s", s->name);
    snprintf(image, sizeof(image), "/services/%s/images", s->name);
    snprintf(disk, sizeof(disk), "/services/%s/disk", s->name);
    snprintf(upper, sizeof(upper), "/seedbox/services/%s/upper", s->name);
    snprintf(work, sizeof(work), "/seedbox/services/%s/work", s->name);
    if (mount(NULL, "/", NULL, MS_REC|MS_PRIVATE, NULL)) die("private service mounts");
    /* The writable layer persists across restarts; the image never changes. */
    directories(upper, 0755); directories(work, 0755);
    /* pivot_root cannot detach initramfs rootfs. Switch this child into a
     * normal temporary root first, binding only its image and persistent
     * disk. Moving that mount over / covers the original namespace root;
     * merely chrooting at /services/NAME would leave bootstrap ancestors. */
    if (mount("tmpfs", root, "tmpfs", MS_NOSUID|MS_NODEV, "size=1m,mode=0755")) die("service bootstrap root");
    directories(image, 0755); directories(disk, 0755);
    snprintf(source, sizeof(source), "/images/%s", s->image);
    if (mount(source, image, NULL, MS_BIND, NULL) || mount(NULL, image, NULL, MS_BIND|MS_REMOUNT|MS_RDONLY|MS_NOSUID|MS_NODEV, NULL)) die("read-only service image");
    snprintf(source, sizeof(source), "/seedbox/services/%s", s->name);
    if (mount(source, disk, NULL, MS_BIND, NULL)) die("persistent service disk");
    if (chdir(root) || mount(root, "/", NULL, MS_MOVE, NULL) || chroot(".") || chdir("/")) die("switch service bootstrap root");
    directories("/root", 0755);
    if (mount("overlay", "/root", "overlay", 0, "lowerdir=/images,upperdir=/disk/upper,workdir=/disk/work")) die("service overlay");
    if (chdir("/root") || syscall(SYS_pivot_root, ".", ".") || umount2(".", MNT_DETACH) || chdir("/")) die("detach image root");
    directories("/dev", 0755); directories("/proc", 0755); directories("/sys", 0755); directories("/tmp", 01777);
    /* devtmpfs is global to the kernel and contains the management port.
     * Give applications only ordinary pseudo-devices in a private /dev. */
    if (mount("tmpfs", "/dev", "tmpfs", MS_NOSUID, "size=1m,mode=0755") || mount("proc", "/proc", "proc", MS_NOSUID|MS_NODEV|MS_NOEXEC, NULL) || mount("sysfs", "/sys", "sysfs", MS_RDONLY|MS_NOSUID|MS_NODEV|MS_NOEXEC, NULL)) die("mount guest filesystems");
    const struct { const char *path; unsigned int major, minor; } devices[] = {
        {"/dev/null", 1, 3}, {"/dev/zero", 1, 5}, {"/dev/full", 1, 7},
        {"/dev/random", 1, 8}, {"/dev/urandom", 1, 9}, {"/dev/tty", 5, 0}
    };
    for (size_t i = 0; i < sizeof(devices)/sizeof(devices[0]); i++) {
        if (mknod(devices[i].path, S_IFCHR|0666, makedev(devices[i].major, devices[i].minor)) || chmod(devices[i].path, 0666)) die("service device");
    }
    if (sethostname(s->name, strlen(s->name))) die("service hostname");
    write_file("/etc/hosts", hosts, strlen(hosts), 0644);
    for (char **volume = s->volumes; *volume; volume++) directories(*volume, 0755);
    directories(s->working_dir, 0755);
    for (uint32_t i = 0; i < s->configs; i++) {
        char *path = s->config_paths[i];
        write_file(path, s->config_contents[i], s->config_sizes[i], 0444);
        if (mount(path, path, NULL, MS_BIND, NULL) || mount(NULL, path, NULL, MS_BIND|MS_REMOUNT|MS_RDONLY, NULL)) die("read-only config mount");
    }
    if (control >= 0) {
        char source[64];
        snprintf(source, sizeof(source), "/proc/self/fd/%d", control);
        directories("/run/seedbox", 0755);
        if (mount(source, "/run/seedbox", NULL, MS_BIND, NULL)) die("control socket mount");
        close(control);
    }
#endif
}
/* Root applications must not recreate the management device, mount the old
 * root/devtmpfs, access effect I/O ports, reboot, or change the shared clock.
 * Drop the bounding set too: exec by UID 0 otherwise restores capabilities. */
static void application_capabilities(void) {
#ifndef SEEDBOX_SUPERVISOR_TEST
    const int denied[] = {CAP_SYS_ADMIN, CAP_MKNOD, CAP_SYS_RAWIO, CAP_SYS_BOOT, CAP_SYS_MODULE, CAP_SYS_TIME, CAP_SYS_PTRACE};
    struct __user_cap_header_struct header = {.version = _LINUX_CAPABILITY_VERSION_3};
    struct __user_cap_data_struct data[2];
    if (syscall(SYS_capget, &header, data)) die("read application capabilities");
    for (size_t i = 0; i < sizeof(denied)/sizeof(denied[0]); i++) {
        unsigned int word = (unsigned int)denied[i] / 32, bit = (unsigned int)denied[i] % 32;
        uint32_t mask = ~(1U << bit);
        if (prctl(PR_CAPBSET_DROP, denied[i], 0, 0, 0)) die("drop application bounding capability");
        data[word].effective &= mask; data[word].permitted &= mask; data[word].inheritable &= mask;
    }
    if (syscall(SYS_capset, &header, data) || prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0)) die("restrict application capabilities");
#endif
}
/* The service init: PID 1 of the service's PID namespace. It launches the
 * application and exits with its status; the kernel then kills every other
 * process in the namespace before this init can be reaped. */
static int service_main(void *argument) {
    struct service *s = argument;
    supervisor = 0;
    sigset_t none; sigemptyset(&none);
    if (sigprocmask(SIG_SETMASK, &none, NULL) || signal(SIGPIPE, SIG_DFL) == SIG_ERR || signal(SIGCHLD, SIG_DFL) == SIG_ERR) die("service signals");
    if (s->netns >= 0 && (setns(s->netns, CLONE_NEWNET) || close(s->netns))) die("join service network namespace");
    if (setpgid(0, 0)) die("service process group");
    enter_root(s);
    /* The bootstrap /dev/null descriptor pins devtmpfs outside the detached
     * root and could supply a mount to open_by_handle_at. Replace it with
     * the service's private device before the application inherits stdin. */
    int input = open("/dev/null", O_RDONLY|O_CLOEXEC);
    if (input < 0 || dup2(input, STDIN_FILENO) < 0 || close(input)) die("private service stdin");
#ifndef SEEDBOX_SUPERVISOR_TEST
    /* Restarts clone after the supervisor grants its effect ports. Linux
     * preserves that I/O bitmap across fork/exec even after RAWIO is dropped. */
    if (ioperm(0, 65536, 0)) die("clear service I/O permissions");
    if (prctl(PR_SET_DUMPABLE, 0, 0, 0, 0)) die("protect service init");
#endif
    pid_t application = fork();
    if (application < 0) die("fork application");
    if (!application) {
        if (chdir(s->working_dir)) die("working directory");
        application_capabilities();
        credentials(s->user);
        environ = s->environment;
        execvp(s->arguments[0], s->arguments);
        die("exec application");
    }
    close(launch_error_fd); launch_error_fd = -1;
    for (;;) {
        int status;
        pid_t done = waitpid(-1, &status, 0);
        if (done == application) _exit(exit_code(status));
        if (done < 0 && errno != EINTR) _exit(125);
    }
}
struct launch { struct service *service; int output, error; };
/* Arrange the init's descriptors: stdin /dev/null, stdout/stderr the relay,
 * fd 3 the close-on-exec launch error pipe, fd 4 its network namespace (if
 * any). Close everything else inherited from the supervisor. */
static int service_child(void *argument) {
    struct launch *launch = argument;
    struct service *s = launch->service;
    supervisor = 0;
    int input = open("/dev/null", O_RDONLY|O_CLOEXEC);
    int output = fcntl(launch->output, F_DUPFD_CLOEXEC, 10), error = fcntl(launch->error, F_DUPFD_CLOEXEC, 10);
    int netns = s->netns >= 0 ? fcntl(s->netns, F_DUPFD_CLOEXEC, 10) : -1;
    if (input < 0 || output < 0 || error < 0 || (s->netns >= 0 && netns < 0)) die("service descriptors");
    if (dup2(input, 0) < 0 || dup2(output, 1) < 0 || dup2(output, 2) < 0 || dup3(error, 3, O_CLOEXEC) < 0) die("service descriptors");
    launch_error_fd = 3;
    if (netns >= 0) {
        if (dup3(netns, 4, O_CLOEXEC) < 0) die("service network descriptor");
        s->netns = 4;
    }
    if (syscall(SYS_close_range, s->netns >= 0 ? 5 : 4, ~0U, 0)) die("close inherited descriptors");
    return service_main(s);
}
/* Without CLONE_VM the child runs on its own copy of this stack. */
static char service_stack[64 * 1024] __attribute__((aligned(16)));
static void relay_drain(struct service *s);
static int start(struct service *s) {
    int launch_error[2], output[2];
    if (pipe2(launch_error, O_CLOEXEC) || pipe2(output, O_CLOEXEC)) die("service pipes");
    struct launch launch = {.service = s, .output = output[1], .error = launch_error[1]};
    pid_t pid = clone(service_child, service_stack + sizeof(service_stack), SERVICE_NAMESPACES|SIGCHLD, &launch);
    if (pid < 0) die("clone service");
    close(launch_error[1]); close(output[1]);
    if (fcntl(output[0], F_SETFL, O_NONBLOCK)) die("service relay");
    if (s->output >= 0) { relay_drain(s); if (s->output >= 0) close(s->output); }
    s->output = output[0]; s->used = 0;
    char error; ssize_t received;
    do { received = read(launch_error[0], &error, 1); } while (received < 0 && errno == EINTR);
    close(launch_error[0]);
    if (received != 0) {
        int status; while (waitpid(pid, &status, 0) < 0 && errno == EINTR) {}
        relay_drain(s);
        s->state = EXITED;
        errno = EIO; return -1;
    }
    s->init = pid; s->state = RUNNING;
    return 0;
}
static int stop(struct service *s) {
    if (s->state == EXITED) { s->state = STOPPED; return 0; }
    if (s->state != RUNNING) { errno = EINVAL; return -1; }
    pid_t group = s->init;
    if (kill(-group, SIGKILL) && errno != ESRCH) return -1;
    int status;
    while (waitpid(group, &status, 0) < 0) { if (errno != EINTR) return -1; }
    s->init = -1; s->state = STOPPED;
    /* Without PID namespaces (tests), reap adopted members of the group. */
    while (waitpid(-group, &status, 0) >= 0 || errno == EINTR) {}
    int result = errno == ECHILD ? 0 : -1;
    relay_drain(s);
    return result;
}

/* ---- Output relay: one console line per service line ---- */
static void console_write(const char *bytes, size_t size) {
    while (size) {
        ssize_t n = write(STDOUT_FILENO, bytes, size);
        if (n < 0 && errno == EINTR) continue;
        if (n <= 0) return; /* Console loss cannot stop the applications. */
        bytes += n; size -= n;
    }
}
static void relay_line(struct service *s) {
    console_write(s->name, strlen(s->name));
    console_write(" | ", 3);
    console_write(s->line, s->used);
    if (!s->used || s->line[s->used - 1] != '\n') console_write("\n", 1);
    s->used = 0;
}
/* Returns 0 at end of file. A lone service's output passes through unchanged. */
static int relay_output(struct service *s) {
    char bytes[4096];
    for (;;) {
        ssize_t n = read(s->output, bytes, sizeof(bytes));
        if (n < 0 && errno == EINTR) continue;
        if (n < 0 && (errno == EAGAIN || errno == EWOULDBLOCK)) return 1;
        if (n <= 0) {
            if (s->used) relay_line(s);
            close(s->output); s->output = -1;
            return 0;
        }
        if (service_count == 1) { console_write(bytes, (size_t)n); continue; }
        for (ssize_t i = 0; i < n; i++) {
            s->line[s->used++] = bytes[i];
            if (bytes[i] == '\n' || s->used == sizeof(s->line)) relay_line(s);
        }
    }
}
static void relay_drain(struct service *s) { if (s->output >= 0) relay_output(s); }

/* ---- Management commands and acknowledgements ---- */
/* Test builds replace only these privileged I/O operations; the management
 * channel and initial application launch still use the production code. */
#ifndef SEEDBOX_READY_IOPERM
#define SEEDBOX_READY_IOPERM ioperm
#define SEEDBOX_READY_OUTL outl
#endif
static void service_channels(int timeout);
static void management_ready(void) {
#ifdef SEEDBOX_TEST_READY_DELAY_NS
    /* Acceptance fixture only: retained per-service opt-in prevents delaying
     * the controller or other applications that share this supervisor. */
    for (uint32_t i = 0; i < service_count; i++) {
        for (char **entry = services[i].environment; *entry; entry++) {
            if (strcmp(*entry, "SEEDBOX_TEST_DELAY_READY=1")) continue;
            struct timespec t;
            if (clock_gettime(CLOCK_MONOTONIC, &t)) die("test readiness clock");
            uint64_t deadline = (uint64_t)t.tv_sec * 1000000000ULL + t.tv_nsec + SEEDBOX_TEST_READY_DELAY_NS;
            for (;;) {
                if (clock_gettime(CLOCK_MONOTONIC, &t)) die("test readiness clock");
                uint64_t now = (uint64_t)t.tv_sec * 1000000000ULL + t.tv_nsec;
                if (now >= deadline) break;
                /* Keep relay requests flowing on the shared machine while
                 * effects wait for its readiness handshake in the VMM. */
                service_channels((int)((deadline - now + 999999) / 1000000));
            }
            goto ready;
        }
    }
ready:
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
    if (ioperm(SEEDBOX_EFFECT_PORT, SEEDBOX_EFFECT_PORT_COUNT, 1)) die("fault effect port");
    outl(id, success ? SEEDBOX_EFFECT_PORT : SEEDBOX_FAILURE_PORT);
#endif
}
static void control(char *command) {
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
    char *action, *args[SEEDBOX_COMMAND_ARGS]; uint32_t id; int count;
    if (control_parse(command, &action, &id, args, &count)) { errno = EINVAL; die("invalid management command"); }
    int success = 0;
    if (!strcmp(action, "stop") || !strcmp(action, "start")) {
        struct service *s = find_service(args[0]);
        if (!s) { errno = EINVAL; die("unknown management service"); }
        if (!strcmp(action, "stop")) success = stop(s) == 0;
        else if (s->state != RUNNING) success = start(s) == 0;
    } else if (!strcmp(action, "partition")) success = partition(args[0], args[1], args[2]) == 0;
    else success = heal(args[0]) == 0;
    acknowledge(action, id, success);
}
static char command_frame[RPC_FRAME_MAX+64];
static size_t command_used;
static void relay_management(void) {
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
                control(command_frame);
            } else if (command_used + 1 < sizeof(command_frame)) command_frame[command_used++] = byte;
            else { errno = E2BIG; die("management command"); }
        }
    }
}
/* A machine completes when its check service exits, or, without one, when
 * all of its services have exited on their own. */
static void reap(void) {
    for (;;) {
        int status;
        pid_t done = waitpid(-1, &status, WNOHANG);
        if (done < 0 && errno == EINTR) continue;
        if (done <= 0) return;
        struct service *s = NULL;
        for (uint32_t i = 0; i < service_count; i++) if (services[i].init == done) s = &services[i];
        if (!s) continue;
        s->init = -1; s->state = EXITED;
        relay_drain(s);
        int code = exit_code(status), any_check = 0, all_exited = 1;
        for (uint32_t i = 0; i < service_count; i++) { any_check |= services[i].check; all_exited &= services[i].state == EXITED; }
        if (s->check || (!any_check && all_exited)) finish(code, 0);
        char line[128];
        int length = snprintf(line, sizeof(line), "seedbox: service %s exited with status %d\n", s->name, code);
        if (length > 0) console_write(line, (size_t)length < sizeof(line) ? (size_t)length : sizeof(line) - 1);
        /* The engine tracks the exit for inspection and restart admission. */
        length = snprintf(line, sizeof(line), "SBX_EXIT1 %s %d\n", s->name, code);
        if (length < 0 || (size_t)length >= sizeof(line)) { errno = E2BIG; die("exit notice"); }
        if (management >= 0) send_all(management, line, (size_t)length);
    }
}
/* Handle only descriptors that poll reports ready. In particular an idle
 * disconnect releases the current session; a partial or outstanding RPC
 * still fails closed. The listener stays open for a later connection. */
static void service_channels(int timeout) {
    struct pollfd pfds[4 + MAX_SERVICES] = {
        {.fd = children, .events = POLLIN},
        {.fd = management, .events = POLLIN},
        {.fd = control_client < 0 ? control_listener : -1, .events = POLLIN},
        {.fd = control_client, .events = POLLIN}
    };
    for (uint32_t i = 0; i < service_count; i++) pfds[4 + i] = (struct pollfd){.fd = services[i].output, .events = POLLIN};
    int ready = poll(pfds, 4 + service_count, timeout);
    if (ready < 0) { if (errno != EINTR) die("poll management"); return; }
    if (!ready) return;
    for (uint32_t i = 0; i < service_count; i++) if (pfds[4 + i].revents) relay_drain(&services[i]);
    if (pfds[0].revents & POLLIN) {
        struct signalfd_siginfo info;
        while (read(children, &info, sizeof(info)) > 0) {}
        reap();
    }
    if (pfds[2].revents & (POLLERR|POLLHUP|POLLNVAL)) { errno = EIO; die("control listener poll"); }
    if (pfds[2].revents & POLLIN) relay_accept();
    if (pfds[3].revents & (POLLERR|POLLNVAL)) { errno = EIO; die("control client poll"); }
    if (pfds[3].revents & (POLLIN|POLLHUP)) relay_requests();
    if (pfds[1].revents & (POLLERR|POLLNVAL)) { errno = EIO; die("management port poll"); }
    if (pfds[1].revents & (POLLIN|POLLHUP)) relay_management();
}
static void watch_children(void) {
    sigset_t mask; sigemptyset(&mask); sigaddset(&mask, SIGCHLD);
    if (sigprocmask(SIG_BLOCK, &mask, NULL)) die("block SIGCHLD");
    children = signalfd(-1, &mask, SFD_NONBLOCK|SFD_CLOEXEC);
    if (children < 0) die("child signal descriptor");
}
#ifndef SEEDBOX_SUPERVISOR_TEST
int main(void) {
    if (signal(SIGPIPE,SIG_IGN)==SIG_ERR) die("ignore relay SIGPIPE");
    directories("/dev", 0755); directories("/proc", 0755); directories("/sys", 0755);
    if (mount("devtmpfs", "/dev", "devtmpfs", 0, NULL)) die("mount bootstrap dev");
    int console = open("/dev/console", O_RDWR);
    if (console >= 0) { dup2(console, 0); dup2(console, 1); dup2(console, 2); if (console > 2) close(console); }
    if (mount("proc", "/proc", "proc", 0, NULL) || mount("sysfs", "/sys", "sysfs", 0, NULL)) die("mount bootstrap filesystems");
    watch_children();
    read_recipe("/seedbox/launch.bin");
    setup_network();
    setup_storage();
    management = open_management("/sys/class/virtio-ports", "/dev");
    for (uint32_t i = 0; i < service_count; i++) {
        if (services[i].controller) relay_socket("/proc/cmdline", CONTROL_SOCKET);
    }
    for (uint32_t i = 0; i < service_count; i++) {
        if (start(&services[i])) die("initial application launch failed");
    }
    management_ready();
    for (;;) service_channels(-1);
}
#endif
