/* Linux network probe shared by the PV and TCG machine tests. One guest runs
 * `server`, another `client`; their kernels resolve each other with ARP over
 * the simulated network, then exchange UDP datagrams and a TCP stream.
 *
 *   server PORT
 *       Echo UDP datagrams and TCP streams on PORT (0 picks one for both).
 *       Prints "NETPROBE ready port=P" once both sockets are bound.
 *   server-disk PORT
 *       The same server, recording its progress on /dev/vda with aligned
 *       512-byte direct I/O, each record written and fsynced before the step
 *       it records is visible on the network:
 *         sector 0  "SBXNETD1", u64 boot count; incremented before Ready
 *                   (a count already at its maximum fails with EOVERFLOW)
 *         sector 1  "SBXNETU1", u32 length, the last UDP datagram; written
 *                   before its echo. A datagram over 500 bytes fails with
 *                   EOVERFLOW rather than being stored in part.
 *         sector 2  "SBXNETT1", u64 byte count, u64 FNV-1a 64 checksum of the
 *                   last cleanly ended TCP stream; written before its close
 *       All values are little-endian and the rest of each sector is zero. A
 *       zero sector is an empty record; anything else malformed fails, never
 *       reformatted. Markers, before and as each record is written:
 *         NETPROBE disk boot=N token=none|LENGTH:CHECKSUM tcp=none|BYTES:CHECKSUM
 *         NETPROBE disk token=LENGTH:CHECKSUM
 *         NETPROBE disk tcp=BYTES:CHECKSUM
 *       CHECKSUM is the record's FNV-1a 64 checksum (of the token bytes, or of
 *       the stream) as 16 lowercase hex digits. Then "NETPROBE ready port=P".
 *   client ADDRESS PORT BYTES
 *       Send UDP probes until one echo returns, then echo BYTES of a fixed
 *       pattern over TCP. Prints one line per step:
 *         NETPROBE udp ok attempts=N
 *         NETPROBE arp ok address=ADDRESS mac=xx:xx:xx:xx:xx:xx   (Linux ARP)
 *         NETPROBE tcp ok bytes=BYTES connects=N
 *         NETPROBE done
 *   idle
 *       Print "NETPROBE idle" and pause forever: a live peer with no network
 *       activity.
 *   drop-client ADDRESS PORT
 *       Run as the controlling oracle, for a host-injected connection drop.
 *       Echo a short stream to a server, read the server-to-client direction
 *       of that TCP incarnation through the guest control API and print it,
 *       then keep echoing 64-byte rounds every 10 ms with a one-second
 *       TCP_USER_TIMEOUT. Once the host drops that direction, a round goes
 *       unacknowledged and the kernel aborts the connection. Then reconnect
 *       and echo; the server's single echo slot frees only after the host
 *       heals the drop. Finally read the direction again for a newer
 *       incarnation. Prints:
 *         NETPROBE drop first ok
 *         NETPROBE drop connection=JSON          (the control reply's object)
 *         NETPROBE drop aborted errno=E rounds=N
 *         NETPROBE drop reconnected connects=N
 *         NETPROBE drop reincarnation old=I new=J
 *         NETPROBE drop done
 *
 * Retries run on the guest's own clock, which the simulation owns, so a
 * server that is still booting is reached deterministically. Failures print
 * "NETPROBE fail STEP errno=E" and exit 70. Usage errors exit 64. */
#define _GNU_SOURCE
#include <arpa/inet.h>
#include <errno.h>
#include <netinet/in.h>
#include <poll.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <fcntl.h>
#include <stdint.h>
#include <netinet/tcp.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <unistd.h>

#define ATTEMPTS 200
#define RETRY_MS 50
#define DATAGRAM "seedbox network probe"

static int fail(const char *step) {
    printf("NETPROBE fail %s errno=%d\n", step, errno);
    fflush(stdout);
    return 70;
}

static int number(const char *text, unsigned long max, unsigned long *out) {
    char *end;
    errno = 0;
    unsigned long value = strtoul(text, &end, 10);
    if (!*text || *end || errno || value > max) return -1;
    *out = value;
    return 0;
}

static unsigned char pattern(size_t at) { return (unsigned char)(at * 131 + (at >> 8) * 7 + 1); }

static int write_all(int fd, const unsigned char *bytes, size_t length) {
    while (length) {
        ssize_t count = write(fd, bytes, length);
        if (count < 0 && errno == EINTR) continue;
        if (count <= 0) return -1;
        bytes += count;
        length -= (size_t)count;
    }
    return 0;
}

#define SECTOR 512
#define TOKEN_LIMIT (SECTOR - 12)
static const char BOOT_MAGIC[] = "SBXNETD1", TOKEN_MAGIC[] = "SBXNETU1", TCP_MAGIC[] = "SBXNETT1";

/* The server's progress records on its single disk. */
struct disk {
    int fd;
    unsigned char *sector; /* one aligned sector for direct I/O */
};

#define FNV_OFFSET 0xcbf29ce484222325u
#define FNV_PRIME 0x100000001b3u
static uint64_t fnv(const unsigned char *bytes, size_t length) {
    uint64_t hash = FNV_OFFSET;
    while (length--) hash = (hash ^ *bytes++) * FNV_PRIME;
    return hash;
}
static uint64_t get64(const unsigned char *at) {
    uint64_t value = 0;
    for (int i = 7; i >= 0; --i) value = value << 8 | at[i];
    return value;
}
static void put64(unsigned char *at, uint64_t value) {
    for (int i = 0; i < 8; ++i) at[i] = (unsigned char)(value >> (8 * i));
}
static int zero(const unsigned char *at, size_t length) {
    while (length--)
        if (*at++) return 0;
    return 1;
}
static int disk_read(struct disk *disk, unsigned index) {
    ssize_t count = pread(disk->fd, disk->sector, SECTOR, (off_t)index * SECTOR);
    if (count != SECTOR) {
        if (count >= 0) errno = EIO;
        return -1;
    }
    return 0;
}
/* Write the sector buffer to `index` and make it durable. */
static int disk_write(struct disk *disk, unsigned index) {
    ssize_t count = pwrite(disk->fd, disk->sector, SECTOR, (off_t)index * SECTOR);
    if (count != SECTOR) {
        if (count >= 0) errno = EIO;
        return -1;
    }
    return fsync(disk->fd);
}
static void record(struct disk *disk, const char magic[8]) {
    memset(disk->sector, 0, SECTOR);
    memcpy(disk->sector, magic, 8);
}
/* 1 for a valid record, 0 for an empty sector, -1 (EBADMSG) otherwise.
 * `body` bytes follow the magic; the rest must be zero. */
static int disk_record(struct disk *disk, const char magic[8], size_t body) {
    if (zero(disk->sector, SECTOR)) return 0;
    if (memcmp(disk->sector, magic, 8) || !zero(disk->sector + 8 + body, SECTOR - 8 - body)) {
        errno = EBADMSG;
        return -1;
    }
    return 1;
}

/* Validate all records, then count this boot durably and report. */
static int disk_open(struct disk *disk) {
    disk->fd = open("/dev/vda", O_RDWR | O_DIRECT | O_CLOEXEC);
    if (disk->fd < 0) return fail("disk open");
    void *buffer;
    if ((errno = posix_memalign(&buffer, 4096, SECTOR))) return fail("disk buffer");
    disk->sector = buffer;
    if (disk_read(disk, 0)) return fail("disk read boot");
    int found = disk_record(disk, BOOT_MAGIC, 8);
    if (found < 0) return fail("disk boot record");
    uint64_t boots = found ? get64(disk->sector + 8) : 0;
    if (boots == UINT64_MAX) {
        errno = EOVERFLOW;
        return fail("disk boot count");
    }
    if (disk_read(disk, 1)) return fail("disk read token");
    unsigned char *token = disk->sector + 8;
    uint32_t token_length = (uint32_t)token[0] | (uint32_t)token[1] << 8 | (uint32_t)token[2] << 16 |
                            (uint32_t)token[3] << 24;
    /* A zero sector has length zero; any other oversized length is malformed. */
    found = token_length > TOKEN_LIMIT ? (errno = EBADMSG, -1)
                                       : disk_record(disk, TOKEN_MAGIC, 4 + token_length);
    if (found < 0) return fail("disk token record");
    char token_text[40] = "none";
    if (found)
        snprintf(token_text, sizeof(token_text), "%u:%016llx", token_length,
                 (unsigned long long)fnv(disk->sector + 12, token_length));
    if (disk_read(disk, 2)) return fail("disk read tcp");
    found = disk_record(disk, TCP_MAGIC, 16);
    if (found < 0) return fail("disk tcp record");
    char tcp_text[48] = "none";
    if (found)
        snprintf(tcp_text, sizeof(tcp_text), "%llu:%016llx", (unsigned long long)get64(disk->sector + 8),
                 (unsigned long long)get64(disk->sector + 16));
    record(disk, BOOT_MAGIC);
    put64(disk->sector + 8, boots + 1);
    if (disk_write(disk, 0)) return fail("disk write boot");
    printf("NETPROBE disk boot=%llu token=%s tcp=%s\n", (unsigned long long)(boots + 1), token_text,
           tcp_text);
    return 0;
}

/* Echo one stream until the peer ends it. With a disk, a cleanly ended
 * stream's length and checksum are durable before the connection closes. */
static int echo_stream(int connection, struct disk *disk) {
    unsigned char buffer[4096];
    uint64_t bytes = 0, checksum = FNV_OFFSET;
    ssize_t count;
    for (;;) {
        count = read(connection, buffer, sizeof(buffer));
        if (count < 0 && errno == EINTR) continue;
        if (count <= 0 || write_all(connection, buffer, (size_t)count)) break;
        bytes += (uint64_t)count;
        for (ssize_t i = 0; i < count; ++i) checksum = (checksum ^ buffer[i]) * FNV_PRIME;
    }
    if (disk && count == 0) {
        record(disk, TCP_MAGIC);
        put64(disk->sector + 8, bytes);
        put64(disk->sector + 16, checksum);
        if (disk_write(disk, 2)) {
            close(connection);
            return fail("disk write tcp");
        }
        printf("NETPROBE disk tcp=%llu:%016llx\n", (unsigned long long)bytes, (unsigned long long)checksum);
        fflush(stdout);
    }
    close(connection);
    return 0;
}

static int server(unsigned long port, struct disk *disk) {
    struct sockaddr_in address = {.sin_family = AF_INET, .sin_port = htons((uint16_t)port)};
    int udp = socket(AF_INET, SOCK_DGRAM | SOCK_CLOEXEC, 0);
    int tcp = socket(AF_INET, SOCK_STREAM | SOCK_CLOEXEC, 0);
    int on = 1;
    if (udp < 0 || tcp < 0) return fail("socket");
    if (setsockopt(tcp, SOL_SOCKET, SO_REUSEADDR, &on, sizeof(on))) return fail("reuse");
    if (bind(tcp, (struct sockaddr *)&address, sizeof(address))) return fail("bind tcp");
    socklen_t length = sizeof(address);
    if (getsockname(tcp, (struct sockaddr *)&address, &length)) return fail("tcp name");
    if (bind(udp, (struct sockaddr *)&address, sizeof(address))) return fail("bind udp");
    if (listen(tcp, 8)) return fail("listen");
    printf("NETPROBE ready port=%u\n", ntohs(address.sin_port));
    fflush(stdout);
    for (;;) {
        struct pollfd fds[2] = {{.fd = udp, .events = POLLIN}, {.fd = tcp, .events = POLLIN}};
        if (poll(fds, 2, -1) < 0) {
            if (errno == EINTR) continue;
            return fail("poll");
        }
        if (fds[0].revents & POLLIN) {
            unsigned char datagram[2048];
            struct sockaddr_in peer;
            socklen_t peer_length = sizeof(peer);
            /* With a disk, MSG_TRUNC reports a datagram's whole length. */
            ssize_t count = recvfrom(udp, datagram, sizeof(datagram), disk ? MSG_TRUNC : 0,
                                     (struct sockaddr *)&peer, &peer_length);
            if (count >= 0 && disk) {
                if ((size_t)count > TOKEN_LIMIT) {
                    errno = EOVERFLOW;
                    return fail("disk token length");
                }
                size_t length = (size_t)count;
                record(disk, TOKEN_MAGIC);
                for (int i = 0; i < 4; ++i) disk->sector[8 + i] = (unsigned char)(length >> (8 * i));
                memcpy(disk->sector + 12, datagram, length);
                if (disk_write(disk, 1)) return fail("disk write token");
                printf("NETPROBE disk token=%zu:%016llx\n", length, (unsigned long long)fnv(datagram, length));
                fflush(stdout);
            }
            if (count >= 0 && sendto(udp, datagram, (size_t)count, 0, (struct sockaddr *)&peer, peer_length) < 0)
                return fail("udp echo");
        }
        /* One stream at a time keeps the server single-threaded. */
        if (fds[1].revents & POLLIN) {
            int connection = accept4(tcp, NULL, NULL, SOCK_CLOEXEC);
            if (connection >= 0 && echo_stream(connection, disk)) return 70;
        }
    }
}

static int udp_probe(const struct sockaddr_in *server) {
    int udp = socket(AF_INET, SOCK_DGRAM | SOCK_CLOEXEC, 0);
    if (udp < 0) return fail("udp socket");
    for (int attempt = 1; attempt <= ATTEMPTS; ++attempt) {
        if (sendto(udp, DATAGRAM, sizeof(DATAGRAM), 0, (const struct sockaddr *)server, sizeof(*server)) < 0 &&
            errno != EHOSTUNREACH && errno != ECONNREFUSED)
            return fail("udp send");
        struct pollfd fd = {.fd = udp, .events = POLLIN};
        int ready = poll(&fd, 1, RETRY_MS);
        if (ready < 0 && errno != EINTR) return fail("udp poll");
        if (ready <= 0) continue;
        char reply[sizeof(DATAGRAM) + 1];
        ssize_t count = recv(udp, reply, sizeof(reply), 0);
        if (count < 0 && (errno == ECONNREFUSED || errno == EINTR)) continue;
        if (count != (ssize_t)sizeof(DATAGRAM) || memcmp(reply, DATAGRAM, sizeof(DATAGRAM))) {
            errno = EBADMSG;
            return fail("udp reply");
        }
        close(udp);
        printf("NETPROBE udp ok attempts=%d\n", attempt);
        return 0;
    }
    errno = ETIMEDOUT;
    return fail("udp");
}

/* Report the kernel's completed neighbour entry; absent on non-Linux hosts. */
static int arp_entry(const char *address) {
    FILE *table = fopen("/proc/net/arp", "re");
    if (!table) return 0;
    char line[256], ip[64], hardware[64];
    unsigned type, flags;
    int found = 0;
    while (fgets(line, sizeof(line), table)) {
        if (sscanf(line, "%63s 0x%x 0x%x %63s", ip, &type, &flags, hardware) == 4 &&
            !strcmp(ip, address) && (flags & 0x2)) {
            printf("NETPROBE arp ok address=%s mac=%s\n", ip, hardware);
            found = 1;
        }
    }
    fclose(table);
    return found;
}

static int tcp_probe(const struct sockaddr_in *server, size_t bytes) {
    int stream = -1, attempt;
    for (attempt = 1; attempt <= ATTEMPTS; ++attempt) {
        stream = socket(AF_INET, SOCK_STREAM | SOCK_CLOEXEC, 0);
        if (stream < 0) return fail("tcp socket");
        if (!connect(stream, (const struct sockaddr *)server, sizeof(*server))) break;
        if (errno != ECONNREFUSED && errno != ETIMEDOUT && errno != EHOSTUNREACH) return fail("tcp connect");
        close(stream);
        stream = -1;
        usleep(RETRY_MS * 1000);
    }
    if (stream < 0) {
        errno = ETIMEDOUT;
        return fail("tcp connect");
    }
    /* Send and receive interleaved so neither side's window can deadlock. */
    size_t sent = 0, received = 0;
    unsigned char buffer[4096];
    while (received < bytes) {
        struct pollfd fd = {.fd = stream, .events = POLLIN | (sent < bytes ? POLLOUT : 0)};
        if (poll(&fd, 1, -1) < 0) {
            if (errno == EINTR) continue;
            return fail("tcp poll");
        }
        if ((fd.revents & POLLOUT) && sent < bytes) {
            size_t length = bytes - sent < sizeof(buffer) ? bytes - sent : sizeof(buffer);
            for (size_t i = 0; i < length; ++i) buffer[i] = pattern(sent + i);
            ssize_t count = send(stream, buffer, length, MSG_DONTWAIT | MSG_NOSIGNAL);
            if (count < 0 && errno != EAGAIN && errno != EINTR) return fail("tcp send");
            if (count > 0) sent += (size_t)count;
            if (sent == bytes && shutdown(stream, SHUT_WR)) return fail("tcp shutdown");
        }
        if (fd.revents & (POLLIN | POLLHUP | POLLERR)) {
            ssize_t count = recv(stream, buffer, sizeof(buffer), MSG_DONTWAIT);
            if (count < 0 && (errno == EAGAIN || errno == EINTR)) continue;
            if (count < 0) return fail("tcp receive");
            if (!count) {
                errno = EPIPE;
                return fail("tcp short echo");
            }
            for (ssize_t i = 0; i < count; ++i)
                if (buffer[i] != pattern(received + (size_t)i)) {
                    errno = EBADMSG;
                    return fail("tcp echo");
                }
            received += (size_t)count;
        }
    }
    char extra;
    if (recv(stream, &extra, 1, 0) != 0) {
        errno = EBADMSG;
        return fail("tcp trailing");
    }
    close(stream);
    printf("NETPROBE tcp ok bytes=%zu connects=%d\n", bytes, attempt);
    return 0;
}

static int client(const char *address, unsigned long port, unsigned long bytes) {
    struct sockaddr_in server = {.sin_family = AF_INET, .sin_port = htons((uint16_t)port)};
    if (inet_pton(AF_INET, address, &server.sin_addr) != 1) return 64;
    int result = udp_probe(&server);
    if (result) return result;
    arp_entry(address);
    fflush(stdout);
    if ((result = tcp_probe(&server, bytes))) return result;
    printf("NETPROBE done\n");
    fflush(stdout);
    return 0;
}


/* One guest control call: a request line in, one reply line out. */
static char control_reply[8192];
static int control_call(int control, unsigned id, const char *operation) {
    char request[1024];
    int size = snprintf(request, sizeof(request), "{\"version\":1,\"id\":%u,%s}\n", id, operation);
    if (size < 0 || (size_t)size >= sizeof(request)) {
        errno = EOVERFLOW;
        return fail("control request");
    }
    if (write_all(control, (const unsigned char *)request, (size_t)size)) return fail("control write");
    size_t used = 0;
    for (;;) {
        if (used + 1 == sizeof(control_reply)) {
            errno = EOVERFLOW;
            return fail("control reply size");
        }
        ssize_t count = read(control, control_reply + used, 1);
        if (count < 0 && errno == EINTR) continue;
        if (count != 1) return fail("control read");
        if (control_reply[used++] == '\n') break;
    }
    control_reply[used] = 0;
    if (!strstr(control_reply, "\"ok\":true")) {
        printf("NETPROBE drop control-error %s", control_reply);
        errno = EPROTO;
        return fail("control result");
    }
    return 0;
}
/* The first object of the reply's "connections" array, copied verbatim. */
static int first_connection(char *out, size_t size, unsigned long long *incarnation) {
    const char *list = strstr(control_reply, "\"connections\":[");
    const char *open = list ? strchr(list, '{') : NULL;
    const char *close = open ? strchr(open, '}') : NULL;
    const char *field = open ? strstr(open, "\"incarnation\":") : NULL;
    if (!close || !field || field > close || (size_t)(close - open + 1) >= size) {
        errno = ENOENT;
        return -1;
    }
    memcpy(out, open, (size_t)(close - open + 1));
    out[close - open + 1] = 0;
    *incarnation = strtoull(field + strlen("\"incarnation\":"), NULL, 10);
    return 0;
}
static int connect_retry(const struct sockaddr_in *server, int *attempts) {
    for (*attempts = 1; *attempts <= ATTEMPTS; ++*attempts) {
        int stream = socket(AF_INET, SOCK_STREAM | SOCK_CLOEXEC, 0);
        if (stream < 0) return -1;
        if (!connect(stream, (const struct sockaddr *)server, sizeof(*server))) return stream;
        int error = errno;
        close(stream);
        if (error != ECONNREFUSED && error != ETIMEDOUT && error != EHOSTUNREACH) {
            errno = error;
            return -1;
        }
        usleep(RETRY_MS * 1000);
    }
    errno = ETIMEDOUT;
    return -1;
}
/* Send `length` pattern bytes and read the same echo back. */
static int echo(int stream, size_t length) {
    unsigned char buffer[256];
    for (size_t i = 0; i < length; ++i) buffer[i] = pattern(i);
    if (write_all(stream, buffer, length)) return -1;
    for (size_t received = 0; received < length;) {
        ssize_t count = read(stream, buffer, length - received);
        if (count < 0 && errno == EINTR) continue;
        if (count <= 0) {
            if (!count) errno = EPIPE;
            return -1;
        }
        for (ssize_t i = 0; i < count; ++i)
            if (buffer[i] != pattern(received + (size_t)i)) {
                errno = EBADMSG;
                return -1;
            }
        received += (size_t)count;
    }
    return 0;
}
static int drop_client(const char *address, unsigned long port) {
    struct sockaddr_in server = {.sin_family = AF_INET, .sin_port = htons((uint16_t)port)};
    if (inet_pton(AF_INET, address, &server.sin_addr) != 1) return 64;
    struct sockaddr_un path = {.sun_family = AF_UNIX};
    const char control_path[] = "/run/seedbox/control.sock";
    memcpy(path.sun_path, control_path, sizeof(control_path));
    int control = socket(AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC, 0);
    if (control < 0 || connect(control, (struct sockaddr *)&path, sizeof(path))) return fail("control connect");
    int attempts;
    int stream = connect_retry(&server, &attempts);
    if (stream < 0) return fail("drop connect");
    if (echo(stream, 64)) return fail("drop first echo");
    printf("NETPROBE drop first ok\n");
    fflush(stdout);
    if (control_call(control, 1, "\"op\":\"connections\",\"from\":{\"machine\":\"server\"},\"to\":{\"machine\":\"oracle\"}"))
        return 70;
    char connection[512];
    unsigned long long old;
    if (first_connection(connection, sizeof(connection), &old)) return fail("drop observe connection");
    printf("NETPROBE drop connection=%s\n", connection);
    fflush(stdout);
    unsigned timeout_ms = 1000;
    if (setsockopt(stream, IPPROTO_TCP, TCP_USER_TIMEOUT, &timeout_ms, sizeof(timeout_ms)))
        return fail("drop user timeout");
    /* Traffic keeps flowing until the host's drop makes a round unanswered. */
    unsigned long rounds = 0;
    while (!echo(stream, 64)) {
        ++rounds;
        usleep(10 * 1000);
    }
    if (errno != ETIMEDOUT) return fail("drop abort");
    printf("NETPROBE drop aborted errno=%d rounds=%lu\n", errno, rounds);
    fflush(stdout);
    close(stream);
    /* After the heal, the server's retransmission to the aborted peer draws
     * a reset, which frees its single echo slot; the new stream then echoes. */
    stream = connect_retry(&server, &attempts);
    if (stream < 0) return fail("drop reconnect");
    if (echo(stream, 64)) return fail("drop second echo");
    printf("NETPROBE drop reconnected connects=%d\n", attempts);
    fflush(stdout);
    char operation[256];
    snprintf(operation, sizeof(operation),
             "\"op\":\"connections\",\"from\":{\"machine\":\"server\"},\"to\":{\"machine\":\"oracle\"},\"after\":%llu", old);
    if (control_call(control, 2, operation)) return 70;
    unsigned long long renewed;
    if (first_connection(connection, sizeof(connection), &renewed) || renewed <= old) return fail("drop reincarnation");
    printf("NETPROBE drop reincarnation old=%llu new=%llu\n", old, renewed);
    printf("NETPROBE drop done\n");
    fflush(stdout);
    close(stream);
    close(control);
    return 0;
}
int main(int argc, char **argv) {
    setvbuf(stdout, NULL, _IOLBF, 0);
    unsigned long port, bytes;
    if (argc == 3 && !strcmp(argv[1], "server") && !number(argv[2], 65535, &port)) return server(port, NULL);
    if (argc == 3 && !strcmp(argv[1], "server-disk") && !number(argv[2], 65535, &port)) {
        struct disk disk;
        int result = disk_open(&disk);
        return result ? result : server(port, &disk);
    }
    if (argc == 4 && !strcmp(argv[1], "drop-client") && !number(argv[3], 65535, &port) && port)
        return drop_client(argv[2], port);
    if (argc == 2 && !strcmp(argv[1], "idle")) {
        printf("NETPROBE idle\n");
        fflush(stdout);
        for (;;) pause();
    }
    if (argc == 5 && !strcmp(argv[1], "client") && !number(argv[3], 65535, &port) && port &&
        !number(argv[4], 1ul << 30, &bytes))
        return client(argv[2], port, bytes);
    fprintf(stderr, "usage: network-probe server PORT | server-disk PORT | client ADDRESS PORT BYTES | drop-client ADDRESS PORT | idle\n");
    return 64;
}
