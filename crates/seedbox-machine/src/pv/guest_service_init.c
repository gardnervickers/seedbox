/* Shared static Linux PID 1. Retain the prepared recipe across application
 * Stop/Start; bootstrap assets are separate from the application filesystem. */
#define _GNU_SOURCE
#include "guest_service.h"
#include "guest_application.h"
#include "guest_network.h"
#include "guest_environment.h"
#include "guest_policy.h"
#include "guest_oom.h"
#include "guest_properties.h"
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
#include <time.h>
#include <unistd.h>

static int management = -1;
static int diagnostic = -1;
static int kernel_log = -1;
static const char *failure_phase = "initialize";
static const char *failure_path = "init";
static struct sbx_oracle oracle;
static struct sbx_properties properties;
static struct sbx_guest_environment environment;
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
/* The descriptor is opened before entering the image root. No guest timer or
 * host monitor is needed: kmsg wakes ppoll, and collect drains it after reaping.
 * Fail clearly on lost records rather than attributing an unobserved kill.
 * Kernel OOM victims and fatal-signal reports share this one reader. */
static int report_kernel_evidence(pid_t application) {
    char record[OOM_REPORT_RECORD_BYTES], frame[OOM_REPORT_FRAME_BYTES];
    char crash[SBX_CRASH_FRAME_BYTES];
    for (;;) {
        ssize_t count = read(kernel_log, record, sizeof(record));
        if (count < 0 && errno == EINTR) continue;
        if (count < 0 && (errno == EAGAIN || errno == EWOULDBLOCK)) return 0;
        if (count <= 0) { if (!count) errno = EIO; return -1; }
        size_t length = oom_report_frame(record, (size_t)count, application, frame);
        if (length && write_all(&management, frame, length)) return -1;
        length = sbx_crash_frame(record, (size_t)count, crash);
        if (length == (size_t)-1) {
            if (sbx_properties_lost(&properties, "crash report too long", 1)) return -1;
        } else if (length && write_all(&management, crash, length)) return -1;
        /* A program that reached neither guest socket reported its loss here. */
        uint64_t lost;
        char reason[SBX_PROPERTY_REASON_BYTES];
        if (sbx_guest_lost_line(record, (size_t)count, &lost, reason) &&
            sbx_properties_lost(&properties, reason, lost)) return -1;
    }
}
/* Before an outcome: the kernel's reports, then records the application sent. */
static int report_evidence(pid_t application) {
    return report_kernel_evidence(application) || sbx_properties_drain(&properties) ? -1 : 0;
}
__attribute__((noreturn)) static void poweroff(void) {
    sync();
    reboot(RB_POWER_OFF);
    for (;;) pause();
}
__attribute__((noreturn)) static void failed(void) {
    int error = errno;
    /* Idempotent after successful setup; also releases owned recipe storage
     * for root-transition/working-directory failures outside setup helpers.
     * failure_path's fixed environment buffer and scalar diagnostics survive. */
    sbx_env_release(&environment);
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
    if (kernel_log >= 0) close(kernel_log);
    sbx_oracle_close(&oracle);
    sbx_properties_close(&properties);
    if (sbx_env_application_limits(&environment)) {
        int error=errno;
        dprintf(diagnostic,"seedbox init: application nofile limit: %s\n",strerror(error));
        return error;
    }
    int error = sbx_application_exec(application);
    dprintf(diagnostic, "seedbox init: exec %s: %s\n", application->arguments[0], strerror(error));
    return error;
}
/* A networked machine's kernel command line carries its static IPv4 address
 * (guest_network.h). Its absence leaves every interface untouched. */
static void configure_network(void) {
    failure_phase = "configure network"; failure_path = "/proc/cmdline";
    char line[4096];
    int fd = open(failure_path, O_RDONLY | O_CLOEXEC);
    if (fd < 0) failed();
    size_t length = 0;
    for (;;) {
        ssize_t count = read(fd, line + length, sizeof(line) - 1 - length);
        if (count < 0 && errno == EINTR) continue;
        if (count < 0) { close(fd); failed(); }
        if (!count) break;
        length += (size_t)count;
        if (length == sizeof(line) - 1) { close(fd); errno = EINVAL; failed(); }
    }
    close(fd);
    line[length] = 0;
    struct sbx_ipv4 config;
    int found = sbx_network_parse(line, &config);
    if (found < 0) failed();
    if (!found) return;
    module("virtio_net");
    failure_phase = "configure network"; failure_path = "lo";
    if (sbx_network_up(failure_path)) failed();
    failure_path = "eth0";
    if (sbx_network_configure(&config, failure_path)) failed();
}
/* Machine counter in nanoseconds of this machine's time base. PV kernels
 * present the counter at 1 GHz (x86 RDTSC and arm64 CNTVCT are emulated). */
static int counter_ns(uint64_t *out) {
#if defined(__x86_64__)
    uint32_t lo, hi;
    __asm__ volatile("rdtsc" : "=a"(lo), "=d"(hi) :: "memory");
    *out = (uint64_t)hi << 32 | lo;
    return 0;
#elif defined(__aarch64__)
    uint64_t ticks, frequency;
    __asm__ volatile("isb; mrs %0, cntvct_el0" : "=r"(ticks) :: "memory");
    __asm__ volatile("mrs %0, cntfrq_el0" : "=r"(frequency));
    if (!frequency) { errno = EINVAL; return -1; }
    *out = (uint64_t)((unsigned __int128)ticks * 1000000000u / frequency);
    return 0;
#else
#error seedbox guest clock requires x86-64 or AArch64
#endif
}
/* Every boot starts CLOCK_REALTIME at the simulation epoch plus the machine
 * counter, before Ready. /seedbox/clock.bin is "SBXCLK1\0" followed by the
 * little-endian u32 epoch seconds; an archive without it uses epoch zero. */
static void set_realtime(void) {
    failure_phase = "read clock epoch"; failure_path = "/seedbox/clock.bin";
    uint32_t epoch = 0;
    int fd = open(failure_path, O_RDONLY | O_CLOEXEC);
    if (fd < 0 && errno != ENOENT) failed();
    if (fd >= 0) {
        unsigned char bytes[13];
        ssize_t count = read(fd, bytes, sizeof(bytes)); close(fd);
        if (count != 12 || memcmp(bytes, "SBXCLK1\0", 8)) { errno = EINVAL; failed(); }
        epoch = (uint32_t)bytes[8] | (uint32_t)bytes[9] << 8 |
                (uint32_t)bytes[10] << 16 | (uint32_t)bytes[11] << 24;
    }
    failure_phase = "set realtime"; failure_path = "CLOCK_REALTIME";
    uint64_t now;
    if (counter_ns(&now)) failed();
    struct timespec realtime = {
        .tv_sec = (time_t)((uint64_t)epoch + now / 1000000000u),
        .tv_nsec = (long)(now % 1000000000u),
    };
    if (clock_settime(CLOCK_REALTIME, &realtime)) failed();
}
/* The kernel reports each core-dumping signal a process does not handle
 * (seedbox.no_crashing_processes); the image never selects this. */
static void report_fatal_signals(void) {
    failure_phase = "report fatal signals"; failure_path = "/proc/sys/kernel/print-fatal-signals";
    int fd = open(failure_path, O_WRONLY | O_CLOEXEC);
    if (fd < 0) failed();
    if (write(fd, "1\n", 2) != 2) { if (!errno) errno = EIO; close(fd); failed(); }
    close(fd);
}
int main(void) {
    sbx_oracle_init(&oracle, write_all, &management);
    sbx_properties_init(&properties, write_all, &management);
    mount_fs("devtmpfs", "/dev");
    mount_fs("proc", "/proc");
    mount_fs("sysfs", "/sys");
    report_fatal_signals();
    failure_phase = "open console"; failure_path = "/dev/console";
    int console = open("/dev/console", O_RDWR);
    if (console < 0) failed();
    for (int fd = 0; fd < 3; ++fd) if (dup2(console, fd) < 0) failed();
    if (console > 2) close(console);
    diagnostic = fcntl(STDERR_FILENO, F_DUPFD_CLOEXEC, 3);
    if (diagnostic < 0) failed();
    failure_phase = "open kernel OOM log"; failure_path = "/dev/kmsg";
    kernel_log = open(failure_path, O_RDONLY | O_NONBLOCK | O_CLOEXEC);
    if (kernel_log < 0 || lseek(kernel_log, 0, SEEK_END) < 0) failed();
    module("virtio_mmio"); module("virtio_console");
    while ((management = port()) < 0) usleep(1000);
    failure_phase = "configure supervision signals"; failure_path = "SIGCHLD/SIGPIPE";
    struct sigaction action = {.sa_handler = changed};
    sigemptyset(&action.sa_mask);
    if (sigaction(SIGCHLD, &action, NULL) || signal(SIGPIPE, SIG_IGN) == SIG_ERR) failed();
    sigset_t blocked;
    sigemptyset(&blocked); sigaddset(&blocked, SIGCHLD);
    if (sigprocmask(SIG_BLOCK, &blocked, &application_mask)) failed();
    configure_network();
    /* Kernel arguments never select or modify the retained application recipe. */
    struct sbx_application application;
    failure_phase = "read launch recipe"; failure_path = "/seedbox/application.bin";
    if (sbx_application_load(&application, failure_path)) failed();
    failure_phase="read guest environment"; failure_path="/seedbox/environment.bin";
    if (sbx_env_load(&environment,failure_path)<0) failed();
    /* Read the explicit named factory selector while host-independent proc is
     * still available, before entering the retained application root. */
    int control_enabled = sbx_env_control_enabled(&environment);
    failure_phase = "read controller selector"; failure_path = "/proc/cmdline";
    FILE *command_file = fopen(failure_path, "re");
    if (!command_file) failed();
    char *command = NULL; size_t command_capacity = 0;
    ssize_t command_length = getline(&command, &command_capacity, command_file);
    if (command_length < 0) { if (!errno) errno = EIO; failed(); }
    if (fclose(command_file) || sbx_control_mode(command, control_enabled, &control_enabled)) failed();
    free(command);
    set_realtime();
    if (sbx_env_before_root(&environment,management)) {
        failure_phase=environment.failure_phase;failure_path=environment.failure_path;failed();
    }
    failure_phase = "enter application root"; failure_path = "/rootfs";
    if (mount("/rootfs", "/rootfs", NULL, MS_BIND | MS_REC, NULL) ||
        chdir("/rootfs") || mount(".", "/", NULL, MS_MOVE, NULL) ||
        chroot(".") || chdir("/")) failed();
    if (!environment.configured) mount_fs("devtmpfs", "/dev");
    mount_fs("proc", "/proc"); mount_fs("sysfs", "/sys");
    if (mkdir("/tmp", 01777) && errno != EEXIST) failed();
    if (sbx_env_after_root(&environment)) {
        failure_phase=environment.failure_phase;failure_path=environment.failure_path;failed();
    }
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
    /* Only configured scalar limits/socket state is needed by later Starts. */
    sbx_env_release(&environment);
    struct sbx_service service;
    sbx_service_init(&service, launch, &application, write_all, &management);
    /* Generic factories keep their all-machine channel. Configured service
     * guests explicitly choose the listener; host authorization is separate. */
    if (control_enabled) {
        failure_phase = "open control socket"; failure_path = "/run/seedbox/control.sock";
        if (mkdir("/run", 0755) && errno != EEXIST) failed();
        if (mkdir("/run/seedbox", 0755) && errno != EEXIST) failed();
        if (chmod("/run", 0755) || chmod("/run/seedbox", 0755)) failed();
        if (sbx_oracle_open(&oracle, "/run/seedbox/control.sock")) failed();
    }
    /* Every machine serves the guest property socket, whatever its image. */
    failure_phase = "open property socket"; failure_path = "/run/seedbox/guest.sock";
    if (mkdir("/run", 0755) && errno != EEXIST) failed();
    if (mkdir("/run/seedbox", 0755) && errno != EEXIST) failed();
    /* An image's own /run/seedbox must not keep programs out. */
    if (chmod("/run/seedbox", 0755)) failed();
    if (sbx_properties_open(&properties, failure_path)) failed();
    application.guest_socket = "/run/seedbox/guest.sock";
    service.oracle = &oracle;
    service.properties = &properties;
    service.before_outcome = report_evidence;
    failure_phase = "launch prepared application"; failure_path = application.arguments[0];
    if (sbx_service_start(&service)) failed();
    int terminating = 0;
    for (;;) {
        int exit_code, result;
        failure_phase = "read kernel log"; failure_path = "/dev/kmsg";
        if (report_kernel_evidence(service.supervisor.child)) failed();
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
        enum { SERVICES = 6, POLLED = SERVICES + SBX_PROPERTY_POLLED };
        struct pollfd fds[POLLED] = {
            {.fd = terminating ? -1 : management, .events = POLLIN},
            {.fd = service.supervisor.output[0], .events = POLLIN},
            {.fd = service.supervisor.output[1], .events = POLLIN},
            {.fd = terminating || oracle.client >= 0 ? -1 : oracle.listener, .events = POLLIN},
            {.fd = terminating ? -1 : oracle.client,
             .events = POLLIN | (oracle.reply_used ? POLLOUT : 0)},
            {.fd = kernel_log, .events = POLLIN}
        };
        for (unsigned i = SERVICES; i < POLLED; ++i)
            fds[i] = (struct pollfd){.fd = sbx_properties_fd(&properties, i - SERVICES), .events = POLLIN};
        failure_phase = "poll services"; failure_path = "management/output/control";
        result = ppoll(fds, POLLED, NULL, &application_mask);
        if (result < 0) { if (errno == EINTR) continue; failed(); }
        failure_phase = "read kernel log"; failure_path = "/dev/kmsg";
        if (report_kernel_evidence(service.supervisor.child)) failed();
        failure_phase = "relay property records"; failure_path = "/run/seedbox/guest.sock";
        {
            short revents[SBX_PROPERTY_POLLED];
            for (unsigned i = 0; i < SBX_PROPERTY_POLLED; ++i) revents[i] = fds[SERVICES + i].revents;
            if (sbx_properties_poll(&properties, revents)) failed();
        }
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
