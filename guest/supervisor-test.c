#define _GNU_SOURCE
#include <stdint.h>
static int readiness_ioperm(unsigned long first, unsigned long count, int enabled);
static void readiness_outl(uint32_t id, unsigned short port);
#define SEEDBOX_READY_IOPERM readiness_ioperm
#define SEEDBOX_READY_OUTL readiness_outl
#define SEEDBOX_TEST_READY_DELAY_NS 10000000ULL
#define SEEDBOX_SUPERVISOR_TEST
#include "compose-init.c"
#include <assert.h>
#include <sys/prctl.h>
#include <sys/socket.h>
static int readiness_permissions, readiness_writes;
static struct timespec readiness_started;
static int readiness_ioperm(unsigned long first, unsigned long count, int enabled) {
    assert(first == SEEDBOX_EFFECT_PORT && count == SEEDBOX_EFFECT_PORT_COUNT && enabled == 1);
    assert(fcntl(management, F_GETFL) & O_NONBLOCK);
    assert(fcntl(management, F_GETFD) & FD_CLOEXEC);
    assert(!isatty(management));
    /* Every service was launched (and is still running) before readiness. */
    for (uint32_t i = 0; i < service_count; i++) {
        int status; assert(services[i].state == RUNNING && waitpid(services[i].init, &status, WNOHANG) == 0);
    }
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
static void test_cmdline(const char *path, const char *text) {
    int fd = open(path, O_WRONLY|O_CREAT|O_TRUNC, 0600); assert(fd >= 0);
    assert(write(fd, text, strlen(text)) == (ssize_t)strlen(text));
    assert(!close(fd));
}
static void put32(FILE *f, uint32_t v) { for (int i = 0; i < 4; i++) fputc((int)(v >> (8 * i)) & 0xff, f); }
static void put(FILE *f, const char *s) { put32(f, (uint32_t)strlen(s)); fputs(s, f); }
static char *program;
static void service(struct service *s, const char *name, int check, char *line) {
    *s = (struct service){.name = (char *)name, .image = "image", .address = "10.0.0.1", .user = "", .working_dir = "/",
        .check = check, .state = STOPPED, .init = -1, .netns = -1, .output = -1};
    char **arguments = calloc(4, sizeof(char *)); assert(arguments);
    arguments[0] = program; arguments[1] = "child"; arguments[2] = line;
    s->arguments = arguments;
    static char *environment[] = {"PATH=/bin:/usr/bin", NULL}, *volumes[] = {NULL};
    s->environment = environment;
    s->volumes = volumes;
}
static char *management_read(int peer) {
    static char buffer[1024];
    ssize_t n = read(peer, buffer, sizeof(buffer) - 1); assert(n > 0); buffer[n] = 0;
    return buffer;
}
static void command(int peer, const char *text) {
    assert(write(peer, text, strlen(text)) == (ssize_t)strlen(text));
    relay_management();
}
static int exits_with(void (*body)(void)) {
    pid_t check = fork(); assert(check >= 0);
    if (!check) { body(); _exit(0); }
    int status; assert(waitpid(check, &status, 0) == check);
    assert(WIFEXITED(status));
    return WEXITSTATUS(status);
}
static void unknown_service(void) { char bad[] = "stop 9 nobody"; control(bad); }
static void malformed_command(void) { char bad[] = "stop 9"; control(bad); }
static void check_exit_completes_machine(void) {
    service_count = 1;
    service(&services[0], "check", 1, "exit=9");
    assert(!start(&services[0]));
    for (;;) service_channels(-1);
}
int main(int argc, char **argv) {
    if (argc > 2 && !strncmp(argv[2], "exit=", 5)) return atoi(argv[2] + 5);
    if (argc > 1) {
        if (argc > 2) { printf("%s\npartial", argv[2]); fflush(stdout); }
        for (;;) pause();
    }
    program = argv[0];
    assert(!prctl(PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0));
    watch_children();
    /* The recipe reader accepts the shared-kernel form and rejects a NIC
     * machine with more than one service. */
    char recipe[] = "/tmp/seedbox-recipe-XXXXXX"; int recipe_file = mkstemp(recipe); assert(recipe_file >= 0);
    FILE *f = fdopen(recipe_file, "w"); assert(f);
    fputs("SBXLAUNCH3", f); put32(f, 1); put32(f, 1000000); put32(f, 0); put32(f, 200000); put32(f, 0); put32(f, 1024);
    put(f, "10.0.0.1 a\n10.0.0.2 b\n"); put32(f, 2);
    const char *names[] = {"a", "b"}, *addresses[] = {"10.0.0.1", "10.0.0.2"};
    for (int i = 0; i < 2; i++) {
        put(f, names[i]); put(f, "0123abcd"); put(f, addresses[i]); put32(f, i == 0 ? 3 : 0); put(f, ""); put(f, "/");
        put32(f, 1); put(f, "/bin/true"); put32(f, 1); put(f, "PATH=/bin"); put32(f, 0);
        put32(f, 1); put(f, "/etc/app.conf"); put(f, "x=1\n");
    }
    assert(!fclose(f));
    read_recipe(recipe);
    assert(namespaces && service_count == 2 && latency_ns == 1000000 && jitter_ns == 200000);
    assert(services[0].check && services[0].controller && !services[1].check && !services[1].controller);
    assert(!strcmp(services[1].address, "10.0.0.2") && services[1].configs == 1 && services[1].config_sizes[0] == 4);
    assert(!strcmp(services[0].image, "0123abcd") && !strcmp(services[1].image, "0123abcd"));
    assert(!strcmp(services[0].arguments[0], "/bin/true") && find_service("b") == &services[1] && !find_service("c"));
    assert(!unlink(recipe));
    namespaces = 0;

    /* Each service's output reaches the console as prefixed lines. */
    char console[] = "/tmp/seedbox-console-XXXXXX"; int console_file = mkstemp(console); assert(console_file >= 0);
    int saved = dup(STDOUT_FILENO); assert(saved >= 0 && dup2(console_file, STDOUT_FILENO) == STDOUT_FILENO);
    int sockets[2]; assert(!socketpair(AF_UNIX,SOCK_STREAM,0,sockets));
    assert(!fcntl(sockets[0], F_SETFL, O_NONBLOCK));
    assert(!fcntl(sockets[0], F_SETFD, FD_CLOEXEC));
    management = sockets[0];
    service_count = 2;
    service(&services[0], "alpha", 1, "hello alpha");
    service(&services[1], "beta", 0, "hello beta");
    static char *delayed[] = {"PATH=/bin:/usr/bin", "SEEDBOX_TEST_DELAY_READY=1", NULL};
    services[1].environment = delayed;
    for (uint32_t i = 0; i < 2; i++) assert(!start(&services[i]));
    pid_t beta = services[1].init;
    /* Announce readiness only after channel opening, every initial exec, and
     * the retained readiness delay. The wrappers check the complete grant. */
    assert(!clock_gettime(CLOCK_MONOTONIC, &readiness_started));
    management_ready();
    assert(readiness_permissions && readiness_writes);
    service_count = 3;
    service(&services[2], "gamma", 0, "exit=7");
    assert(!start(&services[2]));
    /* A service exiting on its own is reaped and reported; others continue. */
    while (services[2].state != EXITED) service_channels(100);
    assert(services[0].state == RUNNING && services[1].state == RUNNING);

    /* Commands may arrive in fragments; stop reaps the whole service first. */
    assert(write(sockets[1], "stop 1 ", 7) == 7);
    relay_management(); assert(services[1].state == RUNNING && command_used == 7);
    command(sockets[1], "beta\n");
    assert(services[1].state == STOPPED && services[1].init == -1 && !command_used);
    int status; assert(waitpid(beta, &status, WNOHANG) == -1 && errno == ECHILD);
    assert(waitpid(-beta, &status, WNOHANG) == -1 && errno == ECHILD);
    assert(strstr(management_read(sockets[1]), "SEEDBOX_EFFECT id=1 action=stop guest_ns="));
    command(sockets[1], "stop 2 beta\nstart 3 beta\n");
    char *acks = management_read(sockets[1]);
    assert(strstr(acks, "SEEDBOX_FAILURE id=2") && strstr(acks, "SEEDBOX_EFFECT id=3 action=start"));
    assert(services[1].state == RUNNING && services[1].init > 0 && services[1].init != beta);
    /* Stopping an exited service only records it as stopped; it restarts. */
    command(sockets[1], "stop 4 gamma\n");
    assert(strstr(management_read(sockets[1]), "SEEDBOX_EFFECT id=4") && services[2].state == STOPPED);
    /* A failed exec is a failed restart, not a running service. */
    assert(!stop(&services[1]));
    static char *missing[] = {"/definitely/no/such/executable", NULL};
    services[1].arguments = missing;
    command(sockets[1], "start 5 beta\n");
    assert(strstr(management_read(sockets[1]), "SEEDBOX_FAILURE id=5") && services[1].state == EXITED);
    /* Partitions need namespaces; unknown handles cannot heal. */
    command(sockets[1], "partition 6 p1 alpha beta\nheal 7 p1\n");
    acks = management_read(sockets[1]);
    assert(strstr(acks, "SEEDBOX_FAILURE id=6") && strstr(acks, "SEEDBOX_FAILURE id=7"));
    assert(exits_with(unknown_service) == 125 && exits_with(malformed_command) == 125);
    assert(!stop(&services[0]));
    assert(waitpid(-1, &status, WNOHANG) == -1 && errno == ECHILD);
    fflush(stdout); assert(dup2(saved, STDOUT_FILENO) == STDOUT_FILENO && !close(saved));
    char text[4096]; ssize_t n = pread(console_file, text, sizeof(text) - 1, 0); assert(n > 0); text[n] = 0;
    assert(strstr(text, "alpha | hello alpha\n") && strstr(text, "beta | hello beta\n"));
    /* A partial final line is completed when the service's output closes. */
    assert(strstr(text, "alpha | partial\n") && strstr(text, "beta | partial\n"));
    assert(strstr(text, "seedbox: service gamma exited with status 7\n"));
    assert(!unlink(console) && !close(console_file));
    /* The machine completes with its check service's status. */
    assert(exits_with(check_exit_completes_machine) == 9);

    /* The public RPC socket is created only from an exact authoritative boot
     * token; image environment entries cannot enable it on other guests. */
    service_count = 0;
    char directory[] = "/tmp/seedbox-relay-XXXXXX"; assert(mkdtemp(directory));
    char cmdline_path[PATH_MAX];
    assert(snprintf(cmdline_path, sizeof(cmdline_path), "%s/cmdline", directory) > 0);
    struct sockaddr_un address = {.sun_family = AF_UNIX};
    assert(snprintf(address.sun_path, sizeof(address.sun_path), "%s/control.sock", directory) > 0);
    const char *disabled[] = {"quiet\n", "seedbox.control=0\n", "seedbox.control=11\n", "xseedbox.control=1\n", "app=seedbox.control=1\n"};
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
    service_channels(0); assert(control_client >= 0 && session_id == 1);
    const char *request = "{\"version\":1,\"id\":1,\"op\":\"now\"}";
    char buffer[512];
    assert(write(peer, request, strlen(request)) == (ssize_t)strlen(request));
    service_channels(0); assert(!waiting_relay);
    assert(write(peer, "\n", 1) == 1); service_channels(0); assert(waiting_relay == 1);
    n = read(sockets[1], buffer, sizeof(buffer)-1); assert(n > 0); buffer[n] = 0;
    assert(!strcmp(buffer, "SBX_RPC1 1 1 {\"version\":1,\"id\":1,\"op\":\"now\"}\n"));
    char reply[] = "SBX_REPLY1 1 1 {\"version\":1,\"id\":1,\"ok\":true}";
    assert(write(sockets[1], reply, 8) == 8); service_channels(0); assert(waiting_relay);
    assert(write(sockets[1], reply + 8, strlen(reply) - 8) == (ssize_t)strlen(reply) - 8);
    assert(write(sockets[1], "\n", 1) == 1); service_channels(0); assert(!waiting_relay);
    n = read(peer, buffer, sizeof(buffer)-1); assert(n > 0); buffer[n] = 0;
    assert(!strcmp(buffer, "{\"version\":1,\"id\":1,\"ok\":true}\n"));
    /* An idle HUP releases only the client; the same listener accepts a new
     * session. Empty sessions consume a session ID but no relay ID. */
    int listener = control_listener;
    close(peer); service_channels(0); assert(control_client == -1 && control_listener == listener);
    peer = socket(AF_UNIX, SOCK_STREAM, 0); assert(peer >= 0);
    assert(!connect(peer, (struct sockaddr *)&address, sizeof(address)));
    service_channels(0); assert(session_id == 2);
    close(peer); service_channels(0); assert(control_client == -1 && relay_id == 1);
    peer = socket(AF_UNIX, SOCK_STREAM, 0); assert(peer >= 0);
    assert(!connect(peer, (struct sockaddr *)&address, sizeof(address)));
    service_channels(0); assert(session_id == 3);
    assert(write(peer, request, strlen(request)) == (ssize_t)strlen(request)); assert(write(peer, "\n", 1) == 1);
    service_channels(0); assert(waiting_relay == 2);
    n = read(sockets[1], buffer, sizeof(buffer)-1); assert(n > 0); buffer[n] = 0;
    assert(!strcmp(buffer, "SBX_RPC1 2 3 {\"version\":1,\"id\":1,\"op\":\"now\"}\n"));
    /* A matching relay with the wrong session must never reach this client. */
    pid_t check = fork(); assert(check >= 0);
    if (!check) { char stale[] = "SBX_REPLY1 2 1 {}"; control(stale); _exit(0); }
    assert(waitpid(check, &status, 0) == check); assert(WIFEXITED(status) && WEXITSTATUS(status) == 125);
    char second[] = "SBX_REPLY1 2 3 {\"version\":1,\"id\":1,\"ok\":true}";
    control(second); assert(!waiting_relay);
    n = read(peer, buffer, sizeof(buffer)-1); assert(n > 0); buffer[n] = 0;
    assert(!strcmp(buffer, "{\"version\":1,\"id\":1,\"ok\":true}\n"));
    close(peer); service_channels(0); assert(control_client == -1);
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
            service_channels(0); _exit(0);
        }
        assert(waitpid(check, &status, 0) == check); assert(WIFEXITED(status) && WEXITSTATUS(status) == 125);
    }
    close(control_listener); control_listener = -1;
    assert(!unlink(address.sun_path) && !unlink(cmdline_path) && !rmdir(directory));
}
