#define _POSIX_C_SOURCE 200809L
#include "guest_oracle.h"
#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <stdio.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/un.h>
#include <unistd.h>

static int fail(struct sbx_oracle *s, int error) { s->poisoned = 1; errno = error; return -1; }
static void reset(struct sbx_oracle_client *c) {
    c->fd = -1; c->spoke = 0; c->session = c->pending = 0;
    c->used = c->reply_used = c->reply_cursor = 0;
}
/* Losing a caller only abandons its transport destination, not host work. The
 * host learns of the closure only for sessions it has seen. */
static int abandon(struct sbx_oracle *s, struct sbx_oracle_client *c) {
    if (c->fd >= 0) close(c->fd);
    int spoke = c->spoke; uint32_t session = c->session;
    reset(c);
    if (!spoke) return 0;
    char frame[32];
    int length = snprintf(frame, sizeof(frame), "SBX_CLOSE1 %u\n", session);
    if (length < 0 || (size_t)length >= sizeof(frame)) return fail(s, E2BIG);
    if (s->write(s->sink, frame, (size_t)length)) return fail(s, errno);
    return 0;
}
static int disconnected(int error) { return error == EPIPE || error == ECONNRESET; }
static int flags(int fd) {
    int status = fcntl(fd, F_GETFL);
    if (status < 0 || fcntl(fd, F_SETFL, status | O_NONBLOCK) < 0 ||
        fcntl(fd, F_SETFD, FD_CLOEXEC) < 0) return -1;
    return 0;
}
void sbx_oracle_init(struct sbx_oracle *s, sbx_oracle_write write, void *sink) {
    memset(s, 0, sizeof(*s)); s->listener = -1; s->write = write; s->sink = sink;
    for (unsigned i = 0; i < SBX_ORACLE_CLIENTS; ++i) reset(&s->clients[i]);
}
void sbx_oracle_close(struct sbx_oracle *s) {
    if (s->listener >= 0) close(s->listener);
    s->listener = -1;
    for (unsigned i = 0; i < SBX_ORACLE_CLIENTS; ++i) {
        if (s->clients[i].fd >= 0) close(s->clients[i].fd);
        reset(&s->clients[i]);
    }
}
int sbx_oracle_open(struct sbx_oracle *s, const char *path) {
    if (s->poisoned) { errno = EPIPE; return -1; }
    if (s->listener >= 0) return fail(s, EALREADY);
    struct sockaddr_un address = {.sun_family = AF_UNIX};
    if (strlen(path) >= sizeof(address.sun_path)) return fail(s, ENAMETOOLONG);
    s->listener = socket(AF_UNIX, SOCK_STREAM, 0);
    if (s->listener < 0) return fail(s, errno);
    strcpy(address.sun_path, path);
    if (flags(s->listener) || bind(s->listener, (struct sockaddr *)&address, sizeof(address)) ||
        chmod(path, 0666) || listen(s->listener, SBX_ORACLE_CLIENTS)) return fail(s, errno);
    return 0;
}
static struct sbx_oracle_client *free_slot(struct sbx_oracle *s) {
    for (unsigned i = 0; i < SBX_ORACLE_CLIENTS; ++i)
        if (s->clients[i].fd < 0) return &s->clients[i];
    return NULL;
}
int sbx_oracle_full(const struct sbx_oracle *s) {
    for (unsigned i = 0; i < SBX_ORACLE_CLIENTS; ++i)
        if (s->clients[i].fd < 0) return 0;
    return 1;
}
int sbx_oracle_accept(struct sbx_oracle *s) {
    if (s->poisoned) { errno = EPIPE; return -1; }
    struct sbx_oracle_client *c = free_slot(s);
    if (!c) return 0;
    int client = accept(s->listener, NULL, NULL);
    if (client < 0) {
        if (errno == EAGAIN || errno == EWOULDBLOCK || errno == EINTR) return 0;
        return fail(s, errno);
    }
    if (flags(client)) { int error = errno; close(client); return fail(s, error); }
    c->fd = client; return 0;
}
int sbx_oracle_read(struct sbx_oracle *s, unsigned slot) {
    if (s->poisoned) { errno = EPIPE; return -1; }
    if (slot >= SBX_ORACLE_CLIENTS) return fail(s, EINVAL);
    struct sbx_oracle_client *c = &s->clients[slot];
    if (c->fd < 0) return 0;
    unsigned char bytes[4096];
    for (;;) {
        ssize_t count = read(c->fd, bytes, sizeof(bytes));
        if (count < 0 && errno == EINTR) continue;
        if (count < 0 && (errno == EAGAIN || errno == EWOULDBLOCK)) return 0;
        if (count < 0) return disconnected(errno) ? abandon(s, c) : fail(s, errno);
        if (!count) return abandon(s, c);
        for (ssize_t i = 0; i < count; ++i) {
            if (c->pending) return fail(s, EBUSY);
            if (bytes[i] == '\n') {
                if (!c->used) return fail(s, EINVAL);
                if (s->relay == UINT32_MAX) return fail(s, EOVERFLOW);
                /* The host learns of a session from its first request, so
                 * number it here: first-request order stays ascending however
                 * the connections were accepted. */
                if (!c->spoke) {
                    if (s->session == UINT32_MAX) return fail(s, EOVERFLOW);
                    c->session = ++s->session;
                }
                c->pending = ++s->relay;
                char frame[SBX_MANAGEMENT_FRAME_MAX];
                int prefix = snprintf(frame, sizeof(frame), "SBX_RPC1 %u %u ", c->pending, c->session);
                if (prefix < 0 || (size_t)prefix + c->used + 1 > sizeof(frame)) return fail(s, E2BIG);
                memcpy(frame + prefix, c->partial, c->used); frame[prefix + c->used] = '\n';
                if (s->write(s->sink, frame, (size_t)prefix + c->used + 1)) return fail(s, errno);
                c->spoke = 1; c->used = 0;
            } else {
                if (!bytes[i]) return fail(s, EINVAL);
                if (c->used + 1 >= sizeof(c->partial)) return fail(s, E2BIG);
                c->partial[c->used++] = bytes[i];
            }
        }
    }
}
static int number(const char **cursor, uint32_t *value) {
    const char *p = *cursor;
    if (*p < '1' || *p > '9') return -1;
    uint32_t n = 0;
    while (*p >= '0' && *p <= '9') {
        unsigned digit = (unsigned)(*p++ - '0');
        if (n > (UINT32_MAX - digit) / 10) return -1;
        n = n * 10 + digit;
    }
    if (*p++ != ' ') return -1;
    *cursor = p; *value = n; return 0;
}
int sbx_oracle_flush(struct sbx_oracle *s, unsigned slot) {
    if (s->poisoned) { errno = EPIPE; return -1; }
    if (slot >= SBX_ORACLE_CLIENTS) return fail(s, EINVAL);
    struct sbx_oracle_client *c = &s->clients[slot];
    while (c->reply_cursor < c->reply_used) {
        ssize_t count = write(c->fd, c->reply + c->reply_cursor,
                              c->reply_used - c->reply_cursor);
        if (count < 0 && errno == EINTR) continue;
        if (count < 0 && (errno == EAGAIN || errno == EWOULDBLOCK)) return 0;
        if (count <= 0) {
            int error = count ? errno : EPIPE;
            return disconnected(error) ? abandon(s, c) : fail(s, error);
        }
        c->reply_cursor += (size_t)count;
    }
    if (c->reply_used) {
        c->reply_used = c->reply_cursor = 0;
        c->pending = 0;
    }
    return 0;
}
/* Shared by init and real-socket tests: client hangup/error belongs to the
 * caller lifetime; invalid descriptors remain supervisor faults. */
int sbx_oracle_poll(struct sbx_oracle *s, unsigned slot, short events) {
    if (events & POLLNVAL) return fail(s, EBADF);
    if (events & (POLLIN | POLLHUP | POLLERR))
        if (sbx_oracle_read(s, slot)) return -1;
    if (events & POLLOUT) return sbx_oracle_flush(s, slot);
    return 0;
}
int sbx_oracle_reply(struct sbx_oracle *s, const char *line) {
    if (s->poisoned) { errno = EPIPE; return -1; }
    if (strncmp(line, "SBX_REPLY1 ", 11)) return fail(s, EINVAL);
    const char *payload = line + 11; uint32_t relay, session;
    if (number(&payload, &relay) || number(&payload, &session) || !*payload ||
        strchr(payload, '\n') || strlen(payload) + 1 > SBX_ORACLE_FRAME_MAX) return fail(s, EINVAL);
    unsigned slot = 0;
    while (slot < SBX_ORACLE_CLIENTS &&
           (s->clients[slot].fd < 0 || s->clients[slot].session != session)) ++slot;
    /* Validate syntax first. An issued reply for a closed session cannot reach
     * any other session's socket. */
    if (slot == SBX_ORACLE_CLIENTS)
        return relay <= s->relay && session <= s->session ? 0 : fail(s, EINVAL);
    struct sbx_oracle_client *c = &s->clients[slot];
    if (c->reply_used || !c->pending || relay != c->pending) return fail(s, EINVAL);
    c->reply_used = strlen(payload) + 1;
    c->reply_cursor = 0;
    memcpy(c->reply, payload, c->reply_used - 1);
    c->reply[c->reply_used - 1] = '\n';
    return sbx_oracle_flush(s, slot);
}
