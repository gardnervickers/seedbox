/* The etcd acceptance oracle: an HTTP/1.0 client of etcd's v3 JSON gateway.
 *
 *   etcd-client smoke
 *
 * Nodes are 10.0.1.1..3 on port 2379. The client waits, a bounded number of
 * attempts apart in simulated time, until every node's status names the same
 * leader; then writes k0=v0 (base64 azA=/djA=) and reads it back from a
 * follower. Requests are HTTP/1.0 with Connection: close, so a response body
 * ends at EOF and is never chunked; a Content-Length, if present, must match.
 * Every response must be status 200 without a JSON "error" field and carry
 * the expected fields; anything else fails closed.
 * Markers go to stdout, every failed attempt's reason to stderr. */
#define _GNU_SOURCE
#include <arpa/inet.h>
#include <errno.h>
#include <netinet/in.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/time.h>
#include <time.h>
#include <unistd.h>

#define NODES 3
#define ATTEMPTS 60
#define PAUSE_NS 500000000L
#define TIMEOUT_S 5

static const char *address(int node) {
    static const char *addresses[NODES] = {"10.0.1.1", "10.0.1.2", "10.0.1.3"};
    return addresses[node];
}

static char error[512];

/* Write all LENGTH bytes, retrying interrupted and partial writes. */
static int write_all(int stream, const char *bytes, size_t length) {
    while (length) {
        ssize_t count = write(stream, bytes, length);
        if (count < 0 && errno == EINTR) continue;
        if (count <= 0) return -1;
        bytes += count;
        length -= (size_t)count;
    }
    return 0;
}

/* POST BODY to PATH on NODE; on success RESPONSE holds the JSON body. */
static int request(int node, const char *path, const char *body, char *response, size_t size) {
    error[0] = 0;
    response[0] = 0;
    int stream = socket(AF_INET, SOCK_STREAM | SOCK_CLOEXEC, 0);
    if (stream < 0) {
        snprintf(error, sizeof error, "socket errno=%d", errno);
        return -1;
    }
    struct timeval timeout = {.tv_sec = TIMEOUT_S};
    if (setsockopt(stream, SOL_SOCKET, SO_RCVTIMEO, &timeout, sizeof timeout) ||
        setsockopt(stream, SOL_SOCKET, SO_SNDTIMEO, &timeout, sizeof timeout)) {
        snprintf(error, sizeof error, "setsockopt errno=%d", errno);
        close(stream);
        return -1;
    }
    struct sockaddr_in server = {.sin_family = AF_INET, .sin_port = htons(2379)};
    inet_pton(AF_INET, address(node), &server.sin_addr);
    if (connect(stream, (struct sockaddr *)&server, sizeof server)) {
        snprintf(error, sizeof error, "connect %s errno=%d", address(node), errno);
        close(stream);
        return -1;
    }
    char head[512];
    int length = snprintf(head, sizeof head,
                          "POST %s HTTP/1.0\r\nHost: %s:2379\r\nContent-Type: application/json\r\n"
                          "Content-Length: %zu\r\nConnection: close\r\n\r\n",
                          path, address(node), strlen(body));
    if (length < 0 || (size_t)length >= sizeof head || write_all(stream, head, (size_t)length) ||
        write_all(stream, body, strlen(body))) {
        snprintf(error, sizeof error, "write %s errno=%d", address(node), errno);
        close(stream);
        return -1;
    }
    static char reply[65536];
    size_t used = 0;
    for (;;) {
        ssize_t count = read(stream, reply + used, sizeof reply - 1 - used);
        if (count < 0 && errno == EINTR) continue;
        if (count < 0) {
            snprintf(error, sizeof error, "read %s errno=%d", address(node), errno);
            close(stream);
            return -1;
        }
        if (count == 0) break;
        used += (size_t)count;
        if (used == sizeof reply - 1) {
            snprintf(error, sizeof error, "reply from %s too long", address(node));
            close(stream);
            return -1;
        }
    }
    close(stream);
    reply[used] = 0;
    char *end = strstr(reply, "\r\n\r\n");
    if ((strncmp(reply, "HTTP/1.0 200 ", 13) && strncmp(reply, "HTTP/1.1 200 ", 13)) || !end) {
        snprintf(error, sizeof error, "%s %s: %.200s", address(node), path, reply);
        return -1;
    }
    size_t actual = used - (size_t)(end + 4 - reply);
    char *field = strcasestr(reply, "\r\nContent-Length: ");
    if (field && field < end && strtoul(field + 18, NULL, 10) != actual) {
        snprintf(error, sizeof error, "%s %s: body of %zu bytes, declared %lu", address(node),
                 path, actual, strtoul(field + 18, NULL, 10));
        return -1;
    }
    if (actual >= size || strstr(end + 4, "\"error\"")) {
        snprintf(error, sizeof error, "%s %s: %.200s", address(node), path, end + 4);
        return -1;
    }
    memcpy(response, end + 4, actual + 1);
    return 0;
}

/* Copy the string value of the first "NAME":"..." field. */
static int field(const char *json, const char *name, char *value, size_t size) {
    char key[64];
    snprintf(key, sizeof key, "\"%s\":\"", name);
    const char *start = strstr(json, key);
    if (!start) return -1;
    start += strlen(key);
    const char *end = strchr(start, '"');
    if (!end || (size_t)(end - start) >= size || end == start) return -1;
    memcpy(value, start, (size_t)(end - start));
    value[end - start] = 0;
    return 0;
}

static void pause_between_attempts(void) {
    struct timespec pause = {.tv_nsec = PAUSE_NS};
    while (nanosleep(&pause, &pause) && errno == EINTR) {
    }
}

/* Wait until every node reports the same leader; returns its node index. */
static int ready(void) {
    for (int attempt = 1; attempt <= ATTEMPTS; attempt++) {
        char leaders[NODES][32], members[NODES][32];
        int agreed = 1;
        for (int node = 0; node < NODES && agreed; node++) {
            char body[65536] = "";
            if (request(node, "/v3/maintenance/status", "{}", body, sizeof body)) {
                agreed = 0;
            } else if (field(body, "leader", leaders[node], sizeof leaders[node]) ||
                       field(body, "member_id", members[node], sizeof members[node])) {
                snprintf(error, sizeof error, "%s status without leader: %.200s",
                         address(node), body);
                agreed = 0;
            } else if (!strcmp(leaders[node], "0") || !strcmp(members[node], "0")) {
                snprintf(error, sizeof error, "%s reports leader %s member %s", address(node),
                         leaders[node], members[node]);
                agreed = 0;
            } else if (strcmp(leaders[node], leaders[0])) {
                snprintf(error, sizeof error, "leaders differ: %s %s", leaders[0], leaders[node]);
                agreed = 0;
            }
            for (int other = 0; agreed && other < node; other++) {
                if (!strcmp(members[other], members[node])) {
                    snprintf(error, sizeof error, "duplicate member %s", members[node]);
                    agreed = 0;
                }
            }
        }
        if (agreed) {
            for (int node = 0; node < NODES; node++) {
                if (!strcmp(members[node], leaders[0])) {
                    printf("ETCDCLIENT ready attempts=%d\n", attempt);
                    printf("ETCDCLIENT leader node=%d id=%s\n", node + 1, leaders[0]);
                    fflush(stdout);
                    return node;
                }
            }
            snprintf(error, sizeof error, "leader %s is not a member", leaders[0]);
        }
        fprintf(stderr, "ETCDCLIENT waiting attempt=%d: %s\n", attempt, error);
        pause_between_attempts();
    }
    printf("ETCDCLIENT fail not ready after %d attempts: %s\n", ATTEMPTS, error);
    return -1;
}

static int smoke(void) {
    int leader = ready();
    if (leader < 0) return 1;
    char body[65536] = "", revision[32] = "";
    if (request(leader, "/v3/kv/put", "{\"key\":\"azA=\",\"value\":\"djA=\"}", body, sizeof body) ||
        field(body, "revision", revision, sizeof revision) || !strcmp(revision, "0")) {
        printf("ETCDCLIENT fail put: %s %.200s\n", error, body);
        return 1;
    }
    printf("ETCDCLIENT put ok key=azA= revision=%s\n", revision);
    int follower = (leader + 1) % NODES;
    if (request(follower, "/v3/kv/range", "{\"key\":\"azA=\"}", body, sizeof body) ||
        !strstr(body, "\"key\":\"azA=\"") || !strstr(body, "\"value\":\"djA=\"") ||
        !strstr(body, "\"count\":\"1\"")) {
        printf("ETCDCLIENT fail range node=%d: %s %.200s\n", follower + 1, error, body);
        return 1;
    }
    printf("ETCDCLIENT get ok node=%d key=azA= value=djA=\n", follower + 1);
    printf("ETCDCLIENT done\n");
    return 0;
}

int main(int argc, char **argv) {
    setvbuf(stdout, NULL, _IOLBF, 0);
    if (argc == 2 && !strcmp(argv[1], "smoke")) return smoke();
    printf("ETCDCLIENT fail usage\n");
    return 64;
}
