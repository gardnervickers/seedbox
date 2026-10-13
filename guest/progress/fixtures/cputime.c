/* User and system CPU time follow the work that consumed them.
 *
 * Translated compute charges guest time through progress calls, which the
 * kernel handles in a syscall; ticks that land there must still count as user
 * time, or utime stays zero and ITIMER_VIRTUAL never fires. Syscall work must
 * stay system time. */
#define _GNU_SOURCE
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/resource.h>
#include <sys/syscall.h>
#include <sys/time.h>
#include <time.h>
#include <unistd.h>

/* Long enough for many 10 ms ticks; ITIMER_VIRTUAL is armed at 2 ms. */
#define PHASE_NS 300000000LL

static volatile sig_atomic_t virtual_signals, prof_signals;

static void on_virtual(int signal) { (void)signal; ++virtual_signals; }
static void on_prof(int signal) { (void)signal; ++prof_signals; }

static void fail(const char *what)
{
    perror(what);
    exit(1);
}

static long long now_ns(clockid_t clock)
{
    struct timespec ts;
    if (clock_gettime(clock, &ts) != 0)
        fail("clock_gettime");
    return ts.tv_sec * 1000000000LL + ts.tv_nsec;
}

struct sample { long long user_us, system_us, cpu_ns; long stat_user, stat_system; };

static struct sample sample(void)
{
    struct sample s;
    struct rusage usage;
    if (getrusage(RUSAGE_SELF, &usage) != 0)
        fail("getrusage");
    s.user_us = usage.ru_utime.tv_sec * 1000000LL + usage.ru_utime.tv_usec;
    s.system_us = usage.ru_stime.tv_sec * 1000000LL + usage.ru_stime.tv_usec;
    s.cpu_ns = now_ns(CLOCK_PROCESS_CPUTIME_ID);
    /* /proc/self/stat fields 14 and 15: utime and stime in clock ticks. */
    char buffer[1024];
    FILE *file = fopen("/proc/self/stat", "r");
    if (!file)
        fail("/proc/self/stat");
    size_t length = fread(buffer, 1, sizeof(buffer) - 1, file);
    fclose(file);
    buffer[length] = 0;
    char *field = strrchr(buffer, ')');
    if (!field || sscanf(field + 2, "%*c %*d %*d %*d %*d %*d %*u %*u %*u %*u %*u %ld %ld",
                         &s.stat_user, &s.stat_system) != 2) {
        fprintf(stderr, "cputime: cannot parse /proc/self/stat\n");
        exit(1);
    }
    return s;
}

static void report(const char *phase, struct sample a, struct sample b)
{
    printf("%s: utime_us=%lld stime_us=%lld cpu_ns=%lld stat_utime=%ld stat_stime=%ld\n",
           phase, b.user_us - a.user_us, b.system_us - a.system_us, b.cpu_ns - a.cpu_ns,
           b.stat_user - a.stat_user, b.stat_system - a.stat_system);
}

static void arm(int which, long interval_us)
{
    struct itimerval value = {{0, interval_us}, {0, interval_us}};
    if (setitimer(which, &value, NULL) != 0)
        fail("setitimer");
}

int main(void)
{
    if (signal(SIGVTALRM, on_virtual) == SIG_ERR || signal(SIGPROF, on_prof) == SIG_ERR)
        fail("signal");

    /* Compute only: no syscalls but the occasional clock read. */
    struct sample before = sample();
    arm(ITIMER_VIRTUAL, 2000);
    arm(ITIMER_PROF, 2000);
    uint64_t x = 0x9e3779b97f4a7c15ULL;
    long long end = now_ns(CLOCK_MONOTONIC) + PHASE_NS;
    do {
        for (unsigned i = 0; i < 1u << 16; ++i) {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
        }
    } while (now_ns(CLOCK_MONOTONIC) < end);
    arm(ITIMER_VIRTUAL, 0);
    arm(ITIMER_PROF, 0);
    struct sample compute = sample();
    report("compute", before, compute);
    printf("compute: itimer_virtual=%d itimer_prof=%d sink=%llu\n", (int)virtual_signals,
           (int)prof_signals, (unsigned long long)(x & 1));

    /* Syscalls only: their cost is the kernel's. */
    end = now_ns(CLOCK_MONOTONIC) + PHASE_NS;
    do {
        for (unsigned i = 0; i < 1000; ++i)
            syscall(SYS_getppid);
    } while (now_ns(CLOCK_MONOTONIC) < end);
    struct sample syscalls = sample();
    report("syscalls", compute, syscalls);

    long long user = compute.user_us - before.user_us;
    long long system = compute.system_us - before.system_us;
    if (virtual_signals < 1 || prof_signals < 1 || user <= system ||
        compute.stat_user - before.stat_user < 1) {
        fprintf(stderr, "cputime: compute is not user time\n");
        return 1;
    }
    if (syscalls.system_us - compute.system_us <= syscalls.user_us - compute.user_us) {
        fprintf(stderr, "cputime: syscall work is not system time\n");
        return 1;
    }
    puts("cputime checks: passed");
    return 0;
}
