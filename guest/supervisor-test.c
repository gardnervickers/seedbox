#define _GNU_SOURCE
#include <stdint.h>
#include <sys/resource.h>
static int application_setrlimit(int resource, const struct rlimit *limit);
#define setrlimit application_setrlimit
static int readiness_ioperm(unsigned long first, unsigned long count, int enabled);
static void readiness_outl(uint32_t id, unsigned short port);
#include <sys/timex.h>
static int test_adjtimex(struct timex *adjustment);
#define SEEDBOX_ADJTIMEX test_adjtimex
#define SEEDBOX_READY_IOPERM readiness_ioperm
#define SEEDBOX_READY_OUTL readiness_outl
#define SEEDBOX_TEST_READY_DELAY_NS 10000000ULL
#define SEEDBOX_SUPERVISOR_TEST
#include "compose-init.c"
#undef setrlimit
#include <assert.h>
#include <sys/socket.h>
static int application_setrlimit(int resource, const struct rlimit *limit) {
    assert(resource == RLIMIT_NOFILE && launch_error_fd >= 0);
    assert(limit->rlim_cur == 1048576 && limit->rlim_max == 1048576);
    /* An unprivileged host test cannot raise its inherited hard ceiling.
     * Check the requested default, then exercise inheritance within it. */
    struct rlimit allowed; assert(!getrlimit(resource, &allowed));
    if (allowed.rlim_max < limit->rlim_max) {
        allowed.rlim_cur = allowed.rlim_max;
        return setrlimit(resource, &allowed);
    }
    return setrlimit(resource, limit);
}
static int readiness_permissions, readiness_writes;
static pid_t readiness_child;
static struct timespec readiness_started;
static int readiness_ioperm(unsigned long first, unsigned long count, int enabled) {
    assert(first == SEEDBOX_EFFECT_PORT && count == SEEDBOX_EFFECT_PORT_COUNT && enabled == 1);
    assert(fcntl(management, F_GETFL) & O_NONBLOCK);
    assert(fcntl(management, F_GETFD) & FD_CLOEXEC);
    assert(!isatty(management));
    int status; assert(waitpid(readiness_child, &status, WNOHANG) == 0);
    struct timespec now; assert(!clock_gettime(CLOCK_MONOTONIC, &now));
    long long elapsed = (now.tv_sec - readiness_started.tv_sec)*1000000000LL + now.tv_nsec - readiness_started.tv_nsec;
    assert(elapsed >= (long long)SEEDBOX_TEST_READY_DELAY_NS);
    assert(!readiness_permissions && !readiness_writes);
    readiness_permissions = 1;
    return 0;
}
static void readiness_outl(uint32_t id, unsigned short port) {
    assert(readiness_permissions && !readiness_writes);
    assert(id == SEEDBOX_READY_ID && port == SEEDBOX_EFFECT_PORT);
    readiness_writes = 1;
}
static struct timex last_adjustment;
static int adjustments;
static int test_adjtimex(struct timex *adjustment) {
    last_adjustment = *adjustment; adjustments++;
    /* Model the kernel's refusal of a wall clock before boot. */
    if (adjustment->time.tv_sec < -1000000) { errno = EINVAL; return -1; }
    return TIME_OK;
}
static void test_cmdline(const char *path, const char *text) {
    int fd = open(path, O_WRONLY|O_CREAT|O_TRUNC, 0600); assert(fd >= 0);
    assert(write(fd, text, strlen(text)) == (ssize_t)strlen(text));
    assert(!close(fd));
}
int main(int argc, char **argv) {
    struct in_addr network_ip, network_mask;
    assert(!network_address("10.0.0.3/24", &network_ip, &network_mask));
    assert(ntohl(network_ip.s_addr) == 0x0a000003 && ntohl(network_mask.s_addr) == 0xffffff00);
    assert(!network_address("10.0.0.3/0", &network_ip, &network_mask) && network_mask.s_addr == 0);
    assert(!network_address("10.0.0.3/32", &network_ip, &network_mask) && network_mask.s_addr == UINT32_MAX);
    assert(network_address("10.0.0.3/33", &network_ip, &network_mask));
    assert(network_address("10.0.0.3/", &network_ip, &network_mask));
    assert(network_address("10.0.0.3/24junk", &network_ip, &network_mask));
    assert(network_address("bad/24", &network_ip, &network_mask));

    if (argc > 1) {
        /* Every exec, including a restart, inherits the runtime limit. */
        struct rlimit limit; assert(!getrlimit(RLIMIT_NOFILE, &limit));
        assert(limit.rlim_cur == limit.rlim_max && limit.rlim_max <= 1048576);
    }
    if (argc > 1 && !strcmp(argv[1], "exit-later")) {
        /* Launched by the supervisor: SIGCHLD must not stay blocked. */
        sigset_t mask; assert(!sigprocmask(SIG_SETMASK, NULL, &mask));
        if (sigismember(&mask, SIGCHLD)) _exit(3);
        struct timespec delay = {.tv_nsec = 100000000}; nanosleep(&delay, NULL);
        _exit(7);
    }
    if (argc > 1) { for (;;) pause(); }
    int sockets[2]; assert(!socketpair(AF_UNIX,SOCK_STREAM,0,sockets));
    assert(!fcntl(sockets[0], F_SETFL, O_NONBLOCK));
    assert(!fcntl(sockets[0], F_SETFD, FD_CLOEXEC));
    management = sockets[0];
    environment = (char *[]){"PATH=/bin:/usr/bin",NULL}; user=""; working_dir="/";
    arguments = (char *[]){argv[0],"child",NULL};
    pid_t child = start(); assert(child > 0); pid_t original = child;
    /* Named port discovery works with devtmpfs nodes and no udev symlink.
     * The ordinary POSIX open path uses a temporary device file in this test. */
    char discovery[] = "/tmp/seedbox-port-XXXXXX"; assert(mkdtemp(discovery));
    char classes[PATH_MAX], devices[PATH_MAX], port[PATH_MAX], attribute[PATH_MAX], node[PATH_MAX];
    assert(snprintf(classes, sizeof(classes), "%s/class", discovery) > 0);
    assert(snprintf(devices, sizeof(devices), "%s/dev", discovery) > 0);
    assert(!mkdir(classes, 0700) && !mkdir(devices, 0700));
    assert(snprintf(port, sizeof(port), "%s/class/vport3p7", discovery) > 0);
    assert(!mkdir(port, 0700));
    assert(snprintf(attribute, sizeof(attribute), "%s/class/vport3p7/name", discovery) > 0);
    int name = open(attribute, O_WRONLY|O_CREAT|O_TRUNC, 0600); assert(name >= 0);
    assert(write(name, "unrelated\n", 10) == 10); close(name);
    assert(find_management(classes, devices) == -1);
    name = open(attribute, O_WRONLY|O_TRUNC); assert(name >= 0);
    assert(write(name, "seedbox\n", 8) == 8); close(name);
    assert(snprintf(node, sizeof(node), "%s/dev/vport3p7", discovery) > 0);
    int device = open(node, O_RDWR|O_CREAT, 0600); assert(device >= 0); close(device);
    int discovered = open_management(classes, devices); assert(discovered >= 0);
    assert(fcntl(discovered, F_GETFL) & O_NONBLOCK);
    assert(fcntl(discovered, F_GETFD) & FD_CLOEXEC);
    close(discovered);
    assert(!unlink(node) && !unlink(attribute) && !rmdir(port));
    assert(!rmdir(classes) && !rmdir(devices) && !rmdir(discovery));
    /* Announce readiness only after channel opening, initial exec, and the
     * retained readiness delay. The wrappers check the complete port grant. */
    readiness_child = child;
    environment = (char *[]){"PATH=/bin:/usr/bin", "SEEDBOX_TEST_DELAY_READY=1", NULL};
    assert(!clock_gettime(CLOCK_MONOTONIC, &readiness_started));
    management_ready();
    environment = (char *[]){"PATH=/bin:/usr/bin", NULL};
    assert(readiness_permissions && readiness_writes);
    assert(write(sockets[1], "stop ", 5) == 5);
    relay_management(&child); assert(child == original && command_used == 5);
    assert(write(sockets[1], "1\n", 2) == 2);
    relay_management(&child); assert(child == -1 && !command_used);
    int status; assert(waitpid(original,&status,WNOHANG) == -1 && errno == ECHILD);
    char buffer[512]; ssize_t n = read(sockets[1],buffer,sizeof(buffer)-1); assert(n>0); buffer[n]=0;
    assert(strstr(buffer,"SEEDBOX_EFFECT id=1 action=stop guest_ns="));
    const char *commands = "stop 2\nstart 3\n";
    assert(write(sockets[1], commands, strlen(commands)) == (ssize_t)strlen(commands));
    relay_management(&child); assert(child > 0 && !command_used);
    n=read(sockets[1],buffer,sizeof(buffer)-1); assert(n>0); buffer[n]=0;
    assert(strstr(buffer,"SEEDBOX_FAILURE id=2") && strstr(buffer,"SEEDBOX_EFFECT id=3 action=start"));
    /* Process pause: the acknowledgement follows the group stop; resume
     * continues it, and a later stop still kills and reaps a frozen group. */
    const char *pauses[] = {"freeze 5\n", "thaw 6\n", "freeze 7\n"};
    const char *effects[] = {"SEEDBOX_EFFECT id=5 action=freeze ", "SEEDBOX_EFFECT id=6 action=thaw ",
                             "SEEDBOX_EFFECT id=7 action=freeze "};
    for (unsigned i = 0; i < 3; i++) {
        assert(write(sockets[1], pauses[i], strlen(pauses[i])) == (ssize_t)strlen(pauses[i]));
        relay_management(&child); assert(child > 0 && !command_used);
        n=read(sockets[1],buffer,sizeof(buffer)-1); assert(n>0); buffer[n]=0;
        assert(strstr(buffer, effects[i]));
        char path[64], stat[256]; snprintf(path, sizeof(path), "/proc/%d/stat", (int)child);
        int fd = open(path, O_RDONLY); assert(fd >= 0);
        ssize_t got = read(fd, stat, sizeof(stat)-1); assert(got > 0); stat[got] = 0; close(fd);
        char state = strrchr(stat, ')')[2];
        assert(i == 1 ? state != 'T' : state == 'T');
    }
    assert(waitpid(child,&status,WNOHANG) == 0);
    assert(!stop(&child));
    arguments=(char *[]){"/definitely/no/such/executable",NULL};
    char failed[]="start 4"; control(failed,&child); assert(child<0);
    n=read(sockets[1],buffer,sizeof(buffer)-1); assert(n>0); buffer[n]=0; assert(strstr(buffer,"SEEDBOX_FAILURE id=4"));
    assert(waitpid(-1,&status,WNOHANG)==-1 && errno==ECHILD);
    /* A clock step is one ADJ_SETOFFSET with a normalized nanosecond field,
     * acknowledged like a process effect; it leaves the process alone. */
    struct { const char *command; long long sec, nsec; } clock_steps[] = {
        {"clock_step 5 1500000001", 1, 500000001}, {"clock_step 6 -1", -1, 999999999},
        {"clock_step 7 -2000000000", -2, 0}, {"clock_step 8 0", 0, 0}};
    for (unsigned i = 0; i < sizeof(clock_steps)/sizeof(*clock_steps); i++) {
        char command[64]; strcpy(command, clock_steps[i].command);
        control(command, &child); assert(child < 0 && adjustments == (int)i + 1);
        assert(last_adjustment.modes == (ADJ_SETOFFSET | ADJ_NANO));
        assert(last_adjustment.time.tv_sec == clock_steps[i].sec && last_adjustment.time.tv_usec == clock_steps[i].nsec);
        n=read(sockets[1],buffer,sizeof(buffer)-1); assert(n>0); buffer[n]=0;
        char expected[64]; snprintf(expected, sizeof(expected), "SEEDBOX_EFFECT id=%u action=clock_step", 5 + i);
        assert(strstr(buffer, expected));
    }
    char rejected[] = "clock_step 9 -9000000000000000"; control(rejected, &child);
    n=read(sockets[1],buffer,sizeof(buffer)-1); assert(n>0); buffer[n]=0; assert(strstr(buffer,"SEEDBOX_FAILURE id=9 action=clock_step"));
    /* The public RPC socket is created only from an exact authoritative boot
     * token; image environment entries cannot enable it on other guests. */
    char directory[] = "/tmp/seedbox-relay-XXXXXX"; assert(mkdtemp(directory));
    char cmdline_path[PATH_MAX];
    assert(snprintf(cmdline_path, sizeof(cmdline_path), "%s/cmdline", directory) > 0);
    struct sockaddr_un address = {.sun_family = AF_UNIX};
    assert(snprintf(address.sun_path, sizeof(address.sun_path), "%s/control.sock", directory) > 0);
    const char *disabled[] = {"quiet\n", "seedbox.control=0\n", "seedbox.control=11\n", "xseedbox.control=1\n", "app=seedbox.control=1\n"};
    environment = (char *[]){"seedbox.control=1", NULL};
    for (unsigned i = 0; i < sizeof(disabled)/sizeof(*disabled); i++) {
        test_cmdline(cmdline_path, disabled[i]);
        relay_socket(cmdline_path, address.sun_path);
        assert(control_listener == -1 && access(address.sun_path, F_OK) == -1 && errno == ENOENT);
    }
    const char *enabled = "quiet\tseedbox.control=1 root=/dev/ram0\n";
    test_cmdline(cmdline_path, enabled);
    relay_socket(cmdline_path, address.sun_path); assert(control_listener >= 0);
    struct stat permissions; assert(!stat(address.sun_path, &permissions)); assert((permissions.st_mode & 0777) == 0666);
    int peer = socket(AF_UNIX, SOCK_STREAM, 0); assert(peer >= 0);
    assert(!connect(peer, (struct sockaddr *)&address, sizeof(address)));
    service_channels(&child, 0); assert(control_client >= 0 && session_id == 1);
    const char *request = "{\"version\":1,\"id\":1,\"op\":\"now\"}";
    assert(write(peer, request, strlen(request)) == (ssize_t)strlen(request));
    service_channels(&child, 0); assert(!waiting_relay);
    assert(write(peer, "\n", 1) == 1); service_channels(&child, 0); assert(waiting_relay == 1);
    n = read(sockets[1], buffer, sizeof(buffer)-1); assert(n > 0); buffer[n] = 0;
    assert(!strcmp(buffer, "SBX_RPC1 1 1 {\"version\":1,\"id\":1,\"op\":\"now\"}\n"));
    char reply[] = "SBX_REPLY1 1 1 {\"version\":1,\"id\":1,\"ok\":true}";
    assert(write(sockets[1], reply, 8) == 8); service_channels(&child, 0); assert(waiting_relay);
    assert(write(sockets[1], reply + 8, strlen(reply) - 8) == (ssize_t)strlen(reply) - 8);
    assert(write(sockets[1], "\n", 1) == 1); service_channels(&child, 0); assert(!waiting_relay);
    n = read(peer, buffer, sizeof(buffer)-1); assert(n > 0); buffer[n] = 0;
    assert(!strcmp(buffer, "{\"version\":1,\"id\":1,\"ok\":true}\n"));
    /* An idle HUP releases only the client; the same listener accepts a new
     * session. Empty sessions consume a session ID but no relay ID. */
    int listener = control_listener;
    close(peer); service_channels(&child, 0); assert(control_client == -1 && control_listener == listener);
    peer = socket(AF_UNIX, SOCK_STREAM, 0); assert(peer >= 0);
    assert(!connect(peer, (struct sockaddr *)&address, sizeof(address)));
    service_channels(&child, 0); assert(session_id == 2);
    close(peer); service_channels(&child, 0); assert(control_client == -1 && relay_id == 1);
    peer = socket(AF_UNIX, SOCK_STREAM, 0); assert(peer >= 0);
    assert(!connect(peer, (struct sockaddr *)&address, sizeof(address)));
    service_channels(&child, 0); assert(session_id == 3);
    assert(write(peer, request, strlen(request)) == (ssize_t)strlen(request)); assert(write(peer, "\n", 1) == 1);
    service_channels(&child, 0); assert(waiting_relay == 2);
    n = read(sockets[1], buffer, sizeof(buffer)-1); assert(n > 0); buffer[n] = 0;
    assert(!strcmp(buffer, "SBX_RPC1 2 3 {\"version\":1,\"id\":1,\"op\":\"now\"}\n"));
    /* A matching relay with the wrong session must never reach this client. */
    pid_t check = fork(); assert(check >= 0);
    if (!check) { char stale[] = "SBX_REPLY1 2 1 {}"; control(stale, &child); _exit(0); }
    assert(waitpid(check, &status, 0) == check); assert(WIFEXITED(status) && WEXITSTATUS(status) == 125);
    char second[] = "SBX_REPLY1 2 3 {\"version\":1,\"id\":1,\"ok\":true}";
    control(second, &child); assert(!waiting_relay);
    n = read(peer, buffer, sizeof(buffer)-1); assert(n > 0); buffer[n] = 0;
    assert(!strcmp(buffer, "{\"version\":1,\"id\":1,\"ok\":true}\n"));
    close(peer); service_channels(&child, 0); assert(control_client == -1);
    /* Poll-driven handling fails closed on partial and outstanding disconnects
     * as well as an invalid descriptor, rather than spinning on EOF/errors. */
    for (int failure = 0; failure < 3; failure++) {
        check = fork(); assert(check >= 0);
        if (!check) {
            int pair[2]; assert(!socketpair(AF_UNIX, SOCK_STREAM|SOCK_NONBLOCK, 0, pair));
            control_client = pair[0]; request_used = 0; waiting_relay = failure == 1 ? 9 : 0;
            if (failure == 0) { assert(write(pair[1], "{", 1) == 1); close(pair[1]); }
            if (failure == 1) close(pair[1]);
            if (failure == 2) close(pair[0]);
            service_channels(&child, 0); _exit(0);
        }
        assert(waitpid(check, &status, 0) == check); assert(WIFEXITED(status) && WEXITSTATUS(status) == 125);
    }
    close(control_listener); control_listener = -1;
    assert(!unlink(address.sun_path) && !unlink(cmdline_path) && !rmdir(directory));
    /* With no timeout and nothing else to wake it, the supervisor still sees
     * its application exit, whether before or during the wait. */
    for (int later = 0; later < 2; later++) {
        check = fork(); assert(check >= 0);
        if (!check) {
            alarm(10); close(management); management = -1;
            watch_children();
            char *exit_later[] = {argv[0], "exit-later", NULL}, *exit_now[] = {"false", NULL};
            arguments = later ? exit_later : exit_now;
            child = start(); assert(child > 0);
            supervise(&child);
        }
        assert(waitpid(check, &status, 0) == check);
        assert(WIFEXITED(status) && WEXITSTATUS(status) == (later ? 7 : 1));
    }
}
