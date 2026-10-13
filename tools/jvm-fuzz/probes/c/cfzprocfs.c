/*
 * Kernel-visible state after a fixed workload, one hash per source:
 *   cfzprocfs TEST SEED SCALE      (TEST: procfs)
 * The workload runs threads, writes and syncs files, exchanges bytes over a
 * socketpair and loopback TCP, arms a POSIX timer and forks a child. Then each
 * file in FILES and each syscall result is hashed on its own line, so a twin
 * run or replay that differs names the subsystem (entropy, timers, scheduler
 * accounting, page cache, interrupts, network counters). A file the guest
 * lacks prints "unavailable <errno>" (the same on every run). The values are
 * not comparable with a native run: the probe is replay-only (det: False).
 * Prints "key value" lines and ends with "OK <test>", like cfz.c.
 */
#define _GNU_SOURCE
#include <arpa/inet.h>
#include <errno.h>
#include <fcntl.h>
#include <netinet/in.h>
#include <pthread.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/resource.h>
#include <sys/socket.h>
#include <sys/sysinfo.h>
#include <sys/times.h>
#include <sys/utsname.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

static long seed = 1, scale = 1;
static uint64_t rng = 1;
static uint64_t next(void) { rng ^= rng << 13; rng ^= rng >> 7; rng ^= rng << 17; return rng; }
#define OUT(k, fmt, ...) printf("%s " fmt "\n", k, __VA_ARGS__)

static uint64_t fnv(const void *p, size_t n, uint64_t h)
{
    const unsigned char *b = p;
    for (size_t i = 0; i < n; i++) h = (h ^ b[i]) * 0x100000001b3ULL;
    return h;
}

/* ---- workload ---- */

static void *spin(void *arg)
{
    uint64_t x = (uint64_t)(uintptr_t)arg, h = 0;
    char path[64];
    snprintf(path, sizeof path, "fzprocfs-%lu", (unsigned long)(x & 7));
    int fd = open(path, O_CREAT | O_RDWR | O_TRUNC, 0600);
    char block[4096];
    for (long i = 0; i < 200000 * scale; i++) {
        x = x * 6364136223846793005ULL + 1442695040888963407ULL;
        h ^= x >> 17;
        if (fd >= 0 && i % 20000 == 0) {
            memset(block, (int)(x & 255), sizeof block);
            if (write(fd, block, sizeof block) != (ssize_t)sizeof block) break;
        }
    }
    if (fd >= 0) {
        fsync(fd);
        close(fd);
        unlink(path);
    }
    return (void *)(uintptr_t)h;
}

static void *tcp_server(void *arg)
{
    int listener = *(int *)arg, c = accept(listener, NULL, NULL);
    char buf[4096];
    ssize_t n;
    while (c >= 0 && (n = read(c, buf, sizeof buf)) > 0)
        if (write(c, buf, (size_t)n) != n) break;
    if (c >= 0) close(c);
    return NULL;
}

static int workload(void)
{
    pthread_t t[3];
    for (int i = 0; i < 3; i++) pthread_create(&t[i], NULL, spin, (void *)(uintptr_t)(next() | 1));
    /* A POSIX timer, so /proc/self/timers has an entry. */
    timer_t timer;
    struct sigevent ev = {.sigev_notify = SIGEV_NONE};
    if (timer_create(CLOCK_MONOTONIC, &ev, &timer) == 0) {
        struct itimerspec spec = {.it_value = {.tv_sec = 1000}, .it_interval = {.tv_nsec = 5000000}};
        timer_settime(timer, 0, &spec, NULL);
    }
    int pair[2];
    if (socketpair(AF_UNIX, SOCK_STREAM, 0, pair) == 0) {
        char buf[512];
        for (int i = 0; i < 64; i++) {
            memset(buf, i, sizeof buf);
            if (write(pair[0], buf, sizeof buf) != (ssize_t)sizeof buf || read(pair[1], buf, sizeof buf) <= 0) break;
        }
        close(pair[0]);
        close(pair[1]);
    }
    int listener = socket(AF_INET, SOCK_STREAM, 0);
    struct sockaddr_in addr = {.sin_family = AF_INET, .sin_addr.s_addr = htonl(INADDR_LOOPBACK)};
    socklen_t len = sizeof addr;
    if (listener >= 0 && bind(listener, (struct sockaddr *)&addr, sizeof addr) == 0 && listen(listener, 1) == 0 &&
        getsockname(listener, (struct sockaddr *)&addr, &len) == 0) {
        pthread_t server;
        pthread_create(&server, NULL, tcp_server, &listener);
        int c = socket(AF_INET, SOCK_STREAM, 0);
        if (c >= 0 && connect(c, (struct sockaddr *)&addr, sizeof addr) == 0) {
            char buf[4096];
            for (int i = 0; i < 32 * scale; i++) {
                memset(buf, (int)(next() & 255), sizeof buf);
                if (write(c, buf, sizeof buf) != (ssize_t)sizeof buf) break;
                for (size_t got = 0; got < sizeof buf;) {
                    ssize_t n = read(c, buf, sizeof buf - got);
                    if (n <= 0) break;
                    got += (size_t)n;
                }
            }
        }
        if (c >= 0) close(c);
        pthread_join(server, NULL);
    }
    if (listener >= 0) close(listener);
    int pipefd[2];
    if (pipe(pipefd) != 0) return 0;
    pid_t pid = fork();
    if (pid == 0) {
        close(pipefd[0]);
        uint64_t h = (uint64_t)(uintptr_t)spin((void *)(uintptr_t)(seed | 1));
        _exit(write(pipefd[1], &h, sizeof h) == (ssize_t)sizeof h ? 0 : 1);
    }
    close(pipefd[1]);
    uint64_t child = 0;
    if (read(pipefd[0], &child, sizeof child) != (ssize_t)sizeof child) child = 0;
    close(pipefd[0]);
    int status = -1;
    waitpid(pid, &status, 0);
    uint64_t h = child;
    for (int i = 0; i < 3; i++) {
        void *r;
        pthread_join(t[i], &r);
        h ^= (uint64_t)(uintptr_t)r;
    }
    OUT("workload", "%016llx %d", (unsigned long long)h, status);
    return 1;
}

/* ---- observation ---- */

static const char *FILES[] = {
    "/proc/self/maps", "/proc/self/stat", "/proc/self/status", "/proc/self/sched",
    "/proc/self/schedstat", "/proc/self/io", "/proc/self/timers", "/proc/self/smaps_rollup",
    "/proc/stat", "/proc/uptime", "/proc/loadavg", "/proc/meminfo", "/proc/vmstat",
    "/proc/interrupts", "/proc/softirqs", "/proc/timer_list", "/proc/slabinfo",
    "/proc/buddyinfo", "/proc/diskstats", "/proc/net/dev", "/proc/net/snmp",
    "/proc/net/sockstat", "/proc/sys/kernel/random/boot_id", "/proc/sys/kernel/random/uuid",
};

static void hash_file(const char *path)
{
    int fd = open(path, O_RDONLY | O_CLOEXEC);
    if (fd < 0) {
        OUT("file", "%s unavailable %s", path, strerrorname_np(errno) ? strerrorname_np(errno) : "?");
        return;
    }
    static char buf[1 << 20];
    size_t total = 0;
    uint64_t h = 0xcbf29ce484222325ULL;
    for (;;) {
        ssize_t n = read(fd, buf, sizeof buf);
        if (n < 0) {
            OUT("file", "%s unavailable %s", path, strerrorname_np(errno) ? strerrorname_np(errno) : "?");
            close(fd);
            return;
        }
        if (n == 0) break;
        h = fnv(buf, (size_t)n, h);
        total += (size_t)n;
    }
    close(fd);
    OUT("file", "%s %zu %016llx", path, total, (unsigned long long)h);
}

static void hash_value(const char *name, const void *p, size_t n, long result)
{
    if (result < 0) {
        OUT("call", "%s unavailable %s", name, strerrorname_np(errno) ? strerrorname_np(errno) : "?");
        return;
    }
    OUT("call", "%s %016llx", name, (unsigned long long)fnv(p, n, 0xcbf29ce484222325ULL));
}

static int t_procfs(void)
{
    if (!workload()) return 0;
    for (size_t i = 0; i < sizeof FILES / sizeof FILES[0]; i++) hash_file(FILES[i]);
    struct sysinfo si;
    memset(&si, 0, sizeof si);
    long r = sysinfo(&si);
    hash_value("sysinfo", &si, sizeof si, r);
    struct utsname u;
    memset(&u, 0, sizeof u);
    r = uname(&u);
    hash_value("uname", &u, sizeof u, r);
    struct rusage ru;
    memset(&ru, 0, sizeof ru);
    r = getrusage(RUSAGE_SELF, &ru);
    hash_value("getrusage_self", &ru, sizeof ru, r);
    memset(&ru, 0, sizeof ru);
    r = getrusage(RUSAGE_CHILDREN, &ru);
    hash_value("getrusage_children", &ru, sizeof ru, r);
    struct { struct tms t; clock_t ticks; } tm;
    memset(&tm, 0, sizeof tm);
    tm.ticks = times(&tm.t);
    hash_value("times", &tm, sizeof tm, tm.ticks == (clock_t)-1 ? -1 : 0);
    return 1;
}

static const struct { const char *name; int (*fn)(void); } TESTS[] = {
    {"procfs", t_procfs},
};

int main(int argc, char **argv)
{
    if (argc < 2) {
        for (size_t i = 0; i < sizeof TESTS / sizeof TESTS[0]; i++) puts(TESTS[i].name);
        return 0;
    }
    seed = argc > 2 ? atol(argv[2]) : 1;
    scale = argc > 3 ? atol(argv[3]) : 1;
    rng = (uint64_t)seed * 0x9E3779B97F4A7C15ULL | 1;
    setvbuf(stdout, NULL, _IOLBF, 0);
    for (size_t i = 0; i < sizeof TESTS / sizeof TESTS[0]; i++) {
        if (strcmp(argv[1], TESTS[i].name)) continue;
        if (TESTS[i].fn()) printf("OK %s\n", argv[1]);
        return 0;
    }
    fprintf(stderr, "unknown test %s\n", argv[1]);
    return 2;
}
