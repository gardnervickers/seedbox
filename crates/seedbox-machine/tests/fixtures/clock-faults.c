/* Native clock-fault acceptance, shared by TCG and PV. One executable, two roles:
 *   clock       reads CLOCK_REALTIME/MONOTONIC/MONOTONIC_RAW and the machine
 *               counter, sleeps, and arms absolute realtime timerfds.
 *   controller  applies drift, a realtime step, crash/reboot and heals to the
 *               machine named "clock" at fixed simulation times, then exits 0.
 * Every line is "<TAG> rt=<ns> mono=<ns> raw=<ns> cnt=<ns>". */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/timerfd.h>
#include <sys/un.h>
#include <time.h>
#include <unistd.h>

static void fail(const char *phase) {
    (void)dprintf(STDERR_FILENO, "CLOCK_FIXTURE_FAILURE %s errno=%d\n", phase, errno);
    _exit(125);
}
static void put(int fd, const char *text, size_t size) {
    while (size) {
        ssize_t count = write(fd, text, size);
        if (count < 0 && errno == EINTR) continue;
        if (count <= 0) fail("write");
        text += count; size -= (size_t)count;
    }
}
static uint64_t ns(clockid_t clock) {
    struct timespec now;
    if (clock_gettime(clock, &now)) fail("clock_gettime");
    return (uint64_t)now.tv_sec * 1000000000u + (uint64_t)now.tv_nsec;
}
static uint64_t counter(void) {
#if defined(__x86_64__)
    uint32_t lo, hi;
    __asm__ volatile("rdtsc" : "=a"(lo), "=d"(hi) :: "memory");
    return (uint64_t)hi << 32 | lo;
#elif defined(__aarch64__)
    uint64_t ticks, frequency;
    __asm__ volatile("isb; mrs %0, cntvct_el0" : "=r"(ticks) :: "memory");
    __asm__ volatile("mrs %0, cntfrq_el0" : "=r"(frequency));
    return (uint64_t)((unsigned __int128)ticks * 1000000000u / frequency);
#elif defined(__riscv)
    static uint64_t frequency;
    if (!frequency) {
        unsigned char bytes[4];
        int fd = open("/sys/firmware/devicetree/base/cpus/timebase-frequency", O_RDONLY);
        if (fd < 0 || read(fd, bytes, 4) != 4) fail("timebase");
        close(fd);
        frequency = (uint64_t)bytes[0] << 24 | (uint64_t)bytes[1] << 16 | (uint64_t)bytes[2] << 8 | bytes[3];
    }
    uint64_t ticks;
    __asm__ volatile("rdtime %0" : "=r"(ticks) :: "memory");
    return (uint64_t)((unsigned __int128)ticks * 1000000000u / frequency);
#endif
}
static void line(const char *tag) {
    char text[256];
    uint64_t rt = ns(CLOCK_REALTIME), mono = ns(CLOCK_MONOTONIC), raw = ns(CLOCK_MONOTONIC_RAW);
    uint64_t cnt = counter();
    int size = snprintf(text, sizeof(text), "%s rt=%llu mono=%llu raw=%llu cnt=%llu\n", tag,
                        (unsigned long long)rt, (unsigned long long)mono,
                        (unsigned long long)raw, (unsigned long long)cnt);
    if (size < 0 || (size_t)size >= sizeof(text)) fail("line");
    put(STDOUT_FILENO, text, (size_t)size);
}
static int realtime_timer(uint64_t at, int flags) {
    int fd = timerfd_create(CLOCK_REALTIME, TFD_NONBLOCK | TFD_CLOEXEC);
    if (fd < 0) fail("timerfd_create");
    struct itimerspec spec = {.it_value = {(time_t)(at / 1000000000u), (long)(at % 1000000000u)}};
    if (timerfd_settime(fd, TFD_TIMER_ABSTIME | flags, &spec, NULL)) fail("timerfd_settime");
    return fd;
}
static void clock_role(void) {
    line("BOOT");
    uint64_t rt0 = ns(CLOCK_REALTIME);
    /* Cancelled by any realtime step; fired only after a forward step. */
    int cancel = realtime_timer(rt0 + 100000000000ull, TFD_TIMER_CANCEL_ON_SET);
    int absolute = realtime_timer(rt0 + 5000000000ull, 0);
    for (int i = 0; i < 2; ++i) {
        struct timespec second = {.tv_sec = 1};
        while (nanosleep(&second, &second)) if (errno != EINTR) fail("nanosleep");
        line("SLEPT");
    }
    for (;;) {
        struct pollfd fds[2] = {{.fd = cancel, .events = POLLIN}, {.fd = absolute, .events = POLLIN}};
        int ready = poll(fds, 2, 500);
        if (ready < 0 && errno == EINTR) continue;
        if (ready < 0) fail("poll");
        if (!ready) { line("TICK"); continue; }
        uint64_t expirations;
        if (fds[0].revents) {
            if (read(cancel, &expirations, sizeof(expirations)) >= 0 || errno != ECANCELED) fail("cancel-on-set");
            line("CANCELED");
            close(cancel); cancel = -1;
        }
        if (fds[1].revents) {
            if (read(absolute, &expirations, sizeof(expirations)) != sizeof(expirations)) fail("absolute timer");
            line("FIRED");
            close(absolute); absolute = -1;
        }
    }
}

static char reply[4096];
static unsigned next_id;
static const char *call(int fd, const char *operation) {
    char request[512];
    int size = snprintf(request, sizeof(request), "{\"version\":1,\"id\":%u,%s}\n", ++next_id, operation);
    if (size < 0 || (size_t)size >= sizeof(request)) fail("request size");
    put(fd, request, (size_t)size);
    size_t used = 0;
    for (;;) {
        if (used + 1 == sizeof(reply)) fail("reply size");
        ssize_t count = read(fd, reply + used, 1);
        if (count < 0 && errno == EINTR) continue;
        if (count != 1) fail("reply read");
        if (reply[used++] == '\n') break;
    }
    reply[used] = 0;
    if (!strstr(reply, "\"ok\":true")) { put(STDOUT_FILENO, reply, used); fail("reply result"); }
    put(STDOUT_FILENO, "RPC_REPLY ", 10); put(STDOUT_FILENO, reply, used);
    return reply;
}
/* Wait until simulation time `at` through the service's own clock. */
static void until(int fd, uint64_t at) {
    const char *now = strstr(call(fd, "\"op\":\"now\""), "\"now_ns\":");
    if (!now) fail("now_ns");
    uint64_t current = strtoull(now + 9, NULL, 10);
    if (current >= at) return;
    char operation[64];
    snprintf(operation, sizeof(operation), "\"op\":\"sleep\",\"duration_ns\":%llu",
             (unsigned long long)(at - current));
    call(fd, operation);
}
static void controller_role(void) {
    struct sockaddr_un address = {.sun_family = AF_UNIX};
    const char path[] = "/run/seedbox/control.sock";
    memcpy(address.sun_path, path, sizeof(path));
    int fd = socket(AF_UNIX, SOCK_STREAM, 0);
    if (fd < 0 || connect(fd, (struct sockaddr *)&address, sizeof(address))) fail("connect");
    const char *target = "\"target\":{\"machine\":\"clock\"}";
    char operation[256];
    snprintf(operation, sizeof(operation), "\"op\":\"clock_drift\",%s,\"rate_ppb\":100000000", target);
    call(fd, operation);
    until(fd, 2500000000ull);
    snprintf(operation, sizeof(operation), "\"op\":\"clock_skew\",%s,\"offset_ns\":2000000000", target);
    call(fd, operation);
    until(fd, 4000000000ull);
    snprintf(operation, sizeof(operation), "\"op\":\"crash\",%s", target);
    call(fd, operation);
    snprintf(operation, sizeof(operation), "\"op\":\"reboot\",%s", target);
    call(fd, operation);
    until(fd, 5000000000ull);
    call(fd, "\"op\":\"heal\",\"handle\":\"p2\"");
    call(fd, "\"op\":\"heal\",\"handle\":\"p1\"");
    until(fd, 6500000000ull);
    put(STDOUT_FILENO, "CONTROLLER_DONE\n", 16);
}
int main(int argc, char **argv) {
    if (argc != 2) fail("role");
    if (!strcmp(argv[1], "clock")) clock_role();
    else if (!strcmp(argv[1], "controller")) controller_role();
    else fail("unknown role");
    return 0;
}
