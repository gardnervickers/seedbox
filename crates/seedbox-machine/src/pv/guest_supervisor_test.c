/* Host POSIX mechanism test, not ARM64PV/RV64TCG native acceptance.
 * Build together with guest_supervisor.c; no guest libc artifacts required. */
#define _GNU_SOURCE
#include "guest_supervisor.h"
#include "guest_service.h"
#include <assert.h>
#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/wait.h>
#include <sys/stat.h>
#include <sys/resource.h>
#include <unistd.h>
#ifdef __linux__
#include <sys/prctl.h>
#endif

static pid_t owned_group = -1;
static void cleanup(void) {
    if (owned_group > 0) {
        kill(-owned_group, SIGKILL);
        while (waitpid(-owned_group, NULL, 0) > 0 || errno == EINTR) {}
        owned_group = -1;
    }
}
static void expired(int signal) { (void)signal; cleanup(); _exit(99); }
struct recipe { const char *path; const char *mode; unsigned launches; sigset_t application_mask; };
static int launch(void *opaque, int error_fd) {
    (void)error_fd;
    struct recipe *recipe = opaque;
    if (sigprocmask(SIG_SETMASK, &recipe->application_mask, NULL)) return errno;
    execl(recipe->path, recipe->path, recipe->mode, (char *)NULL);
    return errno;
}
struct captured { unsigned char bytes[2][8192]; size_t used[2]; };
static int output(void *opaque, unsigned stream, const unsigned char *bytes, size_t length) {
    struct captured *captured = opaque;
    assert(stream < 2 && captured->used[stream] + length <= sizeof(captured->bytes[stream]));
    memcpy(captured->bytes[stream] + captured->used[stream], bytes, length);
    captured->used[stream] += length;
    return 0;
}
struct wire { char bytes[32768]; size_t used; unsigned writes; int fail_log; };
static int write_wire(void *opaque, const void *bytes, size_t length) {
    struct wire *wire = opaque;
    wire->writes++;
    if (wire->fail_log && length >= 9 && !memcmp(bytes, "SBX_LOG1 ", 9)) { errno = EIO; return -1; }
    assert(wire->used + length < sizeof(wire->bytes));
    memcpy(wire->bytes + wire->used, bytes, length);
    wire->used += length; wire->bytes[wire->used] = 0;
    return 0;
}
static void start(struct sbx_supervisor *s, struct recipe *recipe) {
    assert(sbx_supervisor_start(s, launch, recipe) == 0);
    owned_group = s->child;
    recipe->launches++;
}
static void ready(struct sbx_supervisor *s, struct captured *captured) {
    struct pollfd fd = {.fd = s->output[0], .events = POLLIN};
    assert(poll(&fd, 1, -1) == 1);
    assert(sbx_supervisor_output(s, 0, output, captured) == 0);
    assert(captured->used[0] == 6 && memcmp(captured->bytes[0], "ready\n", 6) == 0);
}
static int collect(struct sbx_supervisor *s, struct captured *captured) {
    int code = -1, result;
    while (!(result = sbx_supervisor_collect(s, &code, output, captured))) usleep(1000);
    assert(result == 1);
    assert(s->child == -1 && s->output[0] == -1 && s->output[1] == -1);
    assert(sbx_supervisor_collect(s, &code, output, captured) == 0);
    return code;
}
static int64_t stepped[8];
static unsigned steps;
static int refuse_step;
static int record_step(int64_t ns) {
    stepped[steps++] = ns;
    if (refuse_step) { errno = EINVAL; return -1; }
    return 0;
}
int main(int argc, char **argv) {
    if (argc == 2) {
        sigset_t mask;
        assert(sigprocmask(SIG_SETMASK, NULL, &mask) == 0);
        assert(!sigismember(&mask, SIGCHLD));
#ifdef __linux__
        struct stat pipe;
        assert(fstat(STDOUT_FILENO, &pipe) == 0 && (pipe.st_mode & 0777) == 0666);
        int reopened = open("/proc/self/fd/1", O_WRONLY);
        assert(reopened >= 0); close(reopened);
#endif
        if (!strcmp(argv[1], "binary")) {
            unsigned char bytes[5000];
            for (unsigned i = 0; i < sizeof(bytes); ++i) bytes[i] = (unsigned char)i;
            assert(write(1, bytes, sizeof(bytes)) == sizeof(bytes));
            assert(write(2, "\xff\0z", 3) == 3);
            return 23;
        }
        if (!strcmp(argv[1], "inherited")) {
            pid_t child = fork(); assert(child >= 0);
            if (!child) for (;;) pause();
            assert(write(1, "tail\0", 5) == 5);
            return 7;
        }
        if (!strcmp(argv[1], "orphan")) {
            pid_t child = fork(); assert(child >= 0);
            if (!child) {
                pid_t orphan = fork(); assert(orphan >= 0);
                _exit(0);
            }
            assert(waitpid(child, NULL, 0) == child);
        } else if (!strcmp(argv[1], "group")) {
            pid_t child = fork(); assert(child >= 0);
            if (!child) for (;;) pause();
        } else assert(!strcmp(argv[1], "running"));
        assert(write(2, "q\0!", 3) == 3);
        assert(write(1, "ready\n", 6) == 6);
        for (;;) pause();
    }
#ifdef __linux__
    /* Match PID 1 adoption while retaining isolated process groups. */
    assert(prctl(PR_SET_CHILD_SUBREAPER, 1) == 0);
#endif
    assert(atexit(cleanup) == 0);
    assert(signal(SIGALRM, expired) != SIG_ERR);
    assert(signal(SIGABRT, expired) != SIG_ERR);
    assert(signal(SIGTERM, expired) != SIG_ERR);
    assert(signal(SIGINT, expired) != SIG_ERR);
    alarm(15); /* Test failure guard only; supervisor itself has no deadline. */
    struct sbx_supervisor s; sbx_supervisor_init(&s);
    struct recipe recipe = {.path = argv[0], .mode = "running"};
    sigset_t blocked;
    sigemptyset(&blocked); sigaddset(&blocked, SIGCHLD);
    assert(sigprocmask(SIG_BLOCK, &blocked, &recipe.application_mask) == 0);
    struct captured captured = {0};
    assert(sbx_supervisor_stop(&s, output, &captured) == -1 && errno == EINVAL);
    assert(sbx_supervisor_pause(&s) == -1 && errno == EINVAL);
    assert(sbx_supervisor_resume(&s) == -1 && errno == EINVAL);
    struct recipe missing = recipe;
    missing.path = "/seedbox-nonexistent-test-application";
    assert(sbx_supervisor_start(&s, launch, &missing) == -1 && errno == ENOENT);
    assert(s.child == -1 && s.output[0] == -1 && s.output[1] == -1);

    start(&s, &recipe); ready(&s, &captured);
    assert(sbx_supervisor_start(&s, launch, &recipe) == -1 && errno == EBUSY);
    assert(sbx_supervisor_pause(&s) == 0);
    siginfo_t info = {0};
    assert(waitid(P_PID, (id_t)s.child, &info, WSTOPPED | WNOWAIT | WNOHANG) == 0);
    assert(info.si_code == CLD_STOPPED);
    assert(sbx_supervisor_resume(&s) == 0);
    assert(sbx_supervisor_stop(&s, output, &captured) == 0);
    assert(captured.used[1] == 3 && memcmp(captured.bytes[1], "q\0!", 3) == 0);
    owned_group = -1;
    int code = -1;
    assert(sbx_supervisor_collect(&s, &code, output, &captured) == 0 && code == -1);
    assert(sbx_supervisor_start(&s, launch, &missing) == -1 && errno == ENOENT);
    captured = (struct captured){0};
    start(&s, &recipe); ready(&s, &captured);
    assert(recipe.launches == 2); /* Same retained recipe can launch again. */
    assert(kill(-s.child, SIGTERM) == 0);
    assert(collect(&s, &captured) == 128 + SIGTERM);
    owned_group = -1;

    recipe.mode = "binary"; captured = (struct captured){0};
    start(&s, &recipe);
    info = (siginfo_t){0};
    assert(waitid(P_PID, (id_t)s.child, &info, WEXITED | WNOWAIT) == 0);
    int pause_result = sbx_supervisor_pause(&s);
    /* Darwin rejects signaling a zombie with EPERM; Linux reports ESRCH or
     * lets waitid observe CLD_EXITED. Neither may consume the natural exit. */
    assert(pause_result == -1);
    assert(collect(&s, &captured) == 23);
    owned_group = -1;
    assert(captured.used[0] == 5000);
    for (unsigned i = 0; i < captured.used[0]; ++i)
        assert(captured.bytes[0][i] == (unsigned char)i);
    assert(captured.used[1] == 3 && memcmp(captured.bytes[1], "\xff\0z", 3) == 0);

#ifdef __linux__
    captured = (struct captured){0};
    start(&s, &recipe);
    info = (siginfo_t){0};
    assert(waitid(P_PID, (id_t)s.child, &info, WEXITED | WNOWAIT) == 0);
    assert(sbx_supervisor_stop(&s, output, &captured) == 0);
    owned_group = -1;
    code = -1;
    assert(sbx_supervisor_collect(&s, &code, output, &captured) == 0 && code == -1);
    assert(captured.used[0] == 5000 && captured.used[1] == 3);
#endif

    recipe.mode = "inherited"; captured = (struct captured){0};
    start(&s, &recipe);
    pid_t inherited_group = s.child;
    assert(collect(&s, &captured) == 7); /* Kill the pipe holder; preserve leader code. */
    assert(captured.used[0] == 5 && memcmp(captured.bytes[0], "tail\0", 5) == 0);
#ifdef __linux__
    assert(waitpid(-inherited_group, NULL, WNOHANG) == -1 && errno == ECHILD);
    assert(kill(-inherited_group, 0) == -1 && errno == ESRCH);
#else
    (void)inherited_group;
#endif
    owned_group = -1;
    recipe.mode = "binary"; captured = (struct captured){0};
    start(&s, &recipe);
    assert(collect(&s, &captured) == 23);
    assert(captured.used[0] == 5000 && captured.used[1] == 3);
    owned_group = -1;

#ifdef __linux__
    recipe.mode = "orphan"; captured = (struct captured){0};
    start(&s, &recipe); ready(&s, &captured);
    info = (siginfo_t){0};
    assert(waitid(P_ALL, 0, &info, WEXITED | WNOWAIT) == 0);
    assert(info.si_pid != s.child);
    assert(sbx_supervisor_collect(&s, &code, output, &captured) == 0);
    assert(waitpid(info.si_pid, NULL, WNOHANG) == -1 && errno == ECHILD);
    assert(sbx_supervisor_stop(&s, output, &captured) == 0);
    owned_group = -1;
#endif

    recipe.mode = "group"; captured = (struct captured){0};
    start(&s, &recipe); ready(&s, &captured);
    pid_t group = s.child;
    assert(sbx_supervisor_stop(&s, output, &captured) == 0);
#ifdef __linux__
    assert(waitpid(-group, NULL, WNOHANG) == -1 && errno == ECHILD);
#else
    (void)group;
#endif
    owned_group = -1;
    /* Exercise the actual private wire adapter with live processes. */
    struct sbx_service service;
    struct wire wire = {0};
    recipe.mode = "running";
    sbx_service_init(&service, launch, &recipe, write_wire, &wire);
    assert(sbx_service_start(&service) == 0);
    owned_group = service.supervisor.child;
    assert(!strcmp(wire.bytes, "SBX_READY1\n"));
    struct captured ready_bytes = {0}; ready(&service.supervisor, &ready_bytes);
    assert(sbx_service_feed(&service, "free", 4) == 0);
    assert(!strcmp(wire.bytes, "SBX_READY1\n"));
    assert(sbx_service_feed(&service, "ze 17\nthaw 18\nstop 19\n", 22) == 0);
    owned_group = -1;
    assert(strstr(wire.bytes, "SEEDBOX_EFFECT id=17 action=freeze guest_ns="));
    assert(strstr(wire.bytes, "SEEDBOX_EFFECT id=18 action=thaw guest_ns="));
    assert(strstr(wire.bytes, "SBX_LOG1 2 710021\n"));
    assert(strstr(wire.bytes, "SEEDBOX_EFFECT id=19 action=stop guest_ns="));
    assert(!strstr(wire.bytes, "SBX_EXIT1"));
    recipe.path = missing.path;
    assert(sbx_service_feed(&service, "start 20\n", 9) == 0);
    assert(strstr(wire.bytes, "SEEDBOX_FAILURE id=20 action=start guest_ns="));
    recipe.path = argv[0]; recipe.mode = "binary";
    assert(sbx_service_feed(&service, "start 21\n", 9) == 0);
    owned_group = service.supervisor.child;
    int result;
    while (!(result = sbx_service_collect(&service, &code))) usleep(1000);
    assert(result == 1 && code == 23);
    owned_group = -1;
    assert(strstr(wire.bytes, "SBX_EXIT1 status=23\n"));
    assert(sbx_service_feed(&service, "shut", 4) == 0 && !service.shutdown_requested);
    assert(sbx_service_feed(&service, "down\n", 5) == 0 && service.shutdown_requested);
    assert(sbx_service_feed(&service, "start 0\n", 8) == -1 && errno == EINVAL);
    sbx_service_init(&service, launch, &recipe, write_wire, &wire);
    assert(sbx_service_feed(&service, "start 42949672960\n", 18) == -1 && errno == EINVAL);
    wire = (struct wire){0}; recipe.mode = "running";
    sbx_service_init(&service, launch, &recipe, write_wire, &wire);
    assert(sbx_service_start(&service) == 0);
    owned_group = service.supervisor.child;
    ready_bytes = (struct captured){0}; ready(&service.supervisor, &ready_bytes);
    wire.fail_log = 1;
    assert(sbx_service_feed(&service, "stop 22\n", 8) == -1 && errno == EIO);
    owned_group = -1;
    assert(service.poisoned && wire.writes == 2);
    assert(!strstr(wire.bytes, "SEEDBOX_FAILURE") && !strstr(wire.bytes, "SEEDBOX_EFFECT"));
    assert(sbx_service_feed(&service, "start 23\n", 9) == -1 && errno == EPIPE);
    assert(wire.writes == 2 && service.supervisor.child == -1);
    /* Realtime steps parse signed nonzero offsets and acknowledge as step. */
    wire = (struct wire){0};
    sbx_service_init(&service, launch, &recipe, write_wire, &wire);
    service.step_realtime = record_step;
    assert(sbx_service_feed(&service, "step 5 -1500000000\nstep 6 9223372036854775807\n", 46) == 0);
    assert(stepped[0] == -1500000000LL && stepped[1] == INT64_MAX && steps == 2);
    assert(strstr(wire.bytes, "SEEDBOX_EFFECT id=5 action=step guest_ns="));
    assert(strstr(wire.bytes, "SEEDBOX_EFFECT id=6 action=step guest_ns="));
    refuse_step = 1;
    assert(sbx_service_feed(&service, "step 7 -9223372036854775808\n", 28) == 0);
    assert(stepped[2] == INT64_MIN && steps == 3);
    assert(strstr(wire.bytes, "SEEDBOX_FAILURE id=7 action=step guest_ns="));
    const char *malformed[] = {"step 8 0\n", "step 0 5\n", "step 8 05\n", "step 8 -\n",
                               "step 8\n", "step 8 1 2\n", "step 8 9223372036854775808\n",
                               "step 8 -9223372036854775809\n", "step 8 +1\n"};
    for (size_t i = 0; i < sizeof(malformed) / sizeof(*malformed); ++i) {
        sbx_service_init(&service, launch, &recipe, write_wire, &wire);
        service.step_realtime = record_step;
        assert(sbx_service_feed(&service, malformed[i], strlen(malformed[i])) == -1 && errno == EINVAL);
    }
    assert(steps == 3);
    /* Exhaust only this isolated fixture's descriptor allowance: a local pipe
     * setup failure is fatal, unlike a reported child exec failure. */
    wire = (struct wire){0};
    sbx_service_init(&service, launch, &recipe, write_wire, &wire);
    struct rlimit saved_limit, low_limit;
    assert(getrlimit(RLIMIT_NOFILE, &saved_limit) == 0);
    low_limit = saved_limit; low_limit.rlim_cur = 3;
    assert(setrlimit(RLIMIT_NOFILE, &low_limit) == 0);
    result = sbx_service_feed(&service, "start 24\n", 9);
    int saved_error = errno;
    assert(setrlimit(RLIMIT_NOFILE, &saved_limit) == 0);
    assert(result == -1 && saved_error == EMFILE && service.poisoned);
    assert(service.supervisor.failure == SBX_FAILURE_FATAL);
    assert(wire.writes == 0 && service.supervisor.child == -1);
    puts("guest supervisor host POSIX tests passed");
    return 0;
}
