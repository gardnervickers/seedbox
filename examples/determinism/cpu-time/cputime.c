/* CPU-time clocks, tick accounting and scheduler statistics under seedbox.
 * Every line is `key: value`; the last is a sha256 digest of all earlier ones. */
#define _GNU_SOURCE
#include <pthread.h>
#include <signal.h>
#include <stdarg.h>
#include <stdatomic.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/resource.h>
#include <sys/syscall.h>
#include <sys/time.h>
#include <sys/times.h>
#include <time.h>
#include <unistd.h>

#define BUSY_ROUNDS 900000 /* 1000 iterations + getpid() per round, ~1.1 us */
#define ALT_ROUNDS 900     /* ~1 ms of virtual compute */
#define ALT_CYCLES 300
#define SLEEP_STEPS 150    /* x 10 ms */
#define NSLOT 5            /* 0 main, 1-2 busy, 3 sleeper, 4 alternator */
#define TRACE_MAX 1024

/* ---- compact SHA-256 ---- */
static uint32_t H[8] = {0x6a09e667,0xbb67ae85,0x3c6ef372,0xa54ff53a,0x510e527f,0x9b05688c,0x1f83d9ab,0x5be0cd19};
static const uint32_t K[64] = {
0x428a2f98,0x71374491,0xb5c0fbcf,0xe9b5dba5,0x3956c25b,0x59f111f1,0x923f82a4,0xab1c5ed5,0xd807aa98,0x12835b01,0x243185be,0x550c7dc3,0x72be5d74,0x80deb1fe,0x9bdc06a7,0xc19bf174,
0xe49b69c1,0xefbe4786,0x0fc19dc6,0x240ca1cc,0x2de92c6f,0x4a7484aa,0x5cb0a9dc,0x76f988da,0x983e5152,0xa831c66d,0xb00327c8,0xbf597fc7,0xc6e00bf3,0xd5a79147,0x06ca6351,0x14292967,
0x27b70a85,0x2e1b2138,0x4d2c6dfc,0x53380d13,0x650a7354,0x766a0abb,0x81c2c92e,0x92722c85,0xa2bfe8a1,0xa81a664b,0xc24b8b70,0xc76c51a3,0xd192e819,0xd6990624,0xf40e3585,0x106aa070,
0x19a4c116,0x1e376c08,0x2748774c,0x34b0bcb5,0x391c0cb3,0x4ed8aa4a,0x5b9cca4f,0x682e6ff3,0x748f82ee,0x78a5636f,0x84c87814,0x8cc70208,0x90befffa,0xa4506ceb,0xbef9a3f7,0xc67178f2};
static unsigned char blk[64]; static size_t blen; static uint64_t total;
#define ROR(x,n) (((x)>>(n))|((x)<<(32-(n))))
static void sha_block(void) {
    uint32_t w[64], a[8];
    for (int i = 0; i < 16; i++) w[i] = (uint32_t)blk[4*i]<<24 | blk[4*i+1]<<16 | blk[4*i+2]<<8 | blk[4*i+3];
    for (int i = 16; i < 64; i++)
        w[i] = w[i-16] + (ROR(w[i-15],7)^ROR(w[i-15],18)^(w[i-15]>>3)) + w[i-7] + (ROR(w[i-2],17)^ROR(w[i-2],19)^(w[i-2]>>10));
    memcpy(a, H, sizeof a);
    for (int i = 0; i < 64; i++) {
        uint32_t t1 = a[7] + (ROR(a[4],6)^ROR(a[4],11)^ROR(a[4],25)) + ((a[4]&a[5])^(~a[4]&a[6])) + K[i] + w[i];
        uint32_t t2 = (ROR(a[0],2)^ROR(a[0],13)^ROR(a[0],22)) + ((a[0]&a[1])^(a[0]&a[2])^(a[1]&a[2]));
        memmove(a + 1, a, 7 * sizeof *a); a[4] += t1; a[0] = t1 + t2;
    }
    for (int i = 0; i < 8; i++) H[i] += a[i];
}
static void sha_update(const char *p, size_t n) {
    total += n;
    while (n--) { blk[blen++] = (unsigned char)*p++; if (blen == 64) { sha_block(); blen = 0; } }
}
static void sha_final(char out[65]) {
    uint64_t bits = total * 8;
    blk[blen++] = 0x80;
    if (blen > 56) { memset(blk + blen, 0, 64 - blen); sha_block(); blen = 0; }
    memset(blk + blen, 0, 56 - blen);
    for (int i = 0; i < 8; i++) blk[56 + i] = (unsigned char)(bits >> (56 - 8 * i));
    sha_block();
    for (int i = 0; i < 8; i++) sprintf(out + 8 * i, "%08x", H[i]);
}

/* ---- output: every line goes to stdout and into the digest ---- */
static void emit(const char *fmt, ...) {
    char line[4096]; va_list ap;
    va_start(ap, fmt); vsnprintf(line, sizeof line - 1, fmt, ap); va_end(ap);
    size_t n = strlen(line);
    while (n && line[n-1] == '\n') line[--n] = 0;   /* keep one line per key */
    for (size_t i = 0; i < n; i++) if (line[i] == '\n') line[i] = ' ';
    line[n++] = '\n'; line[n] = 0;
    sha_update(line, n); fputs(line, stdout); fflush(stdout);
}
static void emit_file(const char *key, const char *path, const char *prefix) {
    char buf[8192], *s, *save = NULL; FILE *f = fopen(path, "r");
    if (!f) { emit("%s: unavailable\n", key); return; }
    size_t n = fread(buf, 1, sizeof buf - 1, f); fclose(f); buf[n] = 0;
    for (s = strtok_r(buf, "\n", &save); s; s = strtok_r(NULL, "\n", &save))
        if (!prefix || !strncmp(s, prefix, strlen(prefix))) emit("%s: %s\n", key, s);
}
static void ts(const char *key, clockid_t id) {
    struct timespec t; clock_gettime(id, &t);
    emit("%s: %lld.%09ld\n", key, (long long)t.tv_sec, t.tv_nsec);
}

/* ---- SIGPROF accounting ---- */
static _Thread_local int slot;
static atomic_uint prof[NSLOT];
static atomic_uint ntrace;
static char trace[TRACE_MAX];
static void on_prof(int sig) {
    (void)sig;
    atomic_fetch_add(&prof[slot], 1);
    unsigned i = atomic_fetch_add(&ntrace, 1);
    if (i < TRACE_MAX) trace[i] = (char)('0' + slot);
}

/* ---- workers ---- */
struct report { long tid; struct timespec cpu; struct rusage ru; char sched[128]; double sink; };
static struct report rep[NSLOT];
static const char *names[NSLOT] = {"main", "busy1", "busy2", "sleeper", "alternator"};

static double compute(long rounds, uint64_t seed) {
    double x = 1.0 + (double)seed; uint64_t h = seed;
    for (long r = 0; r < rounds; r++) {
        for (int i = 0; i < 1000; i++) {
            h = h * 6364136223846793005ULL + (uint64_t)i;
            x = x * 1.0000001 + (double)(h >> 40) * 1e-9;
        }
        syscall(SYS_getpid);                       /* advance virtual time */
    }
    return x + (double)(h & 0xffff);
}
static void sleep_ns(long ns) {
    struct timespec t = {ns / 1000000000L, ns % 1000000000L};
    while (nanosleep(&t, &t) != 0) {}               /* EINTR from SIGPROF */
}
static void *worker(void *arg) {
    slot = (int)(long)arg;
    struct report *r = &rep[slot];
    r->tid = syscall(SYS_gettid);
    if (slot <= 2) r->sink = compute(BUSY_ROUNDS, (uint64_t)slot);
    else if (slot == 3) for (int i = 0; i < SLEEP_STEPS; i++) sleep_ns(10000000L);
    else for (int i = 0; i < ALT_CYCLES; i++) { r->sink += compute(ALT_ROUNDS, (uint64_t)i); sleep_ns(1000000L); }
    clock_gettime(CLOCK_THREAD_CPUTIME_ID, &r->cpu);
    getrusage(RUSAGE_THREAD, &r->ru);
    FILE *f = fopen("/proc/thread-self/schedstat", "r");
    if (!f || !fgets(r->sched, sizeof r->sched, f)) strcpy(r->sched, "unavailable");
    if (f) fclose(f);
    r->sched[strcspn(r->sched, "\n")] = 0;
    return NULL;
}

static unsigned long long counter(void) {
#if defined(__x86_64__)
    unsigned lo, hi; __asm__ volatile("rdtsc" : "=a"(lo), "=d"(hi));
    return (unsigned long long)hi << 32 | lo;
#elif defined(__aarch64__)
    unsigned long long v; __asm__ volatile("isb\n\tmrs %0, cntvct_el0" : "=r"(v) :: "memory");
    return v;
#else
    return 0;
#endif
}

int main(void) {
    struct sigaction sa; memset(&sa, 0, sizeof sa);
    sa.sa_handler = on_prof; sigemptyset(&sa.sa_mask);
    sigaction(SIGPROF, &sa, NULL);
    struct itimerval it = {{0, 10000}, {0, 10000}};
    setitimer(ITIMER_PROF, &it, NULL);

    pthread_t th[NSLOT];
    for (long i = 1; i < NSLOT; i++) pthread_create(&th[i], NULL, worker, (void *)i);
    for (int i = 1; i < NSLOT; i++) pthread_join(th[i], NULL);
    memset(&it, 0, sizeof it); setitimer(ITIMER_PROF, &it, NULL);

    for (int i = 1; i < NSLOT; i++) {
        struct report *r = &rep[i];
        emit("%s_thread_cputime: %lld.%09ld\n", names[i], (long long)r->cpu.tv_sec, r->cpu.tv_nsec);
        emit("%s_rusage: utime=%ld.%06ld stime=%ld.%06ld nvcsw=%ld nivcsw=%ld\n", names[i],
             (long)r->ru.ru_utime.tv_sec, (long)r->ru.ru_utime.tv_usec, (long)r->ru.ru_stime.tv_sec,
             (long)r->ru.ru_stime.tv_usec, r->ru.ru_nvcsw, r->ru.ru_nivcsw);
        emit("%s_schedstat: %s\n", names[i], r->sched);
        emit("%s_result: %.17g\n", names[i], r->sink);
    }
    ts("process_cputime", CLOCK_PROCESS_CPUTIME_ID);
    ts("main_thread_cputime", CLOCK_THREAD_CPUTIME_ID);
    struct rusage ru; getrusage(RUSAGE_SELF, &ru);
    emit("rusage_utime: %ld.%06ld\n", (long)ru.ru_utime.tv_sec, (long)ru.ru_utime.tv_usec);
    emit("rusage_stime: %ld.%06ld\n", (long)ru.ru_stime.tv_sec, (long)ru.ru_stime.tv_usec);
    emit("rusage_nvcsw: %ld\n", ru.ru_nvcsw);
    emit("rusage_nivcsw: %ld\n", ru.ru_nivcsw);
    emit("rusage_minflt: %ld\n", ru.ru_minflt);
    struct tms tm; clock_t el = times(&tm);
    emit("times: elapsed=%lld utime=%lld stime=%lld cutime=%lld cstime=%lld\n", (long long)el,
         (long long)tm.tms_utime, (long long)tm.tms_stime, (long long)tm.tms_cutime, (long long)tm.tms_cstime);

    char st[2048] = ""; FILE *f = fopen("/proc/self/stat", "r");
    if (f) { if (!fgets(st, sizeof st, f)) st[0] = 0; fclose(f); }
    st[strcspn(st, "\n")] = 0;
    emit("proc_self_stat: %s\n", st);
    char *p = strrchr(st, ')');                      /* field 3 starts after "(comm) " */
    unsigned long long fld[64] = {0}; int n = 3;
    for (p = p ? strtok(p + 2, " ") : NULL; p && n < 64; p = strtok(NULL, " "), n++)
        sscanf(p, "%llu", &fld[n]);
    emit("proc_stat_fields: utime=%llu stime=%llu cutime=%llu cstime=%llu starttime=%llu\n",
         fld[14], fld[15], fld[16], fld[17], fld[22]);
    emit_file("proc_self_schedstat", "/proc/self/schedstat", NULL);
    emit_file("proc_stat", "/proc/stat", "cpu");
    emit_file("proc_loadavg", "/proc/loadavg", NULL);
    emit_file("proc_uptime", "/proc/uptime", NULL);
    ts("clock_monotonic", CLOCK_MONOTONIC);
    ts("clock_monotonic_raw", CLOCK_MONOTONIC_RAW);
    ts("clock_boottime", CLOCK_BOOTTIME);
    ts("clock_realtime", CLOCK_REALTIME);

    for (int i = 0; i < NSLOT; i++) emit("sigprof_%s: %u\n", names[i], atomic_load(&prof[i]));
    unsigned nt = atomic_load(&ntrace);
    emit("sigprof_total: %u\n", nt);
    if (nt > TRACE_MAX) nt = TRACE_MAX;
    for (unsigned i = 0; i < nt; i += 64)            /* receiving slot of each SIGPROF, in order */
        emit("sigprof_trace_%04u: %.*s\n", i, (int)(nt - i < 64 ? nt - i : 64), trace + i);

#if defined(__x86_64__) || defined(__aarch64__)
    unsigned long long a = counter(), b = counter();
    emit("tsc: delta=%llu\n", b - a);
#else
    (void)counter;
    emit("tsc: unsupported\n");
#endif
    char d[65]; sha_final(d);
    printf("digest: %s\n", d); fflush(stdout);
    return 0;
}
