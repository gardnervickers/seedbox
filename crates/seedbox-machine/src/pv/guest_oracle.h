/* Private guest-side oracle socket relay; no oracle operation schema. */
#ifndef SBX_GUEST_ORACLE_H
#define SBX_GUEST_ORACLE_H
#include <stddef.h>
#include <stdint.h>
#define SBX_ORACLE_FRAME_MAX 16384
#define SBX_MANAGEMENT_FRAME_MAX (SBX_ORACLE_FRAME_MAX + 64)
typedef int (*sbx_oracle_write)(void *, const void *, size_t);
struct sbx_oracle {
    int listener, client, poisoned;
    uint32_t relay, session, pending;
    unsigned char partial[SBX_ORACLE_FRAME_MAX];
    size_t used;
    unsigned char reply[SBX_ORACLE_FRAME_MAX];
    size_t reply_used, reply_cursor;
    sbx_oracle_write write;
    void *sink;
};
void sbx_oracle_init(struct sbx_oracle *, sbx_oracle_write, void *);
/* Caller selects listener exposure and owns its parent directories. */
int sbx_oracle_open(struct sbx_oracle *, const char *path);
int sbx_oracle_accept(struct sbx_oracle *);
/* Nonblocking read: disconnect abandons local partial/reply state and permits
 * reconnect; pipelining fails closed. Relay ID persists across connections;
 * session advances on each accept. Public JSON identity is validated by host. */
int sbx_oracle_read(struct sbx_oracle *);
/* One complete management line without newline, routed by the sole management
 * decoder. Match relay/session before returning opaque payload to the socket.
 * Well-framed historical replies are discarded after caller disconnect. */
int sbx_oracle_reply(struct sbx_oracle *, const char *line);
/* Drain retained reply bytes without waiting; poll client POLLOUT while
 * reply_used is nonzero. The request remains outstanding until fully sent. */
int sbx_oracle_flush(struct sbx_oracle *);
/* Dispatch client poll events through nonblocking read/flush. */
int sbx_oracle_poll(struct sbx_oracle *, short events);
void sbx_oracle_close(struct sbx_oracle *);
#endif
