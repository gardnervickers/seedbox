/* SPDX-License-Identifier: MIT
 * Static, single-process hardware acceptance fixture. No device/network input.
 */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mount.h>
#include <sys/random.h>
#include <sys/reboot.h>
#include <time.h>
#include <unistd.h>

static void fail(const char *what)
{
    dprintf(1, "SEEDBOX_INIT_FAIL: %s: %s\n", what, strerror(errno));
    reboot(RB_POWER_OFF);
    for (;;) pause();
}
static uint64_t clock_ns(clockid_t id)
{
    struct timespec ts;
    if (clock_gettime(id, &ts)) fail("clock_gettime");
    return (uint64_t)ts.tv_sec * 1000000000ULL + ts.tv_nsec;
}
static void sleep_check(uint64_t ns)
{
    struct timespec remaining = { .tv_sec = ns / 1000000000, .tv_nsec = ns % 1000000000 };
    uint64_t before = clock_ns(CLOCK_MONOTONIC);
    while (nanosleep(&remaining, &remaining)) {
        if (errno != EINTR) fail("nanosleep");
    }
    uint64_t elapsed = clock_ns(CLOCK_MONOTONIC) - before;
    printf("sleep requested=%llu elapsed=%llu\n", (unsigned long long)ns, (unsigned long long)elapsed);
    if (elapsed < ns || elapsed > ns + 20000000) { errno = ERANGE; fail("PV sleep duration"); }
}
static void busy_wait_check(void)
{
    /* CONFIG_HZ=100: 20 ms crosses at least one 10 ms tick without idle.
     * The vDSO is disabled, so each read enters seedbox_read() in the kernel.
     */
    uint64_t before = clock_ns(CLOCK_MONOTONIC), elapsed;
    do {
        elapsed = clock_ns(CLOCK_MONOTONIC) - before;
    } while (elapsed < 20000000);
    printf("busy-wait elapsed=%llu\n", (unsigned long long)elapsed);
}
int main(void)
{
    /* /dev/console is also present in the cpio so printk and init stdio work. */
    if (mount("devtmpfs", "/dev", "devtmpfs", 0, "")) fail("mount /dev");
    int fd = open("/dev/console", O_RDWR);
    if (fd < 0) fail("open console");
    for (int i = 0; i != 3; ++i) if (dup2(fd, i) < 0) fail("dup2");
    if (fd > 2) close(fd);
    setvbuf(stdout, NULL, _IONBF, 0);
    for (int i = 0; i != 3; ++i) {
        uint64_t mono = clock_ns(CLOCK_MONOTONIC), real = clock_ns(CLOCK_REALTIME);
        printf("clock mono=%llu real=%llu\n", (unsigned long long)mono, (unsigned long long)real);
    }
    busy_wait_check();
    sleep_check(10000000);
    sleep_check(1000000000);
    uint64_t first, second;
    __asm__ volatile("mrs %0, cntvct_el0" : "=r"(first));
    __asm__ volatile("mrs %0, cntvct_el0" : "=r"(second));
    printf("cntvct first=%llu second=%llu\n", (unsigned long long)first, (unsigned long long)second);
    if (second <= first || second - first > 1000000) { errno = ERANGE; fail("EL0 CNTVCT emulation"); }
    unsigned char random[32];
    size_t used = 0;
    while (used < sizeof(random)) {
        ssize_t n = getrandom(random + used, sizeof(random) - used, 0);
        if (n < 0 && errno == EINTR) continue;
        if (n <= 0) fail("getrandom");
        used += n;
    }
    printf("getrandom ");
    for (size_t i = 0; i < sizeof(random); ++i) printf("%02x", random[i]);
    printf("\nSEEDBOX_INIT_PASS\n");
    if (reboot(RB_POWER_OFF)) fail("poweroff");
    for (;;) pause();
}
