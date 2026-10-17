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
    PROPERTY_SESSIONS = 128,
    /* Prefix, two decimal fields, separators, hex payload and newline. */
    PROPERTY_FRAME_BYTES = 64 + 2 * PROPERTY_RECORD_BYTES,
};

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
/* Kernel log lines that are property evidence: OOM kills, and crashes from
 * faults the process did not handle. Injected stops and machine crashes log
 * none of these. */
static int kernel_evidence(const char *message) {
    static const char *const markers[] = {
        "Out of memory: Killed process", "Memory cgroup out of memory: Killed process",
        "segfault at", "unhandled", "traps:", "potentially unexpected fatal signal",
    };
    for (size_t i = 0; i < sizeof(markers) / sizeof(*markers); ++i) {
        if (strstr(message, markers[i])) return 1;
    }
    return 0;
}

/* Turn one /dev/kmsg record ("prio,seq,usec,flags;message\n...") into a
 * supervisor record, {"kernel":{"message":"..."}}, when it is evidence.
 * Returns the record length, or zero when the line is not evidence. */
static size_t kernel_record(char record[PROPERTY_RECORD_BYTES], const char *kmsg, size_t size) {
    const char *message = memchr(kmsg, ';', size);
    if (!message) return 0;
    message++;
    size_t length = 0;
    while (message + length < kmsg + size && message[length] != '\n') length++;
    char line[512];
    if (length >= sizeof(line)) length = sizeof(line) - 1;
    memcpy(line, message, length);
    line[length] = 0;
    if (!kernel_evidence(line)) return 0;
    static const char prefix[] = "{\"kernel\":{\"message\":\"", suffix[] = "\"}}";
    size_t used = sizeof(prefix) - 1;
    memcpy(record, prefix, used);
    for (size_t i = 0; i < length; ++i) {
        unsigned char c = (unsigned char)line[i];
        /* JSON escaping; leave room for the suffix and an escape. */
        if (used + 8 + sizeof(suffix) > PROPERTY_RECORD_BYTES) break;
        if (c == '"' || c == '\\') { record[used++] = '\\'; record[used++] = (char)c; }
        else if (c < 0x20 || c >= 0x7f) used += (size_t)snprintf(record + used, 8, "\\u%04x", c);
        else record[used++] = (char)c;
    }
    memcpy(record + used, suffix, sizeof(suffix) - 1);
    return used + sizeof(suffix) - 1;
}
#endif
