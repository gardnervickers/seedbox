/* Thin access to the actual guest C components. Assertions and scenarios live
 * in Rust; each fixture loads a private copy of these component globals. */
#define _GNU_SOURCE
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/wait.h>
#include "control.h"
#include "application-output.h"

int64_t sb_abi(unsigned field) {
    const int64_t values[] = {SEEDBOX_READY_ID, SEEDBOX_EFFECT_PORT,
        SEEDBOX_FAILURE_PORT, SEEDBOX_EFFECT_PORT_COUNT, sizeof(uint32_t)};
    return field < sizeof(values)/sizeof(*values) ? values[field] : -1;
}
int sb_parse(const char *text, char *action, uint32_t *id, int64_t *value) {
    char *copy = strdup(text), *parsed = NULL;
    if (!copy) return -1;
    int status = control_parse(copy, &parsed, id, value);
    if (!status) strcpy(action, parsed);
    free(copy);
    return status;
}
static struct application_output captured_output = {-1, -1};
ssize_t sb_frame(unsigned stream, size_t limit, char *frame) {
    return application_output_frame(&captured_output, stream, limit, frame);
}

#ifdef __linux__
#define SEEDBOX_TEST_ARM_IO
#include "io.h"
#include <sys/resource.h>
#include <sys/prctl.h>
#include <sys/timex.h>
static int fixture_execv(const char *, char *const []);
static int fixture_setrlimit(int, const struct rlimit *);
static int fixture_ioperm(unsigned long, unsigned long, int);
static void fixture_outl(uint32_t, unsigned short);
static int fixture_adjtimex(struct timex *);
#define execv fixture_execv
#define setrlimit fixture_setrlimit
#define SEEDBOX_ADJTIMEX fixture_adjtimex
#define SEEDBOX_READY_IOPERM fixture_ioperm
#define SEEDBOX_READY_OUTL fixture_outl
#define SEEDBOX_TEST_READY_DELAY_NS 10000000ULL
#define SEEDBOX_SUPERVISOR_TEST
#include "compose-init.c"
#undef setrlimit
#undef execv

static int diagnostic = -1;
static pid_t fixture_child = -1;
static char *fixture_arguments[3];
static char *normal_environment[] = {"PATH=/bin:/usr/bin", NULL};
static char *delayed_environment[] = {"PATH=/bin:/usr/bin", "SEEDBOX_TEST_DELAY_READY=1", NULL};
static char *misleading_environment[] = {"seedbox.control=1", NULL};
static int64_t readiness[12], readiness_order;
static struct timex adjustment;
static int adjustment_count, adjustment_error;

static int fixture_execv(const char *path, char *const argv[]) {
    dprintf(diagnostic, "exec %s\n", path);
    size_t index = 0;
    for (; argv[index]; ++index) dprintf(diagnostic, "arg %s\n", argv[index]);
    for (index = 0; argv[index] && strcmp(argv[index], "--"); ++index) {}
    if (!argv[index] || !argv[index + 1]) { errno = EINVAL; return -1; }
    if (prctl(PR_SET_PDEATHSIG, SIGKILL)) return -1;
    return execv(argv[index + 1], argv + index + 1);
}
static int fixture_setrlimit(int resource, const struct rlimit *limit) {
    dprintf(diagnostic, "limit %d %llu %llu %d\n", resource,
        (unsigned long long)limit->rlim_cur, (unsigned long long)limit->rlim_max, launch_error_fd);
    struct rlimit allowed;
    if (getrlimit(resource, &allowed)) return -1;
    /* The host's inherited hard ceiling can be below the guest's request. */
    if (allowed.rlim_max < limit->rlim_max) {
        allowed.rlim_cur = allowed.rlim_max;
        return setrlimit(resource, &allowed);
    }
    return setrlimit(resource, limit);
}
static int fixture_ioperm(unsigned long first, unsigned long count, int enabled) {
    readiness[0] = first; readiness[1] = count; readiness[2] = enabled;
    readiness[3] = fcntl(management, F_GETFL);
    readiness[4] = fcntl(management, F_GETFD);
    readiness[5] = isatty(management);
    int status = 0;
    readiness[6] = waitpid(fixture_child, &status, WNOHANG);
    readiness[7] = ++readiness_order;
    return 0;
}
static void fixture_outl(uint32_t id, unsigned short port) {
    readiness[8] = id; readiness[9] = port; readiness[10] = ++readiness_order;
}
static int fixture_adjtimex(struct timex *value) {
    adjustment = *value; adjustment_count++;
    if (adjustment_error) { errno = adjustment_error; return -1; }
    return TIME_OK;
}
int sb_network(const char *cidr, uint32_t *ip, uint32_t *mask) {
    struct in_addr address, netmask;
    int status = network_address(cidr, &address, &netmask);
    if (!status) { *ip = ntohl(address.s_addr); *mask = ntohl(netmask.s_addr); }
    return status;
}
static int fork_call(const char *operation, const char *argument, int close_fd) {
    pid_t pid = fork();
    if (pid < 0) return -1;
    if (!pid) {
        alarm(10);
        if (!strcmp(operation, "control")) {
            char *command = strdup(argument);
            control(command, &fixture_child);
            _exit(0);
        }
        if (!strcmp(operation, "channels")) { service_channels(&fixture_child, 0); _exit(0); }
        if (close_fd >= 0) close(close_fd);
        watch_children();
        fixture_child = start();
        if (fixture_child < 0) _exit(127);
        supervise(&fixture_child);
        _exit(0);
    }
    int status;
    while (waitpid(pid, &status, 0) < 0) { if (errno != EINTR) return -1; }
    return status;
}
#endif

int64_t sb_call(const char *operation, const char *a, const char *b, int64_t value) {
    if (!strcmp(operation, "output_open")) return application_output_open(&captured_output);
    if (!strcmp(operation, "output_read")) return captured_output.read_fd;
    if (!strcmp(operation, "output_write")) return captured_output.write_fd;
    if (!strcmp(operation, "output_finish")) { int status = close(captured_output.write_fd); captured_output.write_fd = -1; return status; }
#ifdef __linux__
    if (!strcmp(operation, "io_retain")) return seedbox_retain_io(a);
    if (!strcmp(operation, "io_enable")) return ioperm(value, 4, 1);
    if (!strcmp(operation, "io_disable")) return ioperm(value, 4, 0);
    if (!strcmp(operation, "io_boot")) return seedbox_io_boot_fd;
    if (!strcmp(operation, "io_active")) return seedbox_io_fd;
    if (!strcmp(operation, "management")) { management = value; return 0; }
    if (!strcmp(operation, "diagnostic")) { diagnostic = value; return 0; }
    if (!strcmp(operation, "arguments")) {
        free(fixture_arguments[0]); free(fixture_arguments[1]);
        fixture_arguments[0] = strdup(a); fixture_arguments[1] = b ? strdup(b) : NULL;
        arguments = fixture_arguments;
        environment = normal_environment;
        user = ""; working_dir = "/"; instructions_per_progress_call = UINT32_MAX; progress_points_per_block = UINT32_MAX - 1;
        return 0;
    }
    if (!strcmp(operation, "start")) return fixture_child = start();
    if (!strcmp(operation, "application_read")) return application_outputs[value].read_fd;
    if (!strcmp(operation, "stop")) return stop(&fixture_child);
    if (!strcmp(operation, "child")) return fixture_child;
    if (!strcmp(operation, "relay_management")) { relay_management(&fixture_child); return 0; }
    if (!strcmp(operation, "command_used")) return command_used;
    if (!strcmp(operation, "control")) { char *command = strdup(a); control(command, &fixture_child); free(command); return 0; }
    if (!strcmp(operation, "find_management")) return find_management(a, b);
    if (!strcmp(operation, "open_management")) return open_management(a, b);
    if (!strcmp(operation, "ready")) {
        environment = delayed_environment; management_ready(); environment = normal_environment; return 0;
    }
    if (!strcmp(operation, "readiness")) return readiness[value];
    if (!strcmp(operation, "adjustment_error")) { adjustment_error = value; return 0; }
    if (!strcmp(operation, "adjustments")) return adjustment_count;
    if (!strcmp(operation, "clock_modes")) return adjustment.modes;
    if (!strcmp(operation, "clock_seconds")) return adjustment.time.tv_sec;
    if (!strcmp(operation, "clock_nanoseconds")) return adjustment.time.tv_usec;
    if (!strcmp(operation, "clock_tick")) return adjustment.tick;
    if (!strcmp(operation, "clock_frequency")) return adjustment.freq;
    if (!strcmp(operation, "relay_socket")) {
        environment = misleading_environment;
        relay_socket(a, b); return 0;
    }
    if (!strcmp(operation, "channels")) { service_channels(&fixture_child, 0); return 0; }
    if (!strcmp(operation, "listener")) return control_listener;
    if (!strcmp(operation, "set_listener")) { control_listener = value; return 0; }
    if (!strcmp(operation, "client")) return control_client;
    if (!strcmp(operation, "session")) return session_id;
    if (!strcmp(operation, "relay")) return relay_id;
    if (!strcmp(operation, "waiting")) return waiting_relay;
    if (!strcmp(operation, "set_client")) { control_client = value; request_used = 0; return 0; }
    if (!strcmp(operation, "set_waiting")) { waiting_relay = value; return 0; }
    if (!strcmp(operation, "fork_control")) return fork_call("control", a, -1);
    if (!strcmp(operation, "fork_channels")) return fork_call("channels", NULL, -1);
    if (!strcmp(operation, "fork_supervise")) return fork_call("supervise", NULL, value);
#endif
    if (!strcmp(operation, "cleanup")) {
        application_output_close(&captured_output);
#ifdef __linux__
        if (fixture_child > 0) stop(&fixture_child);
        for (unsigned stream = 0; stream < 2; ++stream) application_output_close(&application_outputs[stream]);
        if (control_client >= 0) close(control_client);
        if (control_listener >= 0) close(control_listener);
        if (seedbox_io_fd >= 0) close(seedbox_io_fd);
        if (seedbox_io_boot_fd >= 0) close(seedbox_io_boot_fd);
        free(fixture_arguments[0]); free(fixture_arguments[1]);
#endif
        return 0;
    }
    errno = EINVAL; return -1;
}
