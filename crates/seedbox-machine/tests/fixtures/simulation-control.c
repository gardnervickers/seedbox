/* Dedicated ordinary application for native channel tests. No host operations
 * are interpreted: a manual stopped host responder echoes fixture markers. */
#define _POSIX_C_SOURCE 200809L
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <unistd.h>

static void fail(const char *phase) {
    (void)dprintf(STDERR_FILENO, "CONTROL_FIXTURE_FAILURE %s errno=%d\n", phase, errno);
    _exit(125);
}
static int connect_control(void) {
    struct sockaddr_un address = {.sun_family = AF_UNIX};
    const char path[] = "/run/seedbox/control.sock";
    memcpy(address.sun_path, path, sizeof(path));
    int fd = socket(AF_UNIX, SOCK_STREAM, 0);
    if (fd < 0 || connect(fd, (struct sockaddr *)&address, sizeof(address))) fail("connect");
    return fd;
}
static void send_call(int fd, unsigned id, const char *marker) {
    char line[256];
    int length = snprintf(line, sizeof(line), "{\"version\":1,\"id\":%u,\"fixture\":\"%s\"}\n", id, marker);
    if (length < 0 || (size_t)length >= sizeof(line)) fail("format request");
    size_t sent = 0;
    while (sent < (size_t)length) {
        ssize_t count = write(fd, line + sent, (size_t)length - sent);
        if (count < 0 && errno == EINTR) continue;
        if (count <= 0) fail("send request");
        sent += (size_t)count;
    }
}
static void receive_reply(int fd, unsigned id, const char *marker) {
    char line[256], expected[256];
    size_t used = 0;
    while (used + 1 < sizeof(line)) {
        ssize_t count = read(fd, line + used, 1);
        if (count < 0 && errno == EINTR) continue;
        if (count != 1) fail("receive reply");
        if (line[used++] == '\n') break;
    }
    line[used] = 0;
    int length = snprintf(expected, sizeof(expected), "{\"version\":1,\"id\":%u,\"fixture_reply\":\"%s\"}\n", id, marker);
    if (length < 0 || strcmp(line, expected)) {
        (void)dprintf(STDERR_FILENO, "CONTROL_FIXTURE_BAD_REPLY %s", line);
        fail("reply identity/content");
    }
}
static void marker(const char *text) {
    size_t length = strlen(text);
    if (write(STDOUT_FILENO, text, length) != (ssize_t)length) fail("output marker");
}
int main(void) {
    int restarted = access("/tmp/control-initial-started", F_OK) == 0;
    int fd = connect_control();
    if (restarted) {
        send_call(fd, 1, "restarted");
        receive_reply(fd, 1, "restarted");
        marker("RESTARTED\n");
        for (;;) pause();
    }
    int sentinel = open("/tmp/control-initial-started", O_WRONLY | O_CREAT, 0600);
    if (sentinel < 0 || close(sentinel)) fail("restart sentinel");
    send_call(fd, 1, "first");
    receive_reply(fd, 1, "first");
    marker("FIRST\n");
    send_call(fd, 2, "second");
    receive_reply(fd, 2, "second");
    marker("SECOND\n");
    send_call(fd, 3, "abandoned");
    if (close(fd)) fail("close pending request");
    marker("CLOSED\n");
    fd = connect_control();
    send_call(fd, 1, "reconnected");
    receive_reply(fd, 1, "reconnected");
    marker("RECONNECTED\n");
    send_call(fd, 2, "before-stop");
    /* Host Stop/Start interrupts this outstanding call. It must not kill the
     * supervisor or make its reply belong to the replacement app session. */
    receive_reply(fd, 2, "before-stop");
    fail("unexpected host reply before Stop");
}
