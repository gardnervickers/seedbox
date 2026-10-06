#define _GNU_SOURCE
#include <cpuid.h>
#include <errno.h>
#include <fcntl.h>
#include <linux/random.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/auxv.h>
#include <sys/ioctl.h>
#include <sys/mount.h>
#include <sys/random.h>
#include <sys/reboot.h>
#include <time.h>
#include <unistd.h>

static void shutdown_guest(void) {
    fflush(stdout);
    reboot(RB_POWER_OFF);
    for (;;) pause();
}

static void fail(const char *operation) {
    printf("SBX {\"kind\":\"error\",\"operation\":\"%s\",\"errno\":%d}\n", operation, errno);
    shutdown_guest();
}

static uint64_t now(clockid_t clock) {
    struct timespec t;
    if (clock_gettime(clock, &t)) fail("clock_gettime");
    return (uint64_t)t.tv_sec * 1000000000ULL + t.tv_nsec;
}

static uint64_t tsc(void) {
    unsigned lo, hi;
    __asm__ volatile("rdtsc" : "=a"(lo), "=d"(hi));
    return ((uint64_t)hi << 32) | lo;
}

static void hex(const unsigned char *bytes, size_t n) {
    for (size_t i = 0; i < n; ++i) printf("%02x", bytes[i]);
}

static void entropy(const char *phase) {
    unsigned char bytes[32];
    errno = 0;
    ssize_t n = getrandom(bytes, sizeof bytes, GRND_NONBLOCK);
    int error = n < 0 ? errno : 0;
    printf("SBX {\"kind\":\"entropy\",\"phase\":\"%s\",\"count\":%zd,\"errno\":%d,\"bytes\":\"", phase, n, error);
    if (n > 0) hex(bytes, n);
    printf("\",\"at_random\":\"");
    const unsigned char *aux = (const unsigned char *)getauxval(AT_RANDOM);
    if (aux) hex(aux, 16);
    printf("\"}\n");
}

int main(int argc, char **argv) {
    setvbuf(stdout, NULL, _IONBF, 0);
    if (argc == 1) {
        if (mount("devtmpfs", "/dev", "devtmpfs", 0, NULL)) fail("mount_dev");
        int serial = open("/dev/ttyS0", O_RDWR | O_NOCTTY);
        if (serial < 0) fail("serial");
        for (int i = 0; i < 3; ++i) if (dup2(serial, i) < 0) fail("dup2");
        if (serial > 2) close(serial);
        if (mount("proc", "/proc", "proc", 0, NULL)) fail("mount_proc");
        entropy("before_seed");
        char mode = 0;
        int fd = open("/mode", O_RDONLY);
        if (fd < 0 || read(fd, &mode, 1) != 1) fail("mode");
        close(fd);
        if (mode != 'n') {
            struct { int entropy_count, buf_size; unsigned char bytes[32]; } seed = {256, 32, {0}};
            fd = open("/seed", O_RDONLY);
            if (fd < 0 || read(fd, seed.bytes, 32) != 32) fail("seed_read");
            close(fd);
            fd = open("/dev/random", O_RDWR);
            if (fd < 0 || ioctl(fd, RNDADDENTROPY, &seed) < 0) fail("seed_ioctl");
            close(fd);
        }
        /* exec obtains a fresh AT_RANDOM after seeding, like a service launch. */
        char *args[] = {"/probe", mode == 'p' ? "plant" : "payload", NULL};
        char *env[] = {NULL};
        execve(args[0], args, env);
        fail("exec");
    }

    entropy("payload");
    unsigned char random[32];
    if (getrandom(random, sizeof random, GRND_NONBLOCK) != sizeof random) {
        printf("SBX {\"kind\":\"result\",\"status\":\"FAIL\",\"reason\":\"entropy_not_ready\"}\n");
        shutdown_guest();
    }
    if (getrandom(random, sizeof random, 0) != sizeof random) fail("blocking_getrandom");
    unsigned a, b, c, d;
    __cpuid(1, a, b, c, d);
    int rdrand = !!(c & (1u << 30));
    __cpuid_count(7, 0, a, b, c, d);
    int rdseed = !!(b & (1u << 18));
    printf("SBX {\"kind\":\"cpu\",\"rdrand\":%d,\"rdseed\":%d}\n", rdrand, rdseed);
    if (rdrand || rdseed) fail("unexpected_cpu_rng");

    uint64_t h = 0;
    memcpy(&h, random, sizeof h);
    printf("SBX {\"kind\":\"start\",\"monotonic_ns\":%llu,\"realtime_ns\":%llu}\n",
           (unsigned long long)now(CLOCK_MONOTONIC), (unsigned long long)now(CLOCK_REALTIME));
    for (unsigned round = 0; round < 3; ++round) {
        uint64_t before = now(CLOCK_MONOTONIC), r0 = tsc();
        for (uint64_t i = 0; i < 2000000; ++i) {
            h ^= h << 13; h ^= h >> 7; h ^= h << 17;
            h += i + round;
        }
        uint64_t after = now(CLOCK_MONOTONIC), r1 = tsc();
        printf("SBX {\"kind\":\"cpu_work\",\"round\":%u,\"hash\":\"%016llx\",\"before_ns\":%llu,\"after_ns\":%llu,\"tsc_before\":%llu,\"tsc_after\":%llu}\n",
               round, (unsigned long long)h, (unsigned long long)before, (unsigned long long)after,
               (unsigned long long)r0, (unsigned long long)r1);
        if (after <= before || r1 <= r0) fail("cpu_clock_progress");
        struct timespec delay = {0, (round + 1) * 10000000L};
        uint64_t requested = delay.tv_nsec;
        before = now(CLOCK_MONOTONIC);
        while (nanosleep(&delay, &delay)) if (errno != EINTR) fail("nanosleep");
        after = now(CLOCK_MONOTONIC);
        printf("SBX {\"kind\":\"sleep\",\"round\":%u,\"requested_ns\":%llu,\"before_ns\":%llu,\"after_ns\":%llu}\n",
               round, (unsigned long long)requested, (unsigned long long)before, (unsigned long long)after);
        if (after - before < requested) fail("early_sleep");
    }
    /* Exercise clock polling: a frozen clock would hang or hit this bound. */
    uint64_t before = now(CLOCK_MONOTONIC), after = before;
    unsigned polls = 0;
    while (after - before < 1000000 && polls < 100000) {
        after = now(CLOCK_MONOTONIC);
        ++polls;
    }
    printf("SBX {\"kind\":\"poll\",\"count\":%u,\"before_ns\":%llu,\"after_ns\":%llu}\n",
           polls, (unsigned long long)before, (unsigned long long)after);
    if (after - before < 1000000) fail("poll_progress");
    printf("SBX {\"kind\":\"result\",\"status\":\"%s\",\"reason\":\"%s\"}\n",
           strcmp(argv[1], "plant") == 0 ? "FAIL" : "PASS",
           strcmp(argv[1], "plant") == 0 ? "planted_failure" : "completed");
    shutdown_guest();
}
