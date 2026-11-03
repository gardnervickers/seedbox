#define _POSIX_C_SOURCE 200809L
#include "guest_properties.h"
#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <stddef.h>
#include <stdio.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/un.h>
#include <unistd.h>

#ifndef MSG_NOSIGNAL
#define MSG_NOSIGNAL 0
#endif

static const char hex[] = "0123456789abcdef";

size_t sbx_property_frame(char frame[SBX_PROPERTY_FRAME_BYTES], uint32_t session,
                          uint64_t seq, const unsigned char *record, size_t size) {
    if (!size || size > SBX_PROPERTY_RECORD_BYTES) return 0;
    int used = snprintf(frame, SBX_PROPERTY_FRAME_BYTES, "SBX_PROP1 %u %llu ", session,
                        (unsigned long long)seq);
    if (used < 0 || (size_t)used + 2 * size + 1 > SBX_PROPERTY_FRAME_BYTES) return 0;
    for (size_t i = 0; i < size; ++i) {
        frame[used++] = hex[record[i] >> 4];
        frame[used++] = hex[record[i] & 15];
    }
    frame[used++] = '\n';
    return (size_t)used;
}

static const char fatal_signal[] = ": potentially unexpected fatal signal ";

size_t sbx_crash_frame(const char *record, size_t size, char frame[SBX_CRASH_FRAME_BYTES]) {
    const char *end = record + size, *at = record;
    unsigned priority = 0;
    /* Userspace records carry a nonzero facility; the kernel's are bare levels. */
    while (at < end && *at >= '0' && *at <= '9' && priority <= 7) priority = priority * 10 + (unsigned)(*at++ - '0');
    if (at == record || priority > 7 || at == end || *at != ',') return 0;
    const char *message = memchr(record, ';', size);
    if (!message) return 0;
    ++message;
    const char *line_end = memchr(message, '\n', (size_t)(end - message));
    if (line_end) end = line_end;
    /* [EXECUTABLE: ]COMMAND: potentially unexpected fatal signal N. */
    size_t marker = sizeof(fatal_signal) - 1;
    const char *found = NULL;
    for (const char *p = end; p - message >= (ptrdiff_t)marker; --p) {
        if (!memcmp(p - marker, fatal_signal, marker)) { found = p - marker; break; }
    }
    if (!found || found == message) return 0;
    const char *number = found + marker;
    if (end - number < 2 || end[-1] != '.' || *number == '0') return 0;
    unsigned signal = 0;
    for (const char *p = number; p < end - 1; ++p) {
        if (*p < '0' || *p > '9' || signal > 64) return 0;
        signal = signal * 10 + (unsigned)(*p - '0');
    }
    if (signal < 1 || signal > 64) return 0;
    /* kmsg escapes every unprintable byte, so a report is printable ASCII. */
    for (const char *p = message; p < end; ++p)
        if ((unsigned char)*p < 0x20 || (unsigned char)*p >= 0x7f) return 0;
    size_t length = (size_t)(end - message);
    if (length > SBX_CRASH_MESSAGE_BYTES) return (size_t)-1;
    static const char prefix[] = "SBX_CRASH1 ";
    size_t used = sizeof(prefix) - 1;
    memcpy(frame, prefix, used);
    for (size_t i = 0; i < length; ++i) {
        frame[used++] = hex[(unsigned char)message[i] >> 4];
        frame[used++] = hex[(unsigned char)message[i] & 15];
    }
    frame[used++] = '\n';
    return used;
}

void sbx_properties_init(struct sbx_properties *p, sbx_property_write write, void *sink) {
    memset(p, 0, sizeof(*p));
    p->listener = p->abstract_listener = -1;
    for (unsigned i = 0; i < SBX_PROPERTY_SESSIONS; ++i) p->sessions[i].fd = -1;
    p->write = write;
    p->sink = sink;
}

static int nonblocking(int fd) {
    int status = fcntl(fd, F_GETFL);
    if (status < 0 || fcntl(fd, F_SETFL, status | O_NONBLOCK) < 0 ||
        fcntl(fd, F_SETFD, FD_CLOEXEC) < 0) return -1;
    return 0;
}

int sbx_properties_open(struct sbx_properties *p, const char *path) {
    if (p->listener >= 0) { errno = EALREADY; return -1; }
    struct sockaddr_un address = {.sun_family = AF_UNIX};
    if (strlen(path) >= sizeof(address.sun_path)) { errno = ENAMETOOLONG; return -1; }
    strcpy(address.sun_path, path);
    if (unlink(path) && errno != ENOENT) return -1;
    int listener = socket(AF_UNIX, SOCK_SEQPACKET, 0);
    if (listener < 0) return -1;
    /* Applications run as any user; the socket only admits records. */
    if (nonblocking(listener) || bind(listener, (struct sockaddr *)&address, sizeof(address)) ||
        chmod(path, 0666) || listen(listener, SBX_PROPERTY_SESSIONS)) {
        int error = errno;
        close(listener);
        errno = error;
        return -1;
    }
    p->listener = listener;
#ifdef __linux__
    /* The abstract namespace has no files or permissions: programs that
     * cannot reach the path (a mount over /run) still reach this. */
    struct sockaddr_un hidden = {.sun_family = AF_UNIX};
    size_t name = strlen(SBX_PROPERTY_ABSTRACT);
    memcpy(hidden.sun_path + 1, SBX_PROPERTY_ABSTRACT, name);
    int fallback = socket(AF_UNIX, SOCK_SEQPACKET, 0);
    if (fallback < 0) return -1;
    if (nonblocking(fallback) ||
        bind(fallback, (struct sockaddr *)&hidden,
             (socklen_t)(offsetof(struct sockaddr_un, sun_path) + 1 + name)) ||
        listen(fallback, SBX_PROPERTY_SESSIONS)) {
        int error = errno;
        close(fallback);
        errno = error;
        return -1;
    }
    p->abstract_listener = fallback;
#endif
    return 0;
}

/* Supervisor records interpolate only these characters, so they stay JSON. */
static void plain(char *text) {
    for (; *text; ++text) {
        unsigned char c = (unsigned char)*text;
        if (!(c >= 'a' && c <= 'z') && !(c >= 'A' && c <= 'Z') && !(c >= '0' && c <= '9') &&
            !strchr(" .,:;_-/()=+", c)) *text = '?';
    }
}

static int relay(struct sbx_properties *p, uint32_t session, uint64_t seq,
                 const unsigned char *record, size_t size) {
    char frame[SBX_PROPERTY_FRAME_BYTES];
    size_t length = sbx_property_frame(frame, session, seq, record, size);
    if (!length) { errno = E2BIG; return -1; }
    return p->write(p->sink, frame, length);
}

int sbx_properties_lost(struct sbx_properties *p, const char *reason, uint64_t count) {
    char record[512], text[256];
    snprintf(text, sizeof(text), "%s", reason);
    plain(text);
    int size = snprintf(record, sizeof(record), "{\"lost\":{\"reason\":\"%s\",\"count\":%llu}}",
                        text, (unsigned long long)count);
    if (size < 0 || (size_t)size >= sizeof(record)) { errno = E2BIG; return -1; }
    return relay(p, 0, ++p->supervisor_seq, (const unsigned char *)record, (size_t)size);
}

static int accept_from(struct sbx_properties *p, int listener) {
    if (listener < 0) return 0;
    for (;;) {
        int client = accept(listener, NULL, NULL);
        if (client < 0) {
            if (errno == EAGAIN || errno == EWOULDBLOCK || errno == EINTR || errno == ECONNABORTED) return 0;
            return -1;
        }
        struct sbx_property_session *slot = NULL;
        for (unsigned i = 0; i < SBX_PROPERTY_SESSIONS && !slot; ++i)
            if (p->sessions[i].fd < 0) slot = &p->sessions[i];
        /* A refused connection may have records queued: never silently ignored. */
        if (!slot) {
            close(client);
            if (sbx_properties_lost(p, "too many guest connections", 1)) return -1;
            continue;
        }
        if (p->issued == UINT32_MAX) { close(client); errno = EOVERFLOW; return -1; }
        if (nonblocking(client)) { int error = errno; close(client); errno = error; return -1; }
        *slot = (struct sbx_property_session){.fd = client, .session = ++p->issued};
    }
}

static int accept_all(struct sbx_properties *p) {
    return accept_from(p, p->listener) || accept_from(p, p->abstract_listener) ? -1 : 0;
}

/* End a session and tell the engine its sequence will not continue. */
static int close_session(struct sbx_properties *p, struct sbx_property_session *s) {
    close(s->fd);
    s->fd = -1;
    char record[64];
    int size = snprintf(record, sizeof(record), "{\"closed\":{\"session\":%u}}", s->session);
    if (size < 0 || (size_t)size >= sizeof(record)) { errno = E2BIG; return -1; }
    return relay(p, 0, ++p->supervisor_seq, (const unsigned char *)record, (size_t)size);
}

/* Relay at most `budget` records (0: all queued), so one busy session cannot
 * starve management input or the kernel log. */
static int read_session(struct sbx_properties *p, struct sbx_property_session *s, unsigned budget) {
    unsigned char record[SBX_PROPERTY_RECORD_BYTES];
    for (unsigned n = 0; s->fd >= 0 && (!budget || n < budget); ++n) {
        ssize_t received = recv(s->fd, record, sizeof(record), MSG_TRUNC);
        if (received < 0 && errno == EINTR) continue;
        if (received < 0 && (errno == EAGAIN || errno == EWOULDBLOCK)) return 0;
        /* A seqpacket EOF (or a zero-length message) ends the session. */
        if (!received || (received < 0 && errno == ECONNRESET)) return close_session(p, s);
        if (received < 0) return -1;
        /* The guest library counted it, so the session's sequence counts it
         * too: the engine sees the gap and later replies still match. */
        ++s->seq;
        if ((size_t)received > sizeof(record)) {
            if (sbx_properties_lost(p, "guest record too large", 1)) return -1;
            continue;
        }
        if (relay(p, s->session, s->seq, record, (size_t)received)) return -1;
    }
    return 0;
}

static int read_all(struct sbx_properties *p) {
    for (unsigned i = 0; i < SBX_PROPERTY_SESSIONS; ++i)
        if (read_session(p, &p->sessions[i], 0)) return -1;
    return 0;
}

/* Sessions first, so peers that have closed free their slots before new
 * connections are counted against them. */
int sbx_properties_drain(struct sbx_properties *p) {
    return read_all(p) || accept_all(p) || read_all(p) ? -1 : 0;
}

int sbx_properties_poll(struct sbx_properties *p, const short revents[SBX_PROPERTY_POLLED]) {
    for (unsigned i = 0; i < SBX_PROPERTY_SESSIONS; ++i) {
        if (p->sessions[i].fd < 0) continue;
        if (revents[2 + i] & POLLNVAL) { errno = EIO; return -1; }
        if (revents[2 + i] & (POLLIN | POLLHUP | POLLERR))
            if (read_session(p, &p->sessions[i], SBX_PROPERTY_BUDGET)) return -1;
    }
    for (unsigned i = 0; i < 2; ++i) {
        if (revents[i] & (POLLERR | POLLNVAL)) { errno = EIO; return -1; }
        if ((revents[i] & POLLIN) && accept_from(p, i ? p->abstract_listener : p->listener)) return -1;
    }
    return 0;
}

int sbx_guest_lost_line(const char *record, size_t size, uint64_t *count,
                      char reason[SBX_PROPERTY_REASON_BYTES]) {
    const char *end = record + size, *at = record;
    unsigned priority = 0;
    while (at < end && *at >= '0' && *at <= '9' && priority < 1024) priority = priority * 10 + (unsigned)(*at++ - '0');
    /* Programs write at a nonzero facility; the kernel never writes this. */
    if (at == record || priority <= 7 || at == end || *at != ',') return 0;
    const char *message = memchr(record, ';', size);
    if (!message) return 0;
    ++message;
    const char *line_end = memchr(message, '\n', (size_t)(end - message));
    if (line_end) end = line_end;
    static const char prefix[] = SBX_PROPERTY_KMSG_LOST;
    size_t length = sizeof(prefix) - 1;
    if ((size_t)(end - message) < length || memcmp(message, prefix, length)) return 0;
    at = message + length;
    uint64_t value = 0;
    const char *digits = at;
    while (at < end && *at >= '0' && *at <= '9' && value < UINT64_MAX / 10) value = value * 10 + (uint64_t)(*at++ - '0');
    if (at == digits) return 0;
    size_t kept = (size_t)(end - at) < SBX_PROPERTY_REASON_BYTES - 1 ? (size_t)(end - at) : SBX_PROPERTY_REASON_BYTES - 1;
    memcpy(reason, at, kept);
    reason[kept] = 0;
    *count = value ? value : 1;
    return 1;
}

int sbx_properties_reply(struct sbx_properties *p, const char *line) {
    static const char prefix[] = "SBX_GUEST1 ";
    if (strncmp(line, prefix, sizeof(prefix) - 1)) { errno = EINVAL; return -1; }
    const char *at = line + sizeof(prefix) - 1;
    uint64_t session = 0;
    if (*at < '1' || *at > '9') { errno = EINVAL; return -1; }
    for (; *at >= '0' && *at <= '9'; ++at) {
        session = session * 10 + (uint64_t)(*at - '0');
        if (session > UINT32_MAX) { errno = EINVAL; return -1; }
    }
    if (*at++ != ' ' || !*at) { errno = EINVAL; return -1; }
    for (unsigned i = 0; i < SBX_PROPERTY_SESSIONS; ++i) {
        struct sbx_property_session *s = &p->sessions[i];
        if (s->fd < 0 || s->session != (uint32_t)session) continue;
        ssize_t sent;
        do {
            sent = send(s->fd, at, strlen(at), MSG_NOSIGNAL);
        } while (sent < 0 && errno == EINTR);
        /* Sessions are nonblocking. The engine has the record; a session
         * that cannot take its reply times out on its own. */
        return 0;
    }
    return 0;
}

int sbx_properties_fd(const struct sbx_properties *p, unsigned index) {
    if (index < 2) return index ? p->abstract_listener : p->listener;
    return index < SBX_PROPERTY_POLLED ? p->sessions[index - 2].fd : -1;
}

void sbx_properties_close(struct sbx_properties *p) {
    if (p->listener >= 0) close(p->listener);
    if (p->abstract_listener >= 0) close(p->abstract_listener);
    p->listener = p->abstract_listener = -1;
    for (unsigned i = 0; i < SBX_PROPERTY_SESSIONS; ++i) {
        if (p->sessions[i].fd >= 0) close(p->sessions[i].fd);
        p->sessions[i].fd = -1;
    }
}
