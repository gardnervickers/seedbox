/* Private guest-side oracle socket relay; no oracle operation schema. */
#ifndef SBX_GUEST_ORACLE_H
#define SBX_GUEST_ORACLE_H
#include <stddef.h>
#include <stdint.h>
#define SBX_ORACLE_FRAME_MAX 16384
#define SBX_MANAGEMENT_FRAME_MAX (SBX_ORACLE_FRAME_MAX + 64)
/* Concurrent oracle connections; the host decoder enforces the same bound. */
#define SBX_ORACLE_CLIENTS 8
typedef int (*sbx_oracle_write)(void *, const void *, size_t);
/* One connection. Its calls are sequential: one request in flight at a time. */
struct sbx_oracle_client {
    int fd, spoke;
    uint32_t session, pending;
    unsigned char partial[SBX_ORACLE_FRAME_MAX];
    size_t used;
    unsigned char reply[SBX_ORACLE_FRAME_MAX];
    size_t reply_used, reply_cursor;
};
struct sbx_oracle {
    int listener, poisoned;
    uint32_t relay, session;
    struct sbx_oracle_client clients[SBX_ORACLE_CLIENTS];
    sbx_oracle_write write;
    void *sink;
};
void sbx_oracle_init(struct sbx_oracle *, sbx_oracle_write, void *);
/* Caller selects listener exposure and owns its parent directories. */
int sbx_oracle_open(struct sbx_oracle *, const char *path);
/* Nonzero while every client slot is in use; poll the listener only when zero. */
int sbx_oracle_full(const struct sbx_oracle *);
/* Accept one pending connection into a free slot; its session is numbered
 * by its first request. */
int sbx_oracle_accept(struct sbx_oracle *);
/* Nonblocking read of one slot: disconnect abandons that slot's partial/reply
 * state, frees it, and tells the host with SBX_CLOSE1 once the session has
 * sent a request; pipelining within a session fails closed. Relay IDs are
 * shared by all sessions; session advances on each session's first request.
 * Public JSON identity is validated by host. */
int sbx_oracle_read(struct sbx_oracle *, unsigned slot);
/* One complete management line without newline, routed by the sole management
 * decoder to the slot holding its session. Match relay/session before
 * returning opaque payload to the socket. Well-framed replies for a closed
 * session are discarded. */
int sbx_oracle_reply(struct sbx_oracle *, const char *line);
/* Drain one slot's retained reply bytes without waiting; poll its POLLOUT
 * while reply_used is nonzero. The request remains outstanding until sent. */
int sbx_oracle_flush(struct sbx_oracle *, unsigned slot);
/* Dispatch one slot's poll events through nonblocking read/flush. */
int sbx_oracle_poll(struct sbx_oracle *, unsigned slot, short events);
void sbx_oracle_close(struct sbx_oracle *);
#endif
