/* Native Driver acceptance only: read real service replies and exercise a peer.
 * The ordinary peer and oracle use the same retained executable, selected by argv. */
#define _POSIX_C_SOURCE 200809L
#include <ctype.h>
#include <errno.h>
#include <limits.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <time.h>
#include <unistd.h>

static void fail(const char *phase) {
    (void)dprintf(STDERR_FILENO, "DRIVER_RPC_FAILURE %s errno=%d\n", phase, errno);
    _exit(125);
}
static void put(int fd, const char *text, size_t size) {
    while (size) {
        ssize_t count = write(fd, text, size);
        if (count < 0 && errno == EINTR) continue;
        if (count <= 0) fail("write");
        text += count; size -= (size_t)count;
    }
}
static void marker(const char *text) { put(STDOUT_FILENO, text, strlen(text)); }

/* A bounded JSON reader for the fixture's reply objects. Inspect/arrays and
 * operation interpretation are deliberately absent; production owns the wire. */
struct value { const char *start, *end; };
static const char *space(const char *p) { while (*p && isspace((unsigned char)*p)) ++p; return p; }
static const char *string_end(const char *p) {
    if (*p++ != '"') fail("JSON string");
    while (*p && *p != '"') {
        if ((unsigned char)*p < 32) fail("JSON string control");
        if (*p++ == '\\' && !*p++) fail("JSON escape");
    }
    if (!*p) fail("JSON string end");
    return p + 1;
}
static const char *value_end(const char *p, unsigned depth) {
    if (depth > 8) fail("JSON depth");
    p = space(p);
    if (*p == '"') return string_end(p);
    if (*p == '{') {
        p = space(p + 1);
        if (*p == '}') return p + 1;
        for (;;) {
            p = space(string_end(p));
            if (*p++ != ':') fail("JSON colon");
            p = space(value_end(p, depth + 1));
            if (*p == '}') return p + 1;
            if (*p++ != ',') fail("JSON comma");
            p = space(p);
        }
    }
    if (!strncmp(p, "true", 4) || !strncmp(p, "null", 4)) return p + 4;
    if (!strncmp(p, "false", 5)) return p + 5;
    if (!isdigit((unsigned char)*p)) fail("JSON value");
    while (isdigit((unsigned char)*p)) ++p;
    return p;
}
static struct value field(struct value object, const char *name) {
    const char *p = space(object.start);
    if (*p++ != '{') fail("JSON object");
    struct value found = {0};
    p = space(p);
    while (*p != '}') {
        const char *key = p, *end = string_end(p);
        p = space(end);
        if (*p++ != ':') fail("JSON field colon");
        p = space(p);
        const char *last = value_end(p, 0);
        if ((size_t)(end - key) == strlen(name) + 2 && !memcmp(key + 1, name, strlen(name))) {
            if (found.start) fail("duplicate JSON field");
            found = (struct value){p, last};
        }
        p = space(last);
        if (*p == '}') break;
        if (*p++ != ',') fail("JSON field comma");
        p = space(p);
    }
    if (p + 1 != object.end || !found.start) fail("missing JSON field");
    return found;
}
static unsigned long long number(struct value value) {
    unsigned long long result = 0;
    if (value.start == value.end) fail("JSON empty number");
    for (const char *p = value.start; p != value.end; ++p) {
        if (!isdigit((unsigned char)*p) || result > (ULLONG_MAX - (unsigned)(*p - '0')) / 10)
            fail("JSON unsigned number");
        result = result * 10 + (unsigned)(*p - '0');
    }
    return result;
}
static int literal(struct value value, const char *expected) {
    return (size_t)(value.end - value.start) == strlen(expected) && !memcmp(value.start, expected, strlen(expected));
}
static char reply[4096];
static int stream_marker;
static void marker_delay(void) {
    struct timespec delay = {.tv_nsec = 20000000};
    while (nanosleep(&delay, &delay)) if (errno != EINTR) fail("marker delay");
}
static struct value call(int fd, unsigned id, const char *operation, int now) {
    char request[512];
    int size = snprintf(request, sizeof(request), "{\"version\":1,\"id\":%u,%s}\n", id, operation);
    if (size < 0 || (size_t)size >= sizeof(request)) fail("request size");
    put(fd, request, (size_t)size);
    if (stream_marker) {
        stream_marker = 0;
        /* Allow the arm to be admitted before emitting the prefix. Stream the
         * marker before reading its reply: guest reply polling can take longer
         * than this trigger's deliberately short 100 ms matching deadline. */
        marker_delay(); marker("nee"); marker_delay(); marker("dle\n");
    }
    size_t used = 0;
    for (;;) {
        if (used + 1 == sizeof(reply)) fail("reply size");
        ssize_t count = read(fd, reply + used, 1);
        if (count < 0 && errno == EINTR) continue;
        if (count != 1) fail("reply read");
        if (reply[used++] == '\n') break;
    }
    reply[used] = 0;
    const char *end = value_end(reply, 0);
    if (*space(end)) fail("reply trailing bytes");
    struct value root = {reply, end};
    if (number(field(root, "version")) != 1 || number(field(root, "id")) != id ||
        !literal(field(root, "ok"), "true")) fail("reply identity/result");
    unsigned long long requested = number(field(root, "requested_at_ns"));
    unsigned long long admitted = number(field(root, "admitted_at_ns"));
    unsigned long long replied = number(field(root, "replied_at_ns"));
    struct value effect = field(root, "effect_at_ns");
    if (requested > admitted || admitted > replied) fail("reply causal metadata");
    if (!literal(effect, "null")) {
        unsigned long long applied = number(effect);
        if (applied < admitted || applied > replied) fail("effect causal metadata");
    }
    struct value result = field(root, "result");
    if (now && number(field(result, "now_ns")) != admitted) fail("Now canonical time");
    marker("RPC_REPLY "); put(STDOUT_FILENO, reply, used);
    return result;
}
int main(int argc, char **argv) {
    if (argc != 2) fail("role");
    if (!strcmp(argv[1], "epoch")) {
        struct timespec monotonic, realtime, raw, after;
        if (clock_gettime(CLOCK_MONOTONIC, &monotonic) ||
            clock_gettime(CLOCK_REALTIME, &realtime) ||
            clock_gettime(CLOCK_MONOTONIC_RAW, &raw) ||
            clock_gettime(CLOCK_MONOTONIC, &after)) fail("clock read");
        printf("CLOCKS %llu %llu %llu %llu\n",
            (unsigned long long)monotonic.tv_sec * 1000000000 + monotonic.tv_nsec,
            (unsigned long long)realtime.tv_sec * 1000000000 + realtime.tv_nsec,
            (unsigned long long)raw.tv_sec * 1000000000 + raw.tv_nsec,
            (unsigned long long)after.tv_sec * 1000000000 + after.tv_nsec);
        return 0;
    }
    if (!strcmp(argv[1], "peer")) {
        marker("PEER_RUNNING\n");
        for (;;) pause();
    }
    int sleep_trigger = !strcmp(argv[1], "sleep-trigger-oracle");
    if (!sleep_trigger && strcmp(argv[1], "oracle")) fail("unknown role");
    struct sockaddr_un address = {.sun_family = AF_UNIX};
    const char path[] = "/run/seedbox/control.sock";
    memcpy(address.sun_path, path, sizeof(path));
    int fd = socket(AF_UNIX, SOCK_STREAM, 0);
    if (fd < 0 || connect(fd, (struct sockaddr *)&address, sizeof(address))) fail("connect");
    if (sleep_trigger) {
        stream_marker = 1;
        call(fd, 1, "\"op\":\"arm_trigger\",\"source\":{\"machine\":\"oracle\"},\"marker\":[110,101,101,100,108,101],\"match_timeout_ns\":100000000,\"fault\":{\"action\":\"stop\",\"target\":{\"machine\":\"peer\"}}", 0);
        call(fd, 2, "\"op\":\"sleep\",\"duration_ns\":20000000", 0);
        call(fd, 3, "\"op\":\"sleep\",\"duration_ns\":20000000", 0);
        call(fd, 4, "\"op\":\"now\"", 1); marker("DONE\n");
        if (close(fd)) fail("close");
        return 0;
    }
    call(fd, 1, "\"op\":\"now\"", 1); marker("NOW_OK\n");
    call(fd, 2, "\"op\":\"pause\",\"target\":{\"machine\":\"peer\"}", 0); marker("PAUSED\n");
    struct value result = call(fd, 3, "\"op\":\"throttle\",\"target\":{\"machine\":\"peer\"},\"factor_permille\":2000", 0);
    struct value handle = field(result, "handle");
    char heal[256];
    int size = snprintf(heal, sizeof(heal), "\"op\":\"heal\",\"handle\":%.*s", (int)(handle.end - handle.start), handle.start);
    if (size < 0 || (size_t)size >= sizeof(heal)) fail("handle size");
    marker("THROTTLED\n"); call(fd, 4, heal, 0); marker("HEALED\n");
    call(fd, 5, "\"op\":\"resume\",\"target\":{\"machine\":\"peer\"}", 0); marker("RESUMED\n");
    call(fd, 6, "\"op\":\"stop\",\"target\":{\"machine\":\"peer\"}", 0); marker("STOPPED\n");
    call(fd, 7, "\"op\":\"restart\",\"target\":{\"machine\":\"peer\"}", 0); marker("RESTARTED\n");
    call(fd, 8, "\"op\":\"now\"", 1); marker("DONE\n");
    if (close(fd)) fail("close");
    return 0;
}
