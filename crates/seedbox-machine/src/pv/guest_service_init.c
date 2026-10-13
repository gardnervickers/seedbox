/* Shared static Linux PID 1. Retain the prepared recipe across application
 * Stop/Start; bootstrap assets are separate from the application filesystem. */
#define _GNU_SOURCE
#include "guest_service.h"
#include "guest_application.h"
#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mount.h>
#include <sys/reboot.h>
#include <sys/stat.h>
#include <sys/wait.h>
#include <unistd.h>

static int management = -1;
static int diagnostic = -1;
static const char *failure_phase = "initialize";
static const char *failure_path = "init";
static struct sbx_oracle oracle;
static sigset_t application_mask;
static void changed(int signal) { (void)signal; }
static int write_all(void *sink, const void *opaque, size_t length) {
    int fd = *(int *)sink;
    const unsigned char *bytes = opaque;
    while (length) {
        ssize_t count = write(fd, bytes, length);
        if (count < 0 && errno == EINTR) continue;
        if (count < 0 && (errno == EAGAIN || errno == EWOULDBLOCK)) {
            struct pollfd pollfd = {.fd = fd, .events = POLLOUT};
            int result;
            do { result = poll(&pollfd, 1, -1); } while (result < 0 && errno == EINTR);
            if (result < 0) return -1;
            if (pollfd.revents & (POLLHUP | POLLERR | POLLNVAL)) { errno = EPIPE; return -1; }
            continue;
        }
        if (count <= 0) { if (!count) errno = EPIPE; return -1; }
        bytes += count; length -= (size_t)count;
    }
    return 0;
}
__attribute__((noreturn)) static void poweroff(void) {
    sync();
    reboot(RB_POWER_OFF);
    for (;;) pause();
}
__attribute__((noreturn)) static void failed(void) {
    int error = errno;
    dprintf(STDERR_FILENO, "seedbox init: %s %s: %s\n", failure_phase, failure_path, strerror(error));
    char frame[64];
    int length = snprintf(frame, sizeof(frame), "SBX_ERROR1 errno=%d\n", error);
    if (length > 0 && (size_t)length < sizeof(frame)) {
        int fd = management >= 0 ? management : STDERR_FILENO;
        write_all(&fd, frame, (size_t)length);
    }
    poweroff();
}
static void mount_fs(const char *type, const char *path) {
    failure_phase = "mount"; failure_path = path;
    if (mkdir(path, 0755) && errno != EEXIST) failed();
    if (mount(type, path, type, 0, NULL) && errno != EBUSY) failed();
}
static void module(const char *name) {
    failure_phase = "load module"; failure_path = name;
    /* Alpine's reference guest has modules; ARM's private kernel has built-in
     * virtio. No runtime compiler or package manager is involved. */
    if (access("/bin/busybox", X_OK)) return;
    pid_t child = fork();
    if (child < 0) failed();
    if (!child) { execl("/bin/busybox", "busybox", "modprobe", name, (char *)NULL); _exit(127); }
    while (waitpid(child, NULL, 0) < 0) if (errno != EINTR) failed();
}
static int port(void) {
    failure_phase = "open management port"; failure_path = "seedbox-lifecycle";
    DIR *ports = opendir("/sys/class/virtio-ports");
    if (!ports) return -1;
    struct dirent *entry;
    int found = -1;
    while ((entry = readdir(ports))) {
        if (entry->d_name[0] == '.') continue;
        char path[512], name[64];
        int size = snprintf(path, sizeof(path), "/sys/class/virtio-ports/%s/name", entry->d_name);
        if (size < 0 || (size_t)size >= sizeof(path)) continue;
        int fd = open(path, O_RDONLY | O_CLOEXEC);
        if (fd < 0) continue;
        ssize_t count = read(fd, name, sizeof(name) - 1); close(fd);
        if (count <= 0) continue;
        name[count] = 0;
        if (strcmp(name, "seedbox-lifecycle\n") && strcmp(name, "seedbox-lifecycle")) continue;
        size = snprintf(path, sizeof(path), "/dev/%s", entry->d_name);
        if (size < 0 || (size_t)size >= sizeof(path)) continue;
        found = open(path, O_RDWR | O_NONBLOCK | O_CLOEXEC);
        if (found >= 0) break;
    }
    closedir(ports);
    return found;
}
static int launch(void *opaque, int error_fd) {
    (void)error_fd;
    struct sbx_application *application = opaque;
    if (sigprocmask(SIG_SETMASK, &application_mask, NULL)) {
        int error = errno;
        dprintf(diagnostic, "seedbox init: restore application signal mask: %s\n", strerror(error));
        return error;
    }
    if (management >= 0) close(management);
    sbx_oracle_close(&oracle);
    int error = sbx_application_exec(application);
    dprintf(diagnostic, "seedbox init: exec %s: %s\n", application->arguments[0], strerror(error));
    return error;
}
int main(void) {
    sbx_oracle_init(&oracle, write_all, &management);
    mount_fs("devtmpfs", "/dev");
    mount_fs("proc", "/proc");
    mount_fs("sysfs", "/sys");
    failure_phase = "open console"; failure_path = "/dev/console";
    int console = open("/dev/console", O_RDWR);
    if (console < 0) failed();
    for (int fd = 0; fd < 3; ++fd) if (dup2(console, fd) < 0) failed();
    if (console > 2) close(console);
    diagnostic = fcntl(STDERR_FILENO, F_DUPFD_CLOEXEC, 3);
    if (diagnostic < 0) failed();
    module("virtio_mmio"); module("virtio_console");
    while ((management = port()) < 0) usleep(1000);
    failure_phase = "configure supervision signals"; failure_path = "SIGCHLD/SIGPIPE";
    struct sigaction action = {.sa_handler = changed};
    sigemptyset(&action.sa_mask);
    if (sigaction(SIGCHLD, &action, NULL) || signal(SIGPIPE, SIG_IGN) == SIG_ERR) failed();
    sigset_t blocked;
    sigemptyset(&blocked); sigaddset(&blocked, SIGCHLD);
    if (sigprocmask(SIG_BLOCK, &blocked, &application_mask)) failed();
    /* Kernel arguments never select or modify the retained application recipe. */
    struct sbx_application application;
    failure_phase = "read launch recipe"; failure_path = "/seedbox/application.bin";
    if (sbx_application_load(&application, failure_path)) failed();
    failure_phase = "enter application root"; failure_path = "/rootfs";
    if (mount("/rootfs", "/rootfs", NULL, MS_BIND | MS_REC, NULL) ||
        chdir("/rootfs") || mount(".", "/", NULL, MS_MOVE, NULL) ||
        chroot(".") || chdir("/")) failed();
    mount_fs("devtmpfs", "/dev"); mount_fs("proc", "/proc"); mount_fs("sysfs", "/sys");
    if (mkdir("/tmp", 01777) && errno != EEXIST) failed();
    /* Image working directories are created before dropping credentials, as in
     * the existing launch path. Their components resolve inside the image root. */
    char *working = strdup(application.working_directory);
    if (!working) failed();
    for (char *part = working + 1; ; ++part) {
        if (*part && *part != '/') continue;
        char saved = *part; *part = 0;
        failure_phase = "prepare working directory"; failure_path = working;
        if (*working && mkdir(working, 0755) && errno != EEXIST) failed();
        *part = saved; if (!saved) break;
    }
    free(working);
    struct sbx_service service;
    sbx_service_init(&service, launch, &application, write_all, &management);
    /* Every machine exposes the simulation-control channel before app start. */
    failure_phase = "open control socket"; failure_path = "/run/seedbox/control.sock";
    if (mkdir("/run", 0755) && errno != EEXIST) failed();
    if (mkdir("/run/seedbox", 0755) && errno != EEXIST) failed();
    if (chmod("/run", 0755) || chmod("/run/seedbox", 0755)) failed();
    if (sbx_oracle_open(&oracle, "/run/seedbox/control.sock")) failed();
    service.oracle = &oracle;
    failure_phase = "launch prepared application"; failure_path = application.arguments[0];
    if (sbx_service_start(&service)) failed();
    int terminating = 0;
    for (;;) {
        int exit_code, result;
        failure_phase = "shutdown application"; failure_path = application.arguments[0];
        if (service.shutdown_requested && !terminating) {
            terminating = 1;
            if (service.supervisor.child > 0 &&
                kill(-service.supervisor.child, SIGTERM) && errno != ESRCH) failed();
        }
        /* Machine shutdown owns graceful cleanup, distinct from application
         * outcome. A stopped/TERM-ignoring child may wait indefinitely. */
        failure_phase = "collect application"; failure_path = application.arguments[0];
        result = terminating ? sbx_supervisor_collect(&service.supervisor, &exit_code,
                                                       sbx_service_log, &service)
                             : sbx_service_collect(&service, &exit_code);
        if (result < 0) failed();
        /* Natural application exit is an observation, not guest shutdown.
         * The retained recipe remains available for a later Start. */
        if (terminating && service.supervisor.child <= 0) poweroff();
        struct pollfd fds[5] = {
            {.fd = terminating ? -1 : management, .events = POLLIN},
            {.fd = service.supervisor.output[0], .events = POLLIN},
            {.fd = service.supervisor.output[1], .events = POLLIN},
            {.fd = terminating || oracle.client >= 0 ? -1 : oracle.listener, .events = POLLIN},
            {.fd = terminating ? -1 : oracle.client,
             .events = POLLIN | (oracle.reply_used ? POLLOUT : 0)}
        };
        failure_phase = "poll services"; failure_path = "management/output/control";
        result = ppoll(fds, 5, NULL, &application_mask);
        if (result < 0) { if (errno == EINTR) continue; failed(); }
        failure_phase = "accept control connection"; failure_path = "/run/seedbox/control.sock";
        if (fds[3].revents & (POLLERR | POLLHUP | POLLNVAL)) failed();
        if (fds[3].revents & POLLIN) if (sbx_oracle_accept(&oracle)) failed();
        failure_phase = "read control request";
        if (sbx_oracle_poll(&oracle, fds[4].revents)) failed();
        for (unsigned stream = 0; stream < 2; ++stream) {
            failure_phase = "relay application output"; failure_path = stream ? "stderr" : "stdout";
            if (fds[stream + 1].revents & (POLLERR | POLLNVAL)) failed();
            if (fds[stream + 1].revents & (POLLIN | POLLHUP))
                if (sbx_service_output(&service, stream)) failed();
        }
        failure_phase = "read management input"; failure_path = "seedbox-lifecycle";
        if (fds[0].revents & (POLLERR | POLLNVAL)) failed();
        if (fds[0].revents & (POLLIN | POLLHUP)) {
            unsigned char bytes[4096];
            ssize_t count = read(management, bytes, sizeof(bytes));
            if (count < 0 && (errno == EAGAIN || errno == EINTR)) continue;
            if (count <= 0) { errno = EPIPE; failed(); }
            failure_phase = "dispatch management command";
            if (sbx_service_feed(&service, bytes, (size_t)count)) failed();
        }
    }
}
