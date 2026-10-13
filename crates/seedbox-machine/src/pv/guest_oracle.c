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
/* Losing a caller only abandons its transport destination, not host work. */
static int abandon(struct sbx_oracle *s) {
    if (s->client >= 0) close(s->client);
    s->client = -1;
    s->used = s->pending = s->reply_used = s->reply_cursor = 0;
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
    memset(s, 0, sizeof(*s)); s->listener = s->client = -1; s->write = write; s->sink = sink;
}
void sbx_oracle_close(struct sbx_oracle *s) {
    if (s->listener >= 0) close(s->listener);
    if (s->client >= 0) close(s->client);
    s->listener = s->client = -1;
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
        chmod(path, 0666) || listen(s->listener, 1)) return fail(s, errno);
    return 0;
}
int sbx_oracle_accept(struct sbx_oracle *s) {
    if (s->poisoned) { errno = EPIPE; return -1; }
    if (s->client >= 0) return 0;
    int client = accept(s->listener, NULL, NULL);
    if (client < 0) {
        if (errno == EAGAIN || errno == EWOULDBLOCK || errno == EINTR) return 0;
        return fail(s, errno);
    }
    if (s->session == UINT32_MAX) { close(client); return fail(s, EOVERFLOW); }
    if (flags(client)) { int error = errno; close(client); return fail(s, error); }
    s->session++; s->client = client; return 0;
}
int sbx_oracle_read(struct sbx_oracle *s) {
    if (s->poisoned) { errno = EPIPE; return -1; }
    if (s->client < 0) return 0;
    unsigned char bytes[4096];
    for (;;) {
        ssize_t count = read(s->client, bytes, sizeof(bytes));
        if (count < 0 && errno == EINTR) continue;
        if (count < 0 && (errno == EAGAIN || errno == EWOULDBLOCK)) return 0;
        if (count < 0) return disconnected(errno) ? abandon(s) : fail(s, errno);
        if (!count) return abandon(s);
        for (ssize_t i = 0; i < count; ++i) {
            if (s->pending) return fail(s, EBUSY);
            if (bytes[i] == '\n') {
                if (!s->used) return fail(s, EINVAL);
                if (s->relay == UINT32_MAX) return fail(s, EOVERFLOW);
                s->pending = ++s->relay;
                char frame[SBX_MANAGEMENT_FRAME_MAX];
                int prefix = snprintf(frame, sizeof(frame), "SBX_RPC1 %u %u ", s->pending, s->session);
                if (prefix < 0 || (size_t)prefix + s->used + 1 > sizeof(frame)) return fail(s, E2BIG);
                memcpy(frame + prefix, s->partial, s->used); frame[prefix + s->used] = '\n';
                if (s->write(s->sink, frame, (size_t)prefix + s->used + 1)) return fail(s, errno);
                s->used = 0;
            } else {
                if (!bytes[i]) return fail(s, EINVAL);
                if (s->used + 1 >= sizeof(s->partial)) return fail(s, E2BIG);
                s->partial[s->used++] = bytes[i];
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
int sbx_oracle_flush(struct sbx_oracle *s) {
    if (s->poisoned) { errno = EPIPE; return -1; }
    while (s->reply_cursor < s->reply_used) {
        ssize_t count = write(s->client, s->reply + s->reply_cursor,
                              s->reply_used - s->reply_cursor);
        if (count < 0 && errno == EINTR) continue;
        if (count < 0 && (errno == EAGAIN || errno == EWOULDBLOCK)) return 0;
        if (count <= 0) {
            int error = count ? errno : EPIPE;
            return disconnected(error) ? abandon(s) : fail(s, error);
        }
        s->reply_cursor += (size_t)count;
    }
    if (s->reply_used) {
        s->reply_used = s->reply_cursor = 0;
        s->pending = 0;
    }
    return 0;
}
/* Shared by init and real-socket tests: client hangup/error belongs to the
 * caller lifetime; invalid descriptors remain supervisor faults. */
int sbx_oracle_poll(struct sbx_oracle *s, short events) {
    if (events & POLLNVAL) return fail(s, EBADF);
    if (events & (POLLIN | POLLHUP | POLLERR))
        if (sbx_oracle_read(s)) return -1;
    if (events & POLLOUT) return sbx_oracle_flush(s);
    return 0;
}
int sbx_oracle_reply(struct sbx_oracle *s, const char *line) {
    if (s->poisoned) { errno = EPIPE; return -1; }
    if (strncmp(line, "SBX_REPLY1 ", 11)) return fail(s, EINVAL);
    const char *payload = line + 11; uint32_t relay, session;
    if (number(&payload, &relay) || number(&payload, &session) || !*payload ||
        strchr(payload, '\n') || strlen(payload) + 1 > SBX_ORACLE_FRAME_MAX) return fail(s, EINVAL);
    /* Validate syntax first. An issued reply for a disconnected session cannot
     * acquire a replacement socket, including before that socket is accepted. */
    if (relay <= s->relay &&
        (session < s->session || (session == s->session && s->client < 0))) return 0;
    if (s->reply_used || !s->pending || relay != s->pending ||
        session != s->session || s->client < 0) return fail(s, EINVAL);
    s->reply_used = strlen(payload) + 1;
    s->reply_cursor = 0;
    memcpy(s->reply, payload, s->reply_used - 1);
    s->reply[s->reply_used - 1] = '\n';
    return sbx_oracle_flush(s);
}
