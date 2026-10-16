#define _GNU_SOURCE
#include <stdint.h>
#include <sys/resource.h>
#include <unistd.h>
static int application_execv(const char *path, char *const argv[]);
#define execv application_execv
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
#undef execv
#include <assert.h>
#include <sys/socket.h>
/* Exercise supervisor process control on the host without installing the guest
 * translator at /seedbox. Check its launch contract, then run the fixture. */
static int application_execv(const char *path, char *const argv[]) {
    assert(!strcmp(path, "/seedbox/progress/bin64/drrun"));
    assert(!strcmp(argv[0], path));
    assert(!strcmp(argv[1], "-opt_speed"));
    assert(!strcmp(argv[2], "-disable_traces"));
    size_t index = 3;
#if defined(__aarch64__)
    assert(!strcmp(argv[index++], "-ldstex2cas"));
    assert(!strcmp(argv[index++], "-no_hw_cache_consistency"));
    assert(!strcmp(argv[index++], "-fake_ctr_dic"));
#endif
    assert(!strcmp(argv[index++], "-c"));
    assert(!strcmp(argv[index++], "/seedbox/progress/libseedbox_progress.so"));
    assert(!strcmp(argv[index++], "--instructions-per-progress-call"));
    assert(!strcmp(argv[index++], "4294967295"));
    assert(!strcmp(argv[index++], "--progress-points-per-block"));
    assert(!strcmp(argv[index++], "4294967294"));
    assert(!strcmp(argv[index++], "--"));
    assert(argv[index]);
    return execv(argv[index], argv + index);
}
static int application_setrlimit(int resource, const struct rlimit *limit) {
    assert(resource == RLIMIT_NOFILE && launch_error_fd >= 0);
    assert(limit->rlim_cur == APPLICATION_NOFILE + INSTRUMENTATION_FDS);
    assert(limit->rlim_max == APPLICATION_NOFILE + INSTRUMENTATION_FDS);
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
    instructions_per_progress_call = UINT32_MAX;
    progress_points_per_block = UINT32_MAX - 1;
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
        assert(limit.rlim_cur == limit.rlim_max && limit.rlim_max <= APPLICATION_NOFILE + INSTRUMENTATION_FDS);
    }
    if (argc > 1 && !strcmp(argv[1], "exit-now")) _exit(1);
    if (argc > 1 && !strcmp(argv[1], "output-exit")) {
        const unsigned char out[] = {0, 255, '\n'}, err[] = {'e', 0, 128};
        int reopened = open("/proc/self/fd/1", O_WRONLY); assert(reopened >= 0);
        assert(write(reopened, out, sizeof(out)) == sizeof(out)); close(reopened);
        assert(write(STDERR_FILENO, err, sizeof(err)) == sizeof(err));
        _exit(9);
    }
    if (argc > 1 && !strcmp(argv[1], "exit-later")) {
        /* Launched by the supervisor: SIGCHLD must not stay blocked. */
        sigset_t mask; assert(!sigprocmask(SIG_SETMASK, NULL, &mask));
        if (sigismember(&mask, SIGCHLD)) _exit(3);
        struct timespec delay = {.tv_nsec = 100000000}; nanosleep(&delay, NULL);
        _exit(7);
    }
    if (argc > 2 && (!strcmp(argv[1], "property-exit") || !strcmp(argv[1], "property-signal"))) {
        /* Records sent before the exit reach the transport, flushed or not. */
        int sdk = socket(AF_UNIX, SOCK_SEQPACKET, 0); assert(sdk >= 0);
        struct sockaddr_un sdk_address = {.sun_family = AF_UNIX};
        strcpy(sdk_address.sun_path, argv[2]);
        assert(!connect(sdk, (struct sockaddr *)&sdk_address, sizeof(sdk_address)));
        assert(send(sdk, "{\"a\":1}", 7, 0) == 7);
        assert(send(sdk, property_flush, sizeof(property_flush) - 1, 0) == sizeof(property_flush) - 1);
        char ok[4]; assert(recv(sdk, ok, sizeof(ok), 0) == 2 && !memcmp(ok, "ok", 2));
        assert(send(sdk, "{\"b\":2}", 7, 0) == 7);
        if (!strcmp(argv[1], "property-signal")) raise(SIGTERM);
        _exit(5);
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
    /* A clock rate is one absolute ADJ_TICK|ADJ_FREQUENCY: whole 100 ppm tick
     * steps (USER_HZ 100) plus a remainder of at most 50 ppm in scaled ppm. */
    struct { const char *command; long tick, freq; } clock_rates[] = {
        {"clock_rate 10 100000000", 11000, 0}, {"clock_rate 11 -100000000", 9000, 0},
        {"clock_rate 12 150000", 10002, -50 * 65536}, {"clock_rate 13 -149999", 9999, -49999L * 65536 / 1000},
        {"clock_rate 14 1", 10000, 66}, {"clock_rate 15 0", 10000, 0}};
    assert(sysconf(_SC_CLK_TCK) == 100);
    for (unsigned i = 0; i < sizeof(clock_rates)/sizeof(*clock_rates); i++) {
        char command[64]; strcpy(command, clock_rates[i].command);
        int before = adjustments;
        control(command, &child); assert(child < 0 && adjustments == before + 1);
        assert(last_adjustment.modes == (ADJ_TICK | ADJ_FREQUENCY));
        assert(last_adjustment.tick == clock_rates[i].tick && last_adjustment.freq == clock_rates[i].freq);
        n=read(sockets[1],buffer,sizeof(buffer)-1); assert(n>0); buffer[n]=0;
        char expected[64]; snprintf(expected, sizeof(expected), "SEEDBOX_EFFECT id=%u action=clock_rate", 10 + i);
        assert(strstr(buffer, expected));
    }
    /* Past the kernel's reach the guest refuses without calling adjtimex. */
    int before = adjustments;
    char too_fast[] = "clock_rate 16 1000000001"; control(too_fast, &child); assert(adjustments == before);
    n=read(sockets[1],buffer,sizeof(buffer)-1); assert(n>0); buffer[n]=0; assert(strstr(buffer,"SEEDBOX_FAILURE id=16 action=clock_rate"));
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
    /* Property records: framing is hex of the exact datagram, sessions are
     * per connection, and loss is reported by the supervisor's own session. */
    char frame[PROPERTY_FRAME_BYTES];
    size_t framed = property_frame(frame, 7, 42, (const unsigned char *)"{}", 2);
    assert(framed == strlen("SBX_PROP1 7 42 7b7d\n") && !memcmp(frame, "SBX_PROP1 7 42 7b7d\n", framed));
    unsigned char large[PROPERTY_RECORD_BYTES + 1]; memset(large, 'x', sizeof(large));
    assert(property_frame(frame, 1, 1, large, PROPERTY_RECORD_BYTES) == strlen("SBX_PROP1 1 1 \n") + 2 * PROPERTY_RECORD_BYTES);
    assert(!property_frame(frame, 1, 1, large, sizeof(large)) && !property_frame(frame, 1, 1, large, 0));
    /* Kernel log records: only OOM kills and crashes are evidence, and the
     * message is escaped into one JSON string. */
    char kernel[PROPERTY_RECORD_BYTES];
    const char oom[] = "3,812,4096,-;Out of memory: Killed process 412 (ja\"va) total-vm:1kB\n SUBSYSTEM=x\n";
    size_t kernel_size = kernel_record(kernel, oom, sizeof(oom) - 1);
    kernel[kernel_size] = 0;
    assert(!strcmp(kernel, "{\"kernel\":{\"message\":\"Out of memory: Killed process 412 (ja\\\"va) total-vm:1kB\"}}"));
    const char segv[] = "6,9,1,-;worker[77]: segfault at 0 ip 1 sp 2 error 4\tx\n";
    kernel_size = kernel_record(kernel, segv, sizeof(segv) - 1);
    kernel[kernel_size] = 0;
    assert(strstr(kernel, "segfault at 0") && strstr(kernel, "\\u0009x"));
    const char fatal[] = "6,11,3,-;potentially unexpected fatal signal 6.\n";
    assert(kernel_record(kernel, fatal, sizeof(fatal) - 1));
    const char noise[] = "6,10,2,-;virtio_net virtio1 eth0: link up\n";
    assert(!kernel_record(kernel, noise, sizeof(noise) - 1));
    assert(!kernel_record(kernel, "no separator", 12));
    char sdk_directory[] = "/tmp/seedbox-sdk-XXXXXX"; assert(mkdtemp(sdk_directory));
    struct sockaddr_un sdk_address = {.sun_family = AF_UNIX};
    assert(snprintf(sdk_address.sun_path, sizeof(sdk_address.sun_path), "%s/run/sdk.sock", sdk_directory) > 0);
    property_socket(sdk_address.sun_path); assert(property_listener >= 0);
    assert(!stat(sdk_address.sun_path, &permissions)); assert((permissions.st_mode & 0777) == 0666);
    int sdk = socket(AF_UNIX, SOCK_SEQPACKET, 0); assert(sdk >= 0);
    assert(!connect(sdk, (struct sockaddr *)&sdk_address, sizeof(sdk_address)));
    assert(send(sdk, "{}", 2, 0) == 2);
    assert(send(sdk, property_flush, sizeof(property_flush) - 1, 0) == sizeof(property_flush) - 1);
    assert(send(sdk, large, sizeof(large), 0) == sizeof(large));
    service_channels(&child, 0);
    service_channels(&child, 0);
    n = read(sockets[1], buffer, sizeof(buffer)-1); assert(n > 0); buffer[n] = 0;
    char lost[256];
    snprintf(lost, sizeof(lost), "SBX_PROP1 1 1 7b7d\nSBX_PROP1 0 1 ");
    assert(!strncmp(buffer, lost, strlen(lost)));
    /* 7b226c6f7374223a = {"lost": */
    assert(strstr(buffer, "SBX_PROP1 0 1 7b226c6f7374223a") && buffer[n - 1] == '\n');
    char ok[4]; assert(recv(sdk, ok, sizeof(ok), MSG_DONTWAIT) == 2 && !memcmp(ok, "ok", 2));
    /* The engine answers a session over management; other sessions get nothing. */
    char choice_reply[] = "SBX_SDK1 1 {\"choice\":{\"index\":1}}";
    control(choice_reply, &child);
    char other_session[] = "SBX_SDK1 9 {}";
    control(other_session, &child);
    char answer[64]; n = recv(sdk, answer, sizeof(answer), MSG_DONTWAIT);
    assert(n == 22 && !memcmp(answer, "{\"choice\":{\"index\":1}}", 22));
    assert(recv(sdk, answer, sizeof(answer), MSG_DONTWAIT) < 0 && errno == EAGAIN);
    /* Connections beyond the session limit are refused and reported. */
    int extra[PROPERTY_SESSIONS];
    for (unsigned i = 0; i < PROPERTY_SESSIONS; i++) {
        extra[i] = socket(AF_UNIX, SOCK_SEQPACKET, 0); assert(extra[i] >= 0);
        assert(!connect(extra[i], (struct sockaddr *)&sdk_address, sizeof(sdk_address)));
    }
    service_channels(&child, 0);
    n = read(sockets[1], buffer, sizeof(buffer)-1); assert(n > 0); buffer[n] = 0;
    assert(!strncmp(buffer, "SBX_PROP1 0 2 7b226c6f7374223a", 30) && !strchr(buffer, '\n')[1]);
    assert(property_session_id == PROPERTY_SESSIONS);
    /* A closed session frees its slot; the next connection gets a new ID. */
    close(sdk); for (unsigned i = 0; i < PROPERTY_SESSIONS; i++) close(extra[i]);
    service_channels(&child, 0);
    for (unsigned i = 0; i < PROPERTY_SESSIONS; i++) assert(property_clients[i].fd == -1);
    /* At the application's exit, already-sent records precede its exit record. */
    for (int signalled = 0; signalled < 2; signalled++) {
        int records[2]; assert(!socketpair(AF_UNIX, SOCK_STREAM, 0, records));
        check = fork(); assert(check >= 0);
        if (!check) {
            alarm(10); close(records[1]); management = records[0];
            watch_children();
            arguments = (char *[]){argv[0], signalled ? "property-signal" : "property-exit", sdk_address.sun_path, NULL};
            child = start(); assert(child > 0);
            supervise(&child);
        }
        close(records[0]);
        assert(waitpid(check, &status, 0) == check);
        assert(WIFEXITED(status) && WEXITSTATUS(status) == (signalled ? 128 + SIGTERM : 5));
        size_t got = 0;
        while ((n = read(records[1], buffer + got, sizeof(buffer) - got - 1)) > 0) got += (size_t)n;
        assert(n == 0); buffer[got] = 0; close(records[1]);
        /* {"a":1}, {"b":2}, then {"exit":{"code":5}} or {"exit":{"signal":15}}. */
        unsigned session = PROPERTY_SESSIONS + 1 + signalled;
        char expected[512];
        snprintf(expected, sizeof(expected), "SBX_PROP1 %u 1 7b2261223a317d\nSBX_PROP1 %u 2 7b2262223a327d\nSBX_PROP1 0 3 %s\n",
                 session, session, signalled ? "7b2265786974223a7b227369676e616c223a31357d7d" : "7b2265786974223a7b22636f6465223a357d7d");
        assert(!strcmp(buffer, expected));
        /* The forked supervisor's session counter is its own. */
        property_session_id++;
    }
    close(property_listener); property_listener = -1;
    assert(!unlink(sdk_address.sun_path));
    char sdk_parent[PATH_MAX]; snprintf(sdk_parent, sizeof(sdk_parent), "%s/run", sdk_directory);
    assert(!rmdir(sdk_parent) && !rmdir(sdk_directory));
    /* Final output must reach the transport before the oracle exit, even when
     * SIGCHLD wins the poll race. Binary bytes and stream identity survive. */
    int logs[2]; assert(!socketpair(AF_UNIX, SOCK_STREAM, 0, logs));
    check = fork(); assert(check >= 0);
    if (!check) {
        alarm(10); close(logs[1]); close(management); management = logs[0];
        watch_children();
        arguments = (char *[]){argv[0], "output-exit", NULL};
        child = start(); assert(child > 0);
        supervise(&child);
    }
    close(logs[0]);
    assert(waitpid(check, &status, 0) == check);
    assert(WIFEXITED(status) && WEXITSTATUS(status) == 9);
    size_t logged = 0;
    while ((n = read(logs[1], buffer + logged, sizeof(buffer) - logged - 1)) > 0) logged += (size_t)n;
    assert(n == 0); buffer[logged] = 0; close(logs[1]);
    assert(strstr(buffer, "SBX_LOG1 1 00ff0a\n"));
    assert(strstr(buffer, "SBX_LOG1 2 650080\n"));
    /* With no timeout and nothing else to wake it, the supervisor still sees
     * its application exit, whether before or during the wait. */
    for (int later = 0; later < 2; later++) {
        check = fork(); assert(check >= 0);
        if (!check) {
            alarm(10); close(management); management = -1;
            watch_children();
            char *exit_later[] = {argv[0], "exit-later", NULL}, *exit_now[] = {argv[0], "exit-now", NULL};
            arguments = later ? exit_later : exit_now;
            child = start(); assert(child > 0);
            supervise(&child);
        }
        assert(waitpid(check, &status, 0) == check);
        assert(WIFEXITED(status) && WEXITSTATUS(status) == (later ? 7 : 1));
    }
}
