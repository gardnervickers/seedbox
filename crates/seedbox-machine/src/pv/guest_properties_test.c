/* Host tests of the property relay. Frames are printed for the host's Rust
 * decoders to check; `relay DIR` also exercises real SOCK_SEQPACKET sessions
 * (Linux only). Build together with guest_properties.c. */
#define _POSIX_C_SOURCE 200809L
#include "guest_properties.h"
#include <assert.h>
#include <errno.h>
#include <stdio.h>
#include <string.h>

static void frames(void) {
    char frame[SBX_PROPERTY_FRAME_BYTES];
    size_t size = sbx_property_frame(frame, 3, 17, (const unsigned char *)"{}", 2);
    assert(size == strlen("SBX_PROP1 3 17 7b7d\n") && !memcmp(frame, "SBX_PROP1 3 17 7b7d\n", size));
    assert(fwrite(frame, 1, size, stdout) == size);
    static unsigned char largest[SBX_PROPERTY_RECORD_BYTES];
    memset(largest, 0xff, sizeof(largest));
    size = sbx_property_frame(frame, UINT32_MAX, UINT64_MAX, largest, sizeof(largest));
    assert(size > 0 && frame[size - 1] == '\n');
    assert(fwrite(frame, 1, size, stdout) == size);
    assert(!sbx_property_frame(frame, 1, 1, largest, 0));
    assert(!sbx_property_frame(frame, 1, 1, largest, sizeof(largest) + 1));

    char crash[SBX_CRASH_FRAME_BYTES];
    const char *reports[] = {
        "6,812,4120304,-;libdynamorio.so: keel-watchdog: potentially unexpected fatal signal 6.\n",
        "6,813,4120305,-;keel-s3dd: potentially unexpected fatal signal 11.",
        /* kmsg escapes the task name's unprintable bytes and backslashes. */
        "6,814,4120306,c;a: b\\x5cc: potentially unexpected fatal signal 31.\n SUBSYSTEM=x\n",
    };
    for (unsigned i = 0; i < sizeof(reports) / sizeof(*reports); ++i) {
        size = sbx_crash_frame(reports[i], strlen(reports[i]), crash);
        assert(size > 0 && size != (size_t)-1 && crash[size - 1] == '\n');
        assert(fwrite(crash, 1, size, stdout) == size);
    }
    const char *ignored[] = {
        "",
        "6,1,1,-",
        /* Userspace may write the same text to /dev/kmsg, at a nonzero facility. */
        "14,1,1,-;x: potentially unexpected fatal signal 6.",
        "1,2,1,-;keel[77]: segfault at 0 ip 0000 sp 0000 error 4 in keel[1000+2000]",
        "6,2,1,-;traps: keel[77] trap invalid opcode ip:1 sp:2 error:0",
        "6,2,1,-;: potentially unexpected fatal signal 6.",
        "6,2,1,-;x: potentially unexpected fatal signal 6",
        "6,2,1,-;x: potentially unexpected fatal signal 06.",
        "6,2,1,-;x: potentially unexpected fatal signal 65.",
        "6,2,1,-;x: potentially unexpected fatal signal .",
        "6,2,1,-;x\ty: potentially unexpected fatal signal 6.",
    };
    for (unsigned i = 0; i < sizeof(ignored) / sizeof(*ignored); ++i)
        assert(!sbx_crash_frame(ignored[i], strlen(ignored[i]), crash));
    uint64_t lost;
    char reason[SBX_PROPERTY_REASON_BYTES];
    const char *report = "14,9,1,-;seedbox-guest: lost 3 property records: No such file or directory (os error 2)\n";
    assert(sbx_guest_lost_line(report, strlen(report), &lost, reason));
    assert(lost == 3 && !strcmp(reason, " property records: No such file or directory (os error 2)"));
    /* The kernel never writes it; other program lines are not losses. */
    assert(!sbx_guest_lost_line("6,9,1,-;seedbox-guest: lost 3 x", 31, &lost, reason));
    assert(!sbx_guest_lost_line("14,9,1,-;seedbox-guest: lost x", 30, &lost, reason));
    assert(!sbx_guest_lost_line("14,9,1,-;hello", 14, &lost, reason));
    char long_report[2048];
    int length = snprintf(long_report, sizeof(long_report), "6,3,1,-;%01100d: potentially unexpected fatal signal 6.", 7);
    assert(length > 0 && (size_t)length < sizeof(long_report));
    assert(sbx_crash_frame(long_report, (size_t)length, crash) == (size_t)-1);
}

#ifdef __linux__
#include <stdlib.h>
#include <poll.h>
#include <stddef.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <unistd.h>

static char sink[1 << 20];
static size_t sunk;
static int capture(void *unused, const void *bytes, size_t length) {
    (void)unused;
    if (sunk + length > sizeof(sink)) { errno = ENOSPC; return -1; }
    memcpy(sink + sunk, bytes, length);
    sunk += length;
    return 0;
}

static int client(const char *path) {
    int fd = socket(AF_UNIX, SOCK_SEQPACKET, 0);
    assert(fd >= 0);
    struct sockaddr_un address = {.sun_family = AF_UNIX};
    strcpy(address.sun_path, path);
    assert(!connect(fd, (struct sockaddr *)&address, sizeof(address)));
    return fd;
}

static size_t lines(void) {
    size_t n = 0;
    for (size_t i = 0; i < sunk; ++i) n += sink[i] == '\n';
    return n;
}

static void relay(const char *directory) {
    char path[256];
    snprintf(path, sizeof(path), "%s/guest.sock", directory);
    struct sbx_properties p;
    sbx_properties_init(&p, capture, NULL);
    assert(!sbx_properties_open(&p, path));
    int first = client(path), second = client(path);
    assert(send(first, "{\"a\":1}", 7, 0) == 7);
    assert(send(second, "{\"b\":2}", 7, 0) == 7);
    assert(send(first, "{\"flush\":{}}", 12, 0) == 12);
    static char oversized[SBX_PROPERTY_RECORD_BYTES + 1];
    memset(oversized, 'x', sizeof(oversized));
    assert(send(second, oversized, sizeof(oversized), 0) == (ssize_t)sizeof(oversized));
    /* The oversized record still takes its sequence number. */
    assert(send(second, "{\"d\":4}", 7, 0) == 7);
    assert(!sbx_properties_drain(&p));
    /* The engine's reply reaches only its own session, as one message. */
    assert(!sbx_properties_reply(&p, "SBX_GUEST1 1 {\"flush\":{\"seq\":2}}"));
    char reply[64];
    ssize_t got = recv(first, reply, sizeof(reply), 0);
    assert(got == 19 && !memcmp(reply, "{\"flush\":{\"seq\":2}}", 19));
    /* Replies to gone sessions are discarded; malformed ones are refused. */
    assert(!sbx_properties_reply(&p, "SBX_GUEST1 9 {}"));
    assert(sbx_properties_reply(&p, "SBX_GUEST1 01 {}") && errno == EINVAL);
    assert(sbx_properties_reply(&p, "SBX_GUEST1 1") && errno == EINVAL);
    assert(sbx_properties_reply(&p, "SBX_GUEST1 4294967296 {}") && errno == EINVAL);
    /* The abstract listener serves the same sessions (session 3). */
    int hidden = socket(AF_UNIX, SOCK_SEQPACKET, 0);
    struct sockaddr_un address = {.sun_family = AF_UNIX};
    memcpy(address.sun_path + 1, SBX_PROPERTY_ABSTRACT, strlen(SBX_PROPERTY_ABSTRACT));
    assert(!connect(hidden, (struct sockaddr *)&address,
                    (socklen_t)(offsetof(struct sockaddr_un, sun_path) + 1 + strlen(SBX_PROPERTY_ABSTRACT))));
    assert(send(hidden, "{\"e\":5}", 7, 0) == 7);
    /* A busy session is relayed a budget at a time. */
    for (unsigned i = 0; i < SBX_PROPERTY_BUDGET + 8; ++i) assert(send(hidden, "{}", 2, 0) == 2);
    assert(!sbx_properties_drain(&p));
    size_t before = lines();
    assert(send(second, "{}", 2, 0) == 2);
    for (unsigned i = 0; i < SBX_PROPERTY_BUDGET + 8; ++i) assert(send(second, "{}", 2, 0) == 2);
    short revents[SBX_PROPERTY_POLLED] = {0};
    revents[2 + 1] = POLLIN;
    assert(!sbx_properties_poll(&p, revents));
    assert(lines() - before == SBX_PROPERTY_BUDGET);
    assert(!sbx_properties_drain(&p));
    /* A closed peer frees its slot before new connections are counted. */
    close(first);
    int clients[SBX_PROPERTY_SESSIONS];
    for (unsigned i = 0; i < SBX_PROPERTY_SESSIONS - 2; ++i) clients[i] = client(path);
    assert(!sbx_properties_drain(&p));
    int extra = client(path);
    assert(!sbx_properties_drain(&p));
    for (unsigned i = 0; i < SBX_PROPERTY_SESSIONS - 2; ++i) close(clients[i]);
    close(extra);
    close(second);
    close(hidden);
    assert(!sbx_properties_drain(&p));
    sbx_properties_close(&p);
    assert(fwrite(sink, 1, sunk, stdout) == sunk);
}
#endif

int main(int argc, char **argv) {
    if (argc == 3 && !strcmp(argv[1], "relay")) {
#ifdef __linux__
        relay(argv[2]);
        return 0;
#else
        return 77;
#endif
    }
    frames();
    return 0;
}
