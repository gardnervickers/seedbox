/* Real local socket framing test; not native guest acceptance. */
#define _POSIX_C_SOURCE 200809L
#include "guest_service.h"
#include <assert.h>
#include <errno.h>
#include <poll.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <unistd.h>
struct captured { char bytes[SBX_MANAGEMENT_FRAME_MAX]; size_t used; };
static int capture(void *opaque, const void *bytes, size_t length) {
    struct captured *c = opaque;
    assert(c->used + length <= sizeof(c->bytes));
    memcpy(c->bytes + c->used, bytes, length); c->used += length; return 0;
}
struct fixture { struct sbx_oracle oracle; struct captured captured; int peer; char path[104]; };
static int connect_peer(struct fixture *f) {
    int fd = socket(AF_UNIX, SOCK_STREAM, 0); assert(fd >= 0);
    struct sockaddr_un address = {.sun_family = AF_UNIX};
    assert(strlen(f->path) < sizeof(address.sun_path)); strcpy(address.sun_path, f->path);
    assert(connect(fd, (struct sockaddr *)&address, sizeof(address)) == 0);
    assert(sbx_oracle_accept(&f->oracle) == 0); return fd;
}
static void start(struct fixture *f, const char *directory, unsigned n) {
    memset(f, 0, sizeof(*f));
    assert(snprintf(f->path, sizeof(f->path), "%s/s%u", directory, n) > 0);
    sbx_oracle_init(&f->oracle, capture, &f->captured);
    assert(sbx_oracle_open(&f->oracle, f->path) == 0);
    f->peer = connect_peer(f); assert(f->oracle.session == 1);
}
static void finish(struct fixture *f) {
    if (f->peer >= 0) close(f->peer);
    sbx_oracle_close(&f->oracle); assert(unlink(f->path) == 0);
}
static void send_bytes(struct fixture *f, const void *bytes, size_t length) {
    assert(write(f->peer, bytes, length) == (ssize_t)length);
}
static void request(struct fixture *f) {
    static const char payload[] = "{\"version\":1,\"id\":1,\"op\":false}\n";
    send_bytes(f, payload, sizeof(payload)-1); assert(sbx_oracle_read(&f->oracle) == 0);
}
static void receive_exact(int fd, const void *opaque, size_t length) {
    const unsigned char *expected = opaque; unsigned char bytes[2048];
    while (length) {
        struct pollfd p = {.fd = fd, .events = POLLIN}; assert(poll(&p, 1, 3000) == 1);
        size_t limit = length < sizeof(bytes) ? length : sizeof(bytes);
        ssize_t count = read(fd, bytes, limit); assert(count > 0);
        assert(!memcmp(bytes, expected, (size_t)count)); expected += count; length -= (size_t)count;
    }
}
int main(int argc, char **argv) {
    assert(argc == 2); assert(signal(SIGPIPE, SIG_IGN) != SIG_ERR); alarm(15);
    struct fixture f;
    start(&f, argv[1], 1);
    send_bytes(&f, "{\"version\":1,", 13); assert(sbx_oracle_read(&f.oracle) == 0); assert(f.captured.used == 0);
    static const char tail[] = "\"id\":1,\"op\":false}\n";
    send_bytes(&f, tail, sizeof(tail)-1); assert(sbx_oracle_read(&f.oracle) == 0);
    static const char forwarded[] = "SBX_RPC1 1 1 {\"version\":1,\"id\":1,\"op\":false}\n";
    assert(f.captured.used == sizeof(forwarded)-1 && !memcmp(f.captured.bytes, forwarded, sizeof(forwarded)-1));
    struct sbx_service service; sbx_service_init(&service, NULL, NULL, capture, &f.captured); service.oracle = &f.oracle;
    assert(sbx_service_feed(&service, "SBX_REPLY1 1 ", 13) == 0);
    assert(f.oracle.pending == 1);
    assert(sbx_service_feed(&service, "1 {\"error\":\"invalid op\"}\n", 25) == 0);
    receive_exact(f.peer, "{\"error\":\"invalid op\"}\n", 23);
    assert(f.oracle.pending == 0);
    close(f.peer); f.peer = -1; assert(sbx_oracle_read(&f.oracle) == 0); assert(f.oracle.client == -1);
    f.peer = connect_peer(&f); assert(f.oracle.session == 2); f.captured.used = 0; request(&f);
    assert(!memcmp(f.captured.bytes, "SBX_RPC1 2 2 ", 13));
    /* A reply larger than the old 64-byte app parser uses the same one parser. */
    char reply[5100], expected[5004];
    expected[0] = '"'; memset(expected + 1, 'a', 5000); expected[5001] = '"'; expected[5002] = '\n';
    int size = snprintf(reply, sizeof(reply), "SBX_REPLY1 2 2 %.*s", 5003, expected);
    assert(size > 0 && sbx_service_feed(&service, reply, (size_t)size) == 0);
    receive_exact(f.peer, expected, 5003); finish(&f);

    for (unsigned test = 2; test <= 3; ++test) {
        start(&f, argv[1], test); request(&f);
        const char *stale = test == 2 ? "SBX_REPLY1 1 2 {}" : "SBX_REPLY1 2 1 {}";
        assert(sbx_oracle_reply(&f.oracle, stale) == -1 && f.oracle.poisoned);
        assert(sbx_oracle_reply(&f.oracle, "SBX_REPLY1 1 1 {}") == -1 && errno == EPIPE); finish(&f);
    }
    for (unsigned test = 4; test <= 5; ++test) {
        start(&f, argv[1], test);
        if (test == 4) { send_bytes(&f, "{", 1); assert(sbx_oracle_read(&f.oracle) == 0); }
        else request(&f);
        close(f.peer); f.peer = -1; assert(sbx_oracle_read(&f.oracle) == 0);
        assert(!f.oracle.poisoned && f.oracle.client == -1 && !f.oracle.used && !f.oracle.pending);
        if (test == 5) assert(sbx_oracle_reply(&f.oracle, "SBX_REPLY1 1 1 {}") == 0);
        f.peer = connect_peer(&f); assert(f.oracle.session == 2);
        f.captured.used = 0; request(&f);
        unsigned relay = test == 4 ? 1 : 2;
        assert(f.oracle.pending == relay);
        if (test == 5) assert(sbx_oracle_reply(&f.oracle, "SBX_REPLY1 1 1 {}") == 0);
        char current[64];
        assert(snprintf(current, sizeof(current), "SBX_REPLY1 %u 2 {}", relay) > 0);
        assert(sbx_oracle_reply(&f.oracle, current) == 0);
        receive_exact(f.peer, "{}\n", 3); finish(&f);
    }
    start(&f, argv[1], 6);
    send_bytes(&f, "{}\n{}\n", 6); assert(sbx_oracle_read(&f.oracle) == -1 && errno == EBUSY);
    assert(f.oracle.relay == 1 && f.oracle.pending == 1); finish(&f);
    start(&f, argv[1], 7); f.oracle.relay = UINT32_MAX;
    send_bytes(&f, "{}\n", 3); assert(sbx_oracle_read(&f.oracle) == -1 && errno == EOVERFLOW); finish(&f);
    start(&f, argv[1], 8); close(f.peer); f.peer = -1; assert(sbx_oracle_read(&f.oracle) == 0);
    f.oracle.session = UINT32_MAX;
    int peer = socket(AF_UNIX, SOCK_STREAM, 0); assert(peer >= 0);
    struct sockaddr_un address = {.sun_family = AF_UNIX}; strcpy(address.sun_path, f.path);
    assert(connect(peer, (struct sockaddr *)&address, sizeof(address)) == 0);
    assert(sbx_oracle_accept(&f.oracle) == -1 && errno == EOVERFLOW); close(peer); finish(&f);
    start(&f, argv[1], 9); unsigned char nul = 0; send_bytes(&f, &nul, 1);
    assert(sbx_oracle_read(&f.oracle) == -1 && errno == EINVAL); finish(&f);
    /* A client that does not read must not block management/lifecycle work. */
    start(&f, argv[1], 10); request(&f);
    int buffer = 1024;
    assert(setsockopt(f.oracle.client, SOL_SOCKET, SO_SNDBUF, &buffer, sizeof(buffer)) == 0);
    char full[SBX_ORACLE_FRAME_MAX], command[SBX_MANAGEMENT_FRAME_MAX];
    memset(full, 'a', sizeof(full)); full[sizeof(full)-1] = '\n';
    int prefix = snprintf(command, sizeof(command), "SBX_REPLY1 1 1 ");
    memcpy(command + prefix, full, sizeof(full)-1); command[prefix + sizeof(full)-1] = 0;
    assert(sbx_oracle_reply(&f.oracle, command) == 0);
    assert(f.oracle.pending == 1 && f.oracle.reply_used == sizeof(full));
    assert(f.oracle.reply_cursor > 0 && f.oracle.reply_cursor < f.oracle.reply_used);
    sbx_service_init(&service, NULL, NULL, capture, &f.captured); service.oracle = &f.oracle;
    assert(sbx_service_feed(&service, "shutdown\n", 9) == 0 && service.shutdown_requested);
    size_t received = 0;
    while (received < sizeof(full)) {
        struct pollfd ready = {.fd = f.peer, .events = POLLIN};
        assert(poll(&ready, 1, 3000) == 1);
        unsigned char chunk[1024];
        ssize_t count = read(f.peer, chunk, sizeof(chunk)); assert(count > 0);
        assert(!memcmp(chunk, full + received, (size_t)count)); received += (size_t)count;
        assert(sbx_oracle_flush(&f.oracle) == 0);
    }
    assert(f.oracle.pending == 0 && f.oracle.reply_used == 0 && f.oracle.reply_cursor == 0);
    close(f.peer); f.peer = -1; assert(sbx_oracle_read(&f.oracle) == 0);
    f.peer = connect_peer(&f); assert(f.oracle.session == 2);
    f.captured.used = 0; request(&f); assert(f.oracle.pending == 2);
    finish(&f);
    /* A broken reply destination abandons a partially sent reply. Its suffix
     * cannot leak into a replacement session, whose call remains independent. */
    start(&f, argv[1], 11); request(&f);
    assert(setsockopt(f.oracle.client, SOL_SOCKET, SO_SNDBUF, &buffer, sizeof(buffer)) == 0);
    assert(sbx_oracle_reply(&f.oracle, command) == 0);
    assert(f.oracle.reply_cursor > 0 && f.oracle.reply_cursor < f.oracle.reply_used);
    close(f.peer); f.peer = -1;
    struct pollfd disconnected = {.fd = f.oracle.client, .events = POLLIN | POLLOUT};
    assert(poll(&disconnected, 1, 3000) == 1);
    assert(disconnected.revents & (POLLERR | POLLHUP));
    assert(sbx_oracle_poll(&f.oracle, disconnected.revents) == 0);
    assert(!f.oracle.poisoned && f.oracle.client == -1 && !f.oracle.pending &&
           !f.oracle.reply_used && !f.oracle.reply_cursor);
    f.peer = connect_peer(&f); f.captured.used = 0; request(&f);
    assert(f.oracle.pending == 2);
    assert(sbx_oracle_reply(&f.oracle, "SBX_REPLY1 1 1 {}") == 0);
    assert(sbx_oracle_reply(&f.oracle, "SBX_REPLY1 2 2 {}") == 0);
    receive_exact(f.peer, "{}\n", 3); finish(&f);
    /* Syntax/issued-relay validation still precedes stale-session disposal. */
    start(&f, argv[1], 12); request(&f); close(f.peer); f.peer = -1;
    assert(sbx_oracle_read(&f.oracle) == 0);
    assert(sbx_oracle_reply(&f.oracle, "SBX_REPLY1 1 1 ") == -1 && f.oracle.poisoned);
    finish(&f);
    start(&f, argv[1], 13); request(&f); close(f.peer); f.peer = -1;
    assert(sbx_oracle_read(&f.oracle) == 0);
    assert(sbx_oracle_reply(&f.oracle, "SBX_REPLY1 2 1 {}") == -1 && f.oracle.poisoned);
    finish(&f);
    puts("guest oracle host socket tests passed"); return 0;
}
