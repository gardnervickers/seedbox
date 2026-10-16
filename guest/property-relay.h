/* Guest property records. Applications send each SDK message as one
 * SOCK_SEQPACKET datagram; the supervisor relays it unparsed as
 *   SBX_PROP1 <session> <seq> <lowercase hex>\n
 * on the management port. Session 0 is the supervisor's own. The engine
 * decodes, attributes and reduces records at commit time. */
#ifndef SEEDBOX_PROPERTY_RELAY_H
#define SEEDBOX_PROPERTY_RELAY_H
#include <stddef.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>

enum {
    PROPERTY_RECORD_BYTES = 4096,
    PROPERTY_SESSIONS = 32,
    /* Prefix, two decimal fields, separators, hex payload and newline. */
    PROPERTY_FRAME_BYTES = 64 + 2 * PROPERTY_RECORD_BYTES,
};
/* A message that is exactly this asks for a reply once every earlier message
 * from the same connection has been relayed. */
static const char property_flush[] = "{\"flush\":{}}";

/* Returns the frame length, or zero if the record is empty or too large. */
static size_t property_frame(char frame[PROPERTY_FRAME_BYTES], uint32_t session, uint64_t seq,
                             const unsigned char *record, size_t size) {
    static const char hex[] = "0123456789abcdef";
    if (!size || size > PROPERTY_RECORD_BYTES) return 0;
    int used = snprintf(frame, PROPERTY_FRAME_BYTES, "SBX_PROP1 %u %llu ", session,
                        (unsigned long long)seq);
    if (used < 0 || (size_t)used + 2 * size + 1 > PROPERTY_FRAME_BYTES) return 0;
    for (size_t i = 0; i < size; ++i) {
        frame[used++] = hex[record[i] >> 4];
        frame[used++] = hex[record[i] & 15];
    }
    frame[used++] = '\n';
    return (size_t)used;
}
#endif
