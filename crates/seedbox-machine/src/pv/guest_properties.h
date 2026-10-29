/* Guest property evidence relayed on the management channel, beside the
 * application protocol (seedbox_engine_protocol::property and ::crash).
 *
 * Guest programs (seedbox-guest) connect a SOCK_SEQPACKET socket to
 * /run/seedbox/guest.sock; each connection is one session, numbered from 1 per
 * boot, and each message one record of at most SBX_PROPERTY_RECORD_BYTES. Every record is relayed
 * uninterpreted, in arrival order, as
 *   SBX_PROP1 SESSION SEQ LOWERCASE_HEX\n
 * with SEQ counting the session's messages from 1. Session 0 is the
 * supervisor's own: it reports records it could not relay ("lost") and
 * sessions that ended ("closed"). The same sessions are served on the
 * abstract socket "seedbox-guest", for programs that cannot reach the path, and
 * a program that reaches neither reports its loss on /dev/kmsg as
 * "seedbox-guest: lost COUNT property records: REASON", relayed as "lost".
 * The engine answers flush records with
 *   SBX_GUEST1 SESSION JSON\n
 * whose JSON goes back to that session as one message; a session that has
 * gone loses its reply.
 *
 * The kernel's fatal-signal reports (print-fatal-signals) are relayed as
 *   SBX_CRASH1 MESSAGE_HEX\n
 * one per crash. No other kernel line is relayed. */
#ifndef SBX_GUEST_PROPERTIES_H
#define SBX_GUEST_PROPERTIES_H
#include <stddef.h>
#include <stdint.h>

enum {
    SBX_PROPERTY_RECORD_BYTES = 4096,
    /* Concurrent guest connections; another is refused and counted as lost. */
    SBX_PROPERTY_SESSIONS = 64,
    /* Prefix, two decimal fields, separators, hex payload and newline. */
    SBX_PROPERTY_FRAME_BYTES = 64 + 2 * SBX_PROPERTY_RECORD_BYTES,
    /* crash::MAX_MESSAGE_BYTES: a longer report is counted as lost. */
    SBX_CRASH_MESSAGE_BYTES = 1024,
    SBX_CRASH_FRAME_BYTES = 16 + 2 * SBX_CRASH_MESSAGE_BYTES,
    /* Poll entries: the two listeners, then one per session slot. */
    SBX_PROPERTY_POLLED = 2 + SBX_PROPERTY_SESSIONS,
    /* Records relayed per session per wake. */
    SBX_PROPERTY_BUDGET = 32,
    SBX_PROPERTY_REASON_BYTES = 128
};
#define SBX_PROPERTY_SOCKET_ENV "SEEDBOX_GUEST_SOCKET"
#define SBX_PROPERTY_ABSTRACT "seedbox-guest"
#define SBX_PROPERTY_KMSG_LOST "seedbox-guest: lost "

typedef int (*sbx_property_write)(void *sink, const void *bytes, size_t length);

struct sbx_property_session {
    int fd;
    uint32_t session;
    uint64_t seq;
};

struct sbx_properties {
    int listener, abstract_listener;
    struct sbx_property_session sessions[SBX_PROPERTY_SESSIONS];
    /* The last session issued in this boot. */
    uint32_t issued;
    uint64_t supervisor_seq;
    sbx_property_write write;
    void *sink;
};

/* One SBX_PROP1 frame; zero if the record is empty or too large. */
size_t sbx_property_frame(char frame[SBX_PROPERTY_FRAME_BYTES], uint32_t session,
                          uint64_t seq, const unsigned char *record, size_t size);
/* Frame one /dev/kmsg record ("PRIO,SEQ,USEC,FLAGS[,...];MESSAGE\n...") if it
 * is the kernel's own fatal-signal report. Returns the frame size, zero for
 * every other record (including text userspace wrote to /dev/kmsg), or
 * (size_t)-1 for a report too long to relay. */
size_t sbx_crash_frame(const char *record, size_t size, char frame[SBX_CRASH_FRAME_BYTES]);

void sbx_properties_init(struct sbx_properties *, sbx_property_write, void *sink);
/* Bind the listener and, on Linux, the abstract one; the caller owns the
 * parent directory. */
int sbx_properties_open(struct sbx_properties *, const char *path);
/* Accept every pending connection and relay every queued record, without
 * waiting for more. A refused connection or oversized record is reported
 * lost on the supervisor's session. 0, or -1 with errno on relay failure. */
int sbx_properties_drain(struct sbx_properties *);
/* Handle poll results for the entries sbx_properties_fd named, in order:
 * relay ready sessions' records (at most SBX_PROPERTY_BUDGET each), then
 * accept new connections. */
int sbx_properties_poll(struct sbx_properties *, const short revents[SBX_PROPERTY_POLLED]);
/* Whether one /dev/kmsg record is a program's report of undeliverable
 * records; if so its count and (unescaped, truncated) reason. */
int sbx_guest_lost_line(const char *record, size_t size, uint64_t *count,
                      char reason[SBX_PROPERTY_REASON_BYTES]);
/* Report records the supervisor itself failed to relay. */
int sbx_properties_lost(struct sbx_properties *, const char *reason, uint64_t count);
/* One complete SBX_GUEST1 management line, without its newline. Malformed lines
 * fail with EINVAL; a reply to a session that has gone is discarded. */
int sbx_properties_reply(struct sbx_properties *, const char *line);
/* Poll entries: the listeners first, then one per session slot (-1 if idle). */
int sbx_properties_fd(const struct sbx_properties *, unsigned index);
void sbx_properties_close(struct sbx_properties *);
#endif
