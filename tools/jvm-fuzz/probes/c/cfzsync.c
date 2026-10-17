/*
 * Native AArch64 probes for the translator's stop-all-threads synchronization:
 * code modification and cache maintenance against threads and processes in
 * every state (NEXT-100 cases 27-31).
 *   cfzsync TEST SEED SCALE
 * Same conventions as cfz.c: each test prints deterministic "key value" lines
 * and ends with "OK <test>", or "UNSUPPORTED <what>" when the host lacks a
 * feature. Timing-dependent diagnostics go to stderr, and only with CFZ_DIAG set,
 * because the runner compares stderr too. A wait that never
 * completes prints "STUCK ..." (or "TIMEOUT <test>") and exits with status 3.
 *
 *   flushmatrix  27 FlushMatrix: 1 kHz code flushes against threads blocked in
 *                every gated state, spinners, and destructive child phases
 *   syscallivau  28 SyscallOnceAfterPendingIvau: a syscall between IC IVAU and DSB
 *   forkcallers  29 CompetingForkCallers: two fork callers, two code writers
 *   crossivau    30 CrossThreadIvau: write, invalidate and execute on chosen threads
 *   clonestack   31 CloneStackReuse: threads and raw clones on one reused stack
 */
#define _GNU_SOURCE
#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <limits.h>
#include <linux/futex.h>
#include <linux/io_uring.h>
#include <poll.h>
#include <pthread.h>
#include <sched.h>
#include <signal.h>
#include <spawn.h>
#include <stddef.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/epoll.h>
#include <sys/eventfd.h>
#include <sys/mman.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/uio.h>
#include <sys/un.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#ifndef CLONE_VM
#define CLONE_VM 0x00000100
#endif

extern char **environ;
static long seed = 1, scale = 1;
static int diag;
static uint64_t rng = 1;
static uint64_t next(void) { rng ^= rng << 13; rng ^= rng >> 7; rng ^= rng << 17; return rng; }
static uint64_t xnext(uint64_t *s) { *s ^= *s << 13; *s ^= *s >> 7; *s ^= *s << 17; return *s; }
static uint64_t xseed(long id) { return ((uint64_t)seed * 0x9E3779B97F4A7C15ULL + (uint64_t)id * 0xD1B54A32D192ED03ULL) | 1; }
#define OUT(k, fmt, ...) printf("%s " fmt "\n", k, __VA_ARGS__)
static const char *self_path, *test_name;
/* An optional fourth argument (hex) narrows a test while shrinking a failure: flushmatrix rows
 * (bits 0-18) and destructive phases (bits 19-21); clonestack kinds (bits 0-5, others run as
 * "return") and bit 8 to ignore signal-mask mismatches. */
static unsigned long narrow = ~0UL;

/* ---- time, futexes, tasks ---- */

static double now(void) {
    struct timespec t;
    clock_gettime(CLOCK_MONOTONIC, &t);
    return t.tv_sec + t.tv_nsec / 1e9;
}
static void sleep_us(long us) {
    struct timespec t = {us / 1000000, (us % 1000000) * 1000};
    while (nanosleep(&t, &t) && errno == EINTR) {}
}
static long futex(int *u, int op, int v) { return syscall(SYS_futex, u, op, v, NULL, NULL, 0); }
static int load(int *u) { return __atomic_load_n(u, __ATOMIC_ACQUIRE); }
/* Waits until *u >= target; *u only grows. */
static void wait_until(int *u, int target) {
    for (;;) {
        int v = load(u);
        if (v >= target) return;
        futex(u, FUTEX_WAIT_PRIVATE, v);
    }
}
static void set_wake(int *u, int v) {
    __atomic_store_n(u, v, __ATOMIC_RELEASE);
    futex(u, FUTEX_WAKE_PRIVATE, INT_MAX);
}
static pid_t tid(void) { return (pid_t)syscall(SYS_gettid); }

static int read_small(const char *path, char *buf, size_t n) {
    int fd = open(path, O_RDONLY);
    if (fd < 0) return -1;
    ssize_t k = read(fd, buf, n - 1);
    close(fd);
    if (k <= 0) return -1;
    buf[k] = 0;
    return (int)k;
}
/* The syscall a task is blocked in, -1 when running or not in a syscall, -2 when unknown. */
static int task_syscall(pid_t t) {
    char path[64], buf[256];
    snprintf(path, sizeof path, "/proc/self/task/%d/syscall", (int)t);
    if (read_small(path, buf, sizeof buf) < 0) return -2;
    return buf[0] >= '0' && buf[0] <= '9' ? atoi(buf) : -1;
}
static char task_state(pid_t t) {
    char path[64], buf[512];
    snprintf(path, sizeof path, "/proc/self/task/%d/stat", (int)t);
    if (read_small(path, buf, sizeof buf) < 0) return '?';
    char *p = strrchr(buf, ')');
    return p && p[1] && p[2] ? p[2] : '?';
}
static void dump_tasks(void) {
    DIR *d = opendir("/proc/self/task");
    if (!d) return;
    struct dirent *e;
    while ((e = readdir(d))) {
        if (e->d_name[0] == '.') continue;
        char path[320], comm[64] = "?", sys[256] = "?";
        snprintf(path, sizeof path, "/proc/self/task/%s/comm", e->d_name);
        read_small(path, comm, sizeof comm);
        comm[strcspn(comm, "\n")] = 0;
        snprintf(path, sizeof path, "/proc/self/task/%s/syscall", e->d_name);
        read_small(path, sys, sizeof sys);
        sys[strcspn(sys, "\n")] = 0;
        fprintf(stderr, "task %s %s state %c syscall %s\n", e->d_name, comm, task_state(atoi(e->d_name)), sys);
    }
    closedir(d);
}
static void stuck(const char *what, int round) {
    printf("STUCK %s round %d\n", what, round);
    fflush(stdout);
    dump_tasks();
    _exit(3);
}
static void *watchdog(void *arg) {
    sleep_us((long)arg * 1000000L);
    printf("TIMEOUT %s\n", test_name);
    fflush(stdout);
    dump_tasks();
    _exit(3);
}

/* ---- generated code ---- */

static size_t iline = 64, dline = 64;
static void cache_lines(void) {
    uint64_t c;
    asm volatile("mrs %0, ctr_el0" : "=r"(c));
    iline = 4UL << (c & 15);
    dline = 4UL << ((c >> 16) & 15);
}
static void dc_range(const void *p, size_t n) {
    for (uintptr_t a = (uintptr_t)p & ~(dline - 1); a < (uintptr_t)p + n; a += dline) asm volatile("dc cvau, %0" ::"r"(a) : "memory");
}
static void ic_range(const void *p, size_t n) {
    for (uintptr_t a = (uintptr_t)p & ~(iline - 1); a < (uintptr_t)p + n; a += iline) asm volatile("ic ivau, %0" ::"r"(a) : "memory");
}
static inline void dsb(void) { asm volatile("dsb ish" ::: "memory"); }
static inline void isb(void) { asm volatile("isb" ::: "memory"); }
static void *jit_page(size_t n) {
    void *p = mmap(NULL, n, PROT_READ | PROT_WRITE | PROT_EXEC, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (p == MAP_FAILED) { perror("mmap rwx"); exit(2); }
    return p;
}
typedef uint32_t (*fn_t)(void);
#define NOP 0xd503201fu
#define RET 0xd65f03c0u
static uint32_t movz(uint16_t v) { return 0x52800000u | (uint32_t)v << 5; }
/* movz w0, #lo ; movk w0, #hi, lsl #16 ; ret */
static void emit_tag(uint32_t *p, uint32_t v) {
    p[0] = movz((uint16_t)v);
    p[1] = 0x72a00000u | (v >> 16) << 5;
    p[2] = RET;
}

/* A thread that keeps rewriting one-instruction "movz w0, #v; ret" slots of its own page,
 * flushing each and calling it. Values carry the flusher id and the slot, so concurrent
 * callers can check what they ran without depending on timing. */
struct fl {
    uint32_t *code;
    int id, stop;
    long period, bad, count;
    pthread_t t;
};
static uint16_t fl_value(int id, uint32_t gen, int slot) { return (uint16_t)(id << 15 | (gen & 0x1ff) << 6 | slot); }
static int fl_check(const struct fl *f, int slot, uint32_t v) { return (v & 63) == (uint32_t)slot && v >> 15 == (uint32_t)f->id && v < 65536; }
static uint32_t fl_call(const struct fl *f, int slot) { return ((fn_t)(f->code + slot * 2))(); }
static void *fl_main(void *arg) {
    struct fl *f = arg;
    for (uint32_t i = 1; !load(&f->stop); i++) {
        int slot = i % 64;
        uint32_t *p = f->code + slot * 2;
        uint16_t v = fl_value(f->id, i, slot);
        __atomic_store_n(p, movz(v), __ATOMIC_RELAXED);
        if (i & 1) {
            __builtin___clear_cache((char *)p, (char *)(p + 1));
        } else {
            dc_range(p, 4);
            dsb();
            ic_range(p, 4);
            dsb();
            isb();
        }
        f->bad += ((fn_t)p)() != v;
        f->count++;
        if (f->period) sleep_us(f->period);
    }
    return NULL;
}
static void fl_start(struct fl *f, int id, long period) {
    memset(f, 0, sizeof *f);
    f->code = jit_page(4096);
    for (int s = 0; s < 64; s++) { f->code[s * 2] = movz(fl_value(id, 0, s)); f->code[s * 2 + 1] = RET; }
    __builtin___clear_cache((char *)f->code, (char *)f->code + 4096);
    f->id = id;
    f->period = period;
    pthread_create(&f->t, NULL, fl_main, f);
}
static void fl_finish(struct fl *f) {
    __atomic_store_n(&f->stop, 1, __ATOMIC_RELEASE);
    pthread_join(f->t, NULL);
}

/* ==== 30. CrossThreadIvau ==== */

enum { XI_WRITE = 1, XI_INVAL, XI_EXEC, XI_STOP };
#define XI_SLOTS 16
#define XI_SLOT 256
static struct xi_box { int go, done, op, pad[13]; } xi_box[3];
static uint8_t *xi_w, *xi_x;
static uint32_t xi_off, xi_len, xi_pad, xi_gen, xi_got;
static uintptr_t xi_ptr;

static void *xi_thread(void *arg) {
    struct xi_box *b = &xi_box[(long)arg];
    for (int seen = 1;; seen++) {
        wait_until(&b->go, seen);
        int op = b->op;
        if (op == XI_STOP) break;
        if (op == XI_WRITE) {
            uint32_t *p = (uint32_t *)(xi_w + xi_off);
            for (uint32_t k = 0; k < xi_pad; k++) p[k] = NOP;
            emit_tag(p + xi_pad, xi_gen);
            dc_range(p, xi_len);
            dsb();
        } else if (op == XI_INVAL) {
            ic_range(xi_x + xi_off, xi_len);
            dsb();
            isb();
            __atomic_store_n(&xi_ptr, (uintptr_t)(xi_x + xi_off), __ATOMIC_RELEASE);
        } else if (op == XI_EXEC) {
            fn_t f = (fn_t)__atomic_load_n(&xi_ptr, __ATOMIC_ACQUIRE);
            isb();
            __atomic_store_n(&xi_got, f(), __ATOMIC_RELAXED);
        }
        set_wake(&b->done, seen);
    }
    return NULL;
}
static void xi_run(int t, int op) {
    struct xi_box *b = &xi_box[t];
    b->op = op;
    int g = b->go + 1;
    set_wake(&b->go, g);
    if (op != XI_STOP) wait_until(&b->done, g);
}
static int t_crossivau(void) {
    uint8_t *rwx = jit_page(XI_SLOTS * XI_SLOT);
    int fd = memfd_create("xi", 0);
    if (fd < 0 || ftruncate(fd, XI_SLOTS * XI_SLOT)) { puts("UNSUPPORTED memfd"); return 0; }
    uint8_t *aw = mmap(0, XI_SLOTS * XI_SLOT, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    uint8_t *ax = mmap(0, XI_SLOTS * XI_SLOT, PROT_READ | PROT_EXEC, MAP_SHARED, fd, 0);
    if (aw == MAP_FAILED || ax == MAP_FAILED) { puts("UNSUPPORTED alias"); return 0; }
    uint32_t last[2][XI_SLOTS];
    for (int s = 0; s < XI_SLOTS; s++) {
        last[0][s] = last[1][s] = 0xfff00000u | s;
        emit_tag((uint32_t *)(rwx + s * XI_SLOT), last[0][s]);
        emit_tag((uint32_t *)(aw + s * XI_SLOT), last[1][s]);
    }
    __builtin___clear_cache((char *)rwx, (char *)rwx + XI_SLOTS * XI_SLOT);
    dc_range(aw, XI_SLOTS * XI_SLOT);
    dsb();
    ic_range(ax, XI_SLOTS * XI_SLOT);
    dsb();
    isb();
    pthread_t t[3];
    for (long k = 0; k < 3; k++) pthread_create(&t[k], NULL, xi_thread, (void *)k);
    long rounds = 10000 * scale, stale = 0, warm = 0, warmStale = 0;
    long pat[5] = {0}; /* all one thread, writer=invalidator, invalidator=executor, writer=executor, all distinct */
    uint64_t hash = 1469598103934665603ULL;
    for (long r = 0; r < rounds; r++) {
        int w = next() % 3, i = next() % 3, e = next() % 3, alias = next() & 1, slot = next() % XI_SLOTS;
        xi_pad = next() % 40;
        xi_len = (xi_pad + 3) * 4;
        xi_off = slot * XI_SLOT;
        xi_w = alias ? aw : rwx;
        xi_x = alias ? ax : rwx;
        if (next() & 1) {
            /* Make sure the executor has the current code translated before it changes. */
            warm++;
            __atomic_store_n(&xi_ptr, (uintptr_t)(xi_x + xi_off), __ATOMIC_RELEASE);
            xi_run(e, XI_EXEC);
            warmStale += __atomic_load_n(&xi_got, __ATOMIC_RELAXED) != last[alias][slot];
        }
        xi_gen = (uint32_t)slot << 24 | (uint32_t)(r + 1);
        xi_run(w, XI_WRITE);
        xi_run(i, XI_INVAL);
        xi_run(e, XI_EXEC);
        uint32_t got = __atomic_load_n(&xi_got, __ATOMIC_RELAXED);
        if (got != xi_gen) {
            if (stale < 5) fprintf(stderr, "round %ld w%d i%d e%d alias %d slot %d pad %u: got %#x want %#x\n", r, w, i, e, alias, slot, xi_pad, got, xi_gen);
            stale++;
        }
        last[alias][slot] = xi_gen;
        hash = (hash ^ got) * 1099511628211ULL;
        pat[w == i && i == e ? 0 : w == i ? 1 : i == e ? 2 : w == e ? 3 : 4]++;
    }
    for (int k = 0; k < 3; k++) xi_run(k, XI_STOP);
    for (int k = 0; k < 3; k++) pthread_join(t[k], NULL);
    OUT("rounds", "%ld", rounds);
    OUT("patterns", "same %ld wi %ld ie %ld we %ld distinct %ld", pat[0], pat[1], pat[2], pat[3], pat[4]);
    OUT("warm", "%ld", warm);
    OUT("warmStale", "%ld", warmStale);
    OUT("stale", "%ld", stale);
    OUT("hash", "%016lx", (unsigned long)hash);
    return 1;
}

/* ==== 28. SyscallOnceAfterPendingIvau ==== */

static long so_n;
static int so_tpipe[2], so_efd, so_req[2][2], so_pub[2] = {-1, -1}, so_finished;
static uint8_t *so_mode[2];
static uint32_t *so_code[2];
static long so_consumed[2], so_need3[2], so_tickets[2], so_efd_expect[2];
static long so_own_bad[2], so_read_bad[2], so_sys_bad[2], so_reader_bad[2];
static uint32_t so_tag(int w, long s) { return (uint32_t)(w + 1) << 24 | (uint32_t)s; }

/* IC IVAU over [a, end), then one raw syscall, then DSB ISH; ISB: no DSB between the two. */
static long so_ivau_svc(uintptr_t a, uintptr_t end, long nr, long a0, long a1, long a2) {
    register long x0 asm("x0") = a0;
    register long x1 asm("x1") = a1;
    register long x2 asm("x2") = a2;
    register long x8 asm("x8") = nr;
    uintptr_t step = iline;
    asm volatile("1: ic ivau, %[a]\n"
                 "add %[a], %[a], %[st]\n"
                 "cmp %[a], %[e]\n"
                 "b.lo 1b\n"
                 "svc #0\n"
                 "dsb ish\n"
                 "isb"
                 : "+r"(x0), [a] "+r"(a)
                 : "r"(x1), "r"(x2), "r"(x8), [st] "r"(step), [e] "r"(end)
                 : "memory", "cc");
    return x0;
}
static void *so_writer(void *arg) {
    int w = (int)(long)arg;
    uint64_t rs = xseed(100 + w);
    uint32_t *code = so_code[w];
    long consumed = 0;
    for (long s = 0; s < so_n; s++) {
        uint32_t *p = code + (s % 64) * 16;
        emit_tag(p, so_tag(w, s));
        dc_range(p, 12);
        dsb();
        uintptr_t a = (uintptr_t)p & ~(iline - 1), end = (uintptr_t)p + (1 + xnext(&rs) % 3) * 64;
        if (end > (uintptr_t)code + 4096) end = (uintptr_t)code + 4096;
        uint64_t ticket = (uint64_t)w << 32 | (uint64_t)s;
        long ret = -1, want = 8;
        switch (so_mode[w][s]) {
        case 0:
            ret = so_ivau_svc(a, end, SYS_write, so_tpipe[1], (long)&ticket, 8);
            break;
        case 1: {
            uint64_t v = 1 + s % 7;
            so_efd_expect[w] += v;
            ret = so_ivau_svc(a, end, SYS_write, so_efd, (long)&v, 8);
            break;
        }
        case 2: {
            struct iovec iov[2] = {{&ticket, 4}, {(char *)&ticket + 4, 4}};
            ret = so_ivau_svc(a, end, SYS_writev, so_tpipe[1], (long)iov, 2);
            break;
        }
        default: {
            uint8_t b = 0xee;
            want = 1;
            ret = so_ivau_svc(a, end, SYS_read, so_req[w][0], (long)&b, 1);
            so_read_bad[w] += b != (uint8_t)consumed;
            __atomic_store_n(&so_consumed[w], ++consumed, __ATOMIC_RELEASE);
            break;
        }
        }
        if (ret != want) {
            if (so_sys_bad[w] < 5) fprintf(stderr, "writer %d seq %ld mode %d: syscall returned %ld\n", w, s, so_mode[w][s], ret);
            so_sys_bad[w]++;
        }
        so_own_bad[w] += ((fn_t)p)() != so_tag(w, s);
        __atomic_store_n(&so_pub[w], (int)s, __ATOMIC_RELEASE);
    }
    __atomic_add_fetch(&so_finished, 1, __ATOMIC_RELEASE);
    return NULL;
}
static void *so_reader(void *arg) {
    long id = (long)arg, n = 0, checked = 0;
    while (load(&so_finished) < 2) {
        for (int w = 0; w < 2; w++) {
            int s1 = load(&so_pub[w]);
            if (s1 < 0) continue;
            isb();
            uint32_t got = ((fn_t)(so_code[w] + (s1 % 64) * 16))();
            int s2 = load(&so_pub[w]);
            /* Slot s1 % 64 is rewritten only after s1 + 63 is published. */
            if (s2 - s1 <= 62) {
                checked++;
                if (got != so_tag(w, s1)) {
                    if (so_reader_bad[id] < 5) fprintf(stderr, "reader %ld writer %d seq %d: got %#x\n", id, w, s1, got);
                    so_reader_bad[id]++;
                }
            }
        }
        if ((++n & 15) == 0) sleep_us(20);
    }
    if (diag) fprintf(stderr, "reader %ld checked %ld\n", id, checked);
    return NULL;
}
static void *so_pinger(void *arg) {
    (void)arg;
    long sent[2] = {0, 0};
    while (sent[0] < so_need3[0] || sent[1] < so_need3[1]) {
        for (int w = 0; w < 2; w++) {
            /* Send the next byte only once the previous one was consumed, so reads block. */
            if (sent[w] < so_need3[w] && __atomic_load_n(&so_consumed[w], __ATOMIC_ACQUIRE) >= sent[w]) {
                uint8_t b = (uint8_t)sent[w];
                if (write(so_req[w][1], &b, 1) == 1) sent[w]++;
            }
        }
        sleep_us(100);
    }
    return NULL;
}
static long so_dup, so_missing, so_unexpected, so_received, so_poll_eintr;
static void *so_collector(void *arg) {
    (void)arg;
    uint8_t *seen = calloc(2 * so_n, 1);
    for (;;) {
        struct pollfd pf = {so_tpipe[0], POLLIN, 0};
        int k = poll(&pf, 1, 50);
        if (k < 0 && errno == EINTR) { so_poll_eintr++; continue; } /* none natively */
        if (k <= 0) {
            if (load(&so_finished) >= 2) break;
            continue;
        }
        uint64_t t;
        size_t got = 0;
        while (got < 8) {
            ssize_t m = read(so_tpipe[0], (char *)&t + got, 8 - got);
            if (m < 0 && errno == EINTR) continue;
            if (m <= 0) break;
            got += m;
        }
        if (got < 8) break;
        so_received++;
        uint64_t w = t >> 32, s = t & 0xffffffff;
        if (w > 1 || s >= (uint64_t)so_n) { so_unexpected++; continue; }
        if (seen[w * so_n + s] < 255) seen[w * so_n + s]++;
    }
    for (int w = 0; w < 2; w++)
        for (long s = 0; s < so_n; s++) {
            int c = seen[w * so_n + s], want = so_mode[w][s] == 0 || so_mode[w][s] == 2;
            if (!want) so_unexpected += c;
            else if (c == 0) so_missing++;
            else so_dup += c - 1;
        }
    free(seen);
    if (diag) fprintf(stderr, "collector poll eintr %ld\n", so_poll_eintr);
    return NULL;
}
static int t_syscallivau(void) {
    so_n = 1500 * scale;
    if (pipe(so_tpipe) || pipe(so_req[0]) || pipe(so_req[1])) { perror("pipe"); return 0; }
    fcntl(so_tpipe[1], F_SETPIPE_SZ, 4096); /* small, so ticket writes sometimes block */
    so_efd = eventfd(0, EFD_NONBLOCK);
    for (int w = 0; w < 2; w++) {
        so_code[w] = jit_page(4096);
        for (int s = 0; s < 64; s++) emit_tag(so_code[w] + s * 16, 0xff000000u | s);
        __builtin___clear_cache((char *)so_code[w], (char *)so_code[w] + 4096);
        so_mode[w] = malloc(so_n);
        for (long s = 0; s < so_n; s++) {
            so_mode[w][s] = next() % 4;
            so_need3[w] += so_mode[w][s] == 3;
            so_tickets[w] += so_mode[w][s] == 0 || so_mode[w][s] == 2;
        }
    }
    pthread_t wt[2], rt[2], col, pin;
    pthread_create(&col, NULL, so_collector, NULL);
    pthread_create(&pin, NULL, so_pinger, NULL);
    for (long k = 0; k < 2; k++) pthread_create(&rt[k], NULL, so_reader, (void *)k);
    for (long k = 0; k < 2; k++) pthread_create(&wt[k], NULL, so_writer, (void *)k);
    for (int k = 0; k < 2; k++) pthread_join(wt[k], NULL);
    for (int k = 0; k < 2; k++) pthread_join(rt[k], NULL);
    pthread_join(pin, NULL);
    pthread_join(col, NULL);
    uint64_t efd = 0;
    if (read(so_efd, &efd, 8) != 8) efd = 0;
    OUT("iterations", "%ld %ld", so_n, so_n);
    OUT("tickets", "expect %ld received %ld dup %ld missing %ld unexpected %ld", so_tickets[0] + so_tickets[1], so_received, so_dup, so_missing, so_unexpected);
    OUT("eventfd", "expect %ld got %lu", so_efd_expect[0] + so_efd_expect[1], (unsigned long)efd);
    OUT("reads", "expect %ld badByte %ld", so_need3[0] + so_need3[1], so_read_bad[0] + so_read_bad[1]);
    OUT("syscallBad", "%ld", so_sys_bad[0] + so_sys_bad[1]);
    OUT("ownStale", "%ld", so_own_bad[0] + so_own_bad[1]);
    OUT("readerStale", "%ld", so_reader_bad[0] + so_reader_bad[1]);
    return 1;
}

/* ==== 29. CompetingForkCallers ==== */

#define FC_PHASES 4
static int fc_go, fc_done[2], fc_rec[2], fc_k, fc_exec_stop;
static struct fl fc_w[2];
static uint32_t *fc_phasefn;
static int fc_reaped[FC_PHASES][2], fc_ok[FC_PHASES][2];
static long fc_exec_bad;
static int fc_code(int c, int p, int i) { return 1 + (c * 97 + p * 13 + i * 3) % 200; }
static uint32_t fc_phasetag(int p) { return 0x5a000000u | (uint32_t)p << 16 | (uint32_t)(seed & 0xffff); }
static uint64_t fc_record(int c, int p, int i, uint32_t t, int ok) { return (uint64_t)c | (uint64_t)p << 4 | (uint64_t)i << 8 | (uint64_t)ok << 31 | (uint64_t)t << 32; }
/* Runs in a fork or vfork child: raw syscalls only. */
static int fc_child_body(int c, int p, int i) {
    uint32_t t = ((fn_t)fc_phasefn)();
    int ok = 1;
    for (int k = 0; k < 8; k++)
        for (int w = 0; w < 2; w++) {
            int slot = (k * 7 + i) % 64;
            ok &= fl_check(&fc_w[w], slot, fl_call(&fc_w[w], slot));
        }
    uint64_t rec = fc_record(c, p, i, t, ok);
    syscall(SYS_write, fc_rec[1], &rec, 8);
    return ok ? fc_code(c, p, i) : 255;
}
/* The vfork child runs the phase's generated function, then execs a helper that writes the
 * record (a vfork child that exits without exec is covered by clonestack). */
static __attribute__((noinline)) pid_t fc_vfork(char **ok_av, char **bad_av, uint32_t tag) {
    pid_t pid = vfork();
    if (pid == 0) {
        char **av = ((fn_t)fc_phasefn)() == tag ? ok_av : bad_av;
        execve(self_path, av, environ);
        _exit(252);
    }
    return pid;
}
static void *fc_caller(void *arg) {
    int c = (int)(long)arg;
    for (int p = 0; p < FC_PHASES; p++) {
        wait_until(&fc_go, p + 1);
        for (int i = 0; i < fc_k; i++) {
            int mode = c == 0 ? 0 : i % 3;
            char a[5][24];
            snprintf(a[0], 24, "%d", fc_rec[1]);
            snprintf(a[1], 24, "%d", c);
            snprintf(a[2], 24, "%d", p);
            snprintf(a[3], 24, "%d", i);
            snprintf(a[4], 24, "%u", fc_phasetag(p));
            char *av[] = {(char *)self_path, "fcchild", a[0], a[1], a[2], a[3], a[4], NULL};
            char *bad_av[] = {(char *)self_path, "fcchild", a[0], a[1], a[2], a[3], "0", NULL};
            pid_t pid;
            if (mode == 0) {
                pid = fork();
                if (pid == 0) _exit(fc_child_body(c, p, i));
            } else if (mode == 1) {
                pid = fc_vfork(av, bad_av, fc_phasetag(p));
            } else {
                if (posix_spawn(&pid, self_path, NULL, NULL, av, environ)) pid = -1;
            }
            int st = 0;
            if (pid > 0 && waitpid(pid, &st, 0) == pid) {
                fc_reaped[p][c]++;
                fc_ok[p][c] += WIFEXITED(st) && WEXITSTATUS(st) == fc_code(c, p, i);
                if (!WIFEXITED(st) || WEXITSTATUS(st) != fc_code(c, p, i))
                    fprintf(stderr, "caller %d phase %d child %d mode %d: status %#x\n", c, p, i, mode, st);
            } else {
                fprintf(stderr, "caller %d phase %d child %d mode %d: pid %d errno %s\n", c, p, i, mode, (int)pid, strerror(errno));
            }
        }
        set_wake(&fc_done[c], p + 1);
    }
    return NULL;
}
static void *fc_executor(void *arg) {
    long k = (long)arg;
    uint64_t rs = xseed(300 + k);
    long bad = 0;
    while (!load(&fc_exec_stop)) {
        int w = xnext(&rs) & 1, slot = xnext(&rs) % 64;
        bad += !fl_check(&fc_w[w], slot, fl_call(&fc_w[w], slot));
    }
    __atomic_add_fetch(&fc_exec_bad, bad, __ATOMIC_RELAXED);
    if (k & 1) pthread_exit(NULL);
    return NULL;
}
static int fc_child_main(int argc, char **argv) {
    if (argc < 7) return 254;
    int fd = atoi(argv[2]), c = atoi(argv[3]), p = atoi(argv[4]), i = atoi(argv[5]);
    uint32_t t = (uint32_t)strtoul(argv[6], NULL, 10);
    uint64_t rec = fc_record(c, p, i, t, 1);
    if (write(fd, &rec, 8) != 8) return 253;
    return fc_code(c, p, i);
}
static int t_forkcallers(void) {
    fc_k = 15 * scale;
    if (pipe(fc_rec)) { perror("pipe"); return 0; }
    fc_phasefn = jit_page(4096);
    fl_start(&fc_w[0], 0, 150);
    fl_start(&fc_w[1], 1, 150);
    pthread_t callers[2];
    for (long c = 0; c < 2; c++) pthread_create(&callers[c], NULL, fc_caller, (void *)c);
    long recBad = 0, recChildBad = 0, recCount[FC_PHASES] = {0};
    for (int p = 0; p < FC_PHASES; p++) {
        emit_tag(fc_phasefn, fc_phasetag(p));
        __builtin___clear_cache((char *)fc_phasefn, (char *)fc_phasefn + 12);
        int ne = 2 + (p & 1);
        pthread_t ex[3];
        __atomic_store_n(&fc_exec_stop, 0, __ATOMIC_RELEASE);
        for (long k = 0; k < ne; k++) pthread_create(&ex[k], NULL, fc_executor, (void *)(k + 3 * p));
        set_wake(&fc_go, p + 1);
        wait_until(&fc_done[0], p + 1);
        wait_until(&fc_done[1], p + 1);
        __atomic_store_n(&fc_exec_stop, 1, __ATOMIC_RELEASE);
        for (int k = 0; k < ne; k++) pthread_join(ex[k], NULL);
        uint8_t seen[2][256];
        memset(seen, 0, sizeof seen);
        for (int n = 0; n < 2 * fc_k; n++) {
            struct pollfd pf = {fc_rec[0], POLLIN, 0};
            int k;
            while ((k = poll(&pf, 1, 5000)) < 0 && errno == EINTR) {}
            if (k <= 0) break;
            uint64_t rec;
            if (read(fc_rec[0], &rec, 8) != 8) break;
            int c = rec & 15, rp = (rec >> 4) & 15, i = (rec >> 8) & 0x7fffff, ok = (rec >> 31) & 1;
            uint32_t t = rec >> 32;
            if (c > 1 || rp != p || i >= fc_k || i > 255 || t != fc_phasetag(p)) { recBad++; continue; }
            recChildBad += !ok;
            seen[c][i]++;
        }
        for (int c = 0; c < 2; c++)
            for (int i = 0; i < fc_k && i < 256; i++) recCount[p] += seen[c][i] == 1, recBad += seen[c][i] > 1;
    }
    for (int c = 0; c < 2; c++) pthread_join(callers[c], NULL);
    fl_finish(&fc_w[0]);
    fl_finish(&fc_w[1]);
    for (int p = 0; p < FC_PHASES; p++)
        printf("phase %d caller0 %d/%d/%d caller1 %d/%d/%d records %ld\n", p, fc_reaped[p][0], fc_ok[p][0], fc_k,
               fc_reaped[p][1], fc_ok[p][1], fc_k, recCount[p]);
    OUT("recordBad", "%ld", recBad);
    OUT("childJitBad", "%ld", recChildBad);
    OUT("executorBad", "%ld", fc_exec_bad);
    OUT("writerBad", "%ld", fc_w[0].bad + fc_w[1].bad);
    if (diag) fprintf(stderr, "writer flushes %ld %ld\n", fc_w[0].count, fc_w[1].count);
    return 1;
}

/* ==== 31. CloneStackReuse ==== */

#define CS_REGION (1UL << 20)
#define CS_MIN (128UL << 10)
enum { CS_RETURN, CS_EXIT, CS_CANCEL, CS_RAWTHREAD, CS_VMPROC, CS_VFORK, CS_KINDS };
static const char *cs_names[CS_KINDS] = {"return", "pthread_exit", "cancel", "clone_thread", "clone_vm", "clone_vfork"};
enum { BAD_COOKIE = 1, BAD_ARG = 2, BAD_MASK = 4, BAD_COMPUTE = 8, BAD_STACK = 16, BAD_TLS = 32, BAD_JIT = 64, BAD_EXIT = 128 };
static __thread uint64_t cs_cookie = 0x5eed5eedULL;
static struct fl cs_pub;
static int cs_sigs[8];
static uint64_t cs_tls[8][32] __attribute__((aligned(64)));
struct cs_arg {
    int round, kind, ready, ctid;
    uintptr_t lo, hi;
    uint64_t mask, compute, tls, kmask;
    uint32_t bad;
    long result;
};
static __attribute__((noinline)) uint64_t cs_compute(int r) {
    uint64_t x = (uint64_t)(r + 1) * 0x9E3779B97F4A7C15ULL | 1;
    for (int i = 0; i < 2000; i++) { x ^= x << 13; x ^= x >> 7; x ^= x << 17; }
    return x;
}
static uint64_t cs_maskbits(const uint64_t *kset) {
    uint64_t m = 0;
    for (int k = 0; k < 8; k++) m |= (uint64_t)((kset[0] >> (cs_sigs[k] - 1)) & 1) << k;
    return m;
}
static __attribute__((noinline)) uint32_t cs_common(struct cs_arg *a, uint64_t kmask) {
    uint32_t bad = 0;
    volatile int local = 0;
    if ((uintptr_t)&local < a->lo || (uintptr_t)&local >= a->hi) bad |= BAD_STACK;
    a->kmask = kmask;
    if (cs_maskbits(&kmask) != a->mask) bad |= BAD_MASK;
    if (cs_compute(a->round) != a->compute) bad |= BAD_COMPUTE;
    int slot = a->round % 64;
    if (!fl_check(&cs_pub, slot, fl_call(&cs_pub, slot))) bad |= BAD_JIT;
    return bad;
}
static __attribute__((noinline)) void cs_exit_deep(int depth, long v) {
    if (depth) cs_exit_deep(depth - 1, v);
    pthread_exit((void *)v);
}
static void *cs_thread(void *p) {
    struct cs_arg *a = p;
    uint32_t bad = 0;
    if (a->round < 0 || a->kind > CS_CANCEL) bad |= BAD_ARG;
    if (cs_cookie != 0x5eed5eedULL) bad |= BAD_COOKIE;
    cs_cookie = (uint64_t)a->round * 0x1000193 + 7;
    if ((uintptr_t)&cs_cookie < a->lo || (uintptr_t)&cs_cookie >= a->hi) bad |= BAD_TLS;
    sigset_t cur;
    pthread_sigmask(SIG_BLOCK, NULL, &cur);
    uint64_t k = 0;
    for (int s = 1; s < 64; s++) if (sigismember(&cur, s)) k |= 1ULL << (s - 1);
    bad |= cs_common(a, k);
    if (cs_cookie != (uint64_t)a->round * 0x1000193 + 7) bad |= BAD_COOKIE;
    a->bad = bad;
    long v = a->round ^ 0xabc;
    if (a->kind == CS_EXIT) cs_exit_deep(a->round % 5, v);
    if (a->kind == CS_CANCEL) {
        set_wake(&a->ready, 1);
        for (;;) pause();
    }
    return (void *)v;
}
/* Raw clone children: no libc, no TLS. */
static long cs_raw_child(void *p) {
    struct cs_arg *a = p;
    uint64_t tp, kmask = 0;
    asm volatile("mrs %0, tpidr_el0" : "=r"(tp));
    {
        register long x0 asm("x0") = SIG_BLOCK;
        register long x1 asm("x1") = 0;
        register long x2 asm("x2") = (long)&kmask;
        register long x3 asm("x3") = 8;
        register long x8 asm("x8") = SYS_rt_sigprocmask;
        asm volatile("svc #0" : "+r"(x0) : "r"(x1), "r"(x2), "r"(x3), "r"(x8) : "memory");
    }
    uint32_t bad = cs_common(a, kmask);
    if (tp != a->tls) bad |= BAD_TLS;
    a->bad = bad;
    a->result = a->round ^ 0xabc;
    return (a->round & 0x3f) + 1;
}
static long raw_clone(unsigned long flags, void *stack, int *ptid, unsigned long tls, int *ctid, long (*fn)(void *), void *arg) {
    register unsigned long x0 asm("x0") = flags;
    register void *x1 asm("x1") = stack;
    register int *x2 asm("x2") = ptid;
    register unsigned long x3 asm("x3") = tls;
    register int *x4 asm("x4") = ctid;
    register long (*x5)(void *) asm("x5") = fn;
    register void *x6 asm("x6") = arg;
    register long x8 asm("x8") = SYS_clone;
    asm volatile("svc #0\n"
                 "cbnz x0, 1f\n"
                 "mov x29, xzr\n"
                 "mov x30, xzr\n"
                 "mov x0, x6\n"
                 "blr x5\n"
                 "mov x8, #93\n" /* exit */
                 "svc #0\n"
                 "brk #0\n"
                 "1:"
                 : "+r"(x0)
                 : "r"(x1), "r"(x2), "r"(x3), "r"(x4), "r"(x5), "r"(x6), "r"(x8)
                 : "memory", "cc");
    return (long)x0;
}
static int t_clonestack(void) {
    for (int k = 0; k < 4; k++) cs_sigs[k] = (int[]){SIGUSR1, SIGUSR2, SIGWINCH, SIGURG}[k];
    for (int k = 0; k < 4; k++) cs_sigs[4 + k] = SIGRTMIN + 3 + k;
    uint8_t *region = mmap(NULL, CS_REGION + 65536, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (region == MAP_FAILED) { perror("mmap"); return 0; }
    mprotect(region + CS_REGION, 65536, PROT_NONE);
    fl_start(&cs_pub, 1, 200);
    long rounds = 2000 * scale, ok[CS_KINDS] = {0}, cnt[CS_KINDS] = {0}, badBits[8] = {0};
    sigset_t old;
    pthread_sigmask(SIG_SETMASK, NULL, &old);
    uint64_t parent_tp;
    asm volatile("mrs %0, tpidr_el0" : "=r"(parent_tp));
    for (long r = 0; r < rounds; r++) {
        int kind = next() % CS_KINDS;
        if (!(narrow >> kind & 1)) kind = CS_RETURN;
        size_t size = CS_MIN + (next() % 57) * (16UL << 10);
        uint8_t *base = (next() & 1) ? region + CS_REGION - size : region;
        uint64_t mbits = next() & 0xff;
        sigset_t m = old;
        for (int k = 0; k < 8; k++) if (mbits >> k & 1) sigaddset(&m, cs_sigs[k]);
        struct cs_arg a;
        memset(&a, 0, sizeof a);
        a.round = (int)r;
        a.kind = kind;
        a.lo = (uintptr_t)base;
        a.hi = (uintptr_t)base + size;
        a.mask = mbits;
        a.compute = cs_compute((int)r);
        a.tls = kind == CS_VFORK ? parent_tp : (uintptr_t)&cs_tls[r % 8][16]; /* room on both sides */
        uint32_t bad = 0;
        cnt[kind]++;
        pthread_sigmask(SIG_SETMASK, &m, NULL);
        if (kind <= CS_CANCEL) {
            pthread_attr_t at;
            pthread_attr_init(&at);
            pthread_attr_setstack(&at, base, size);
            pthread_t t;
            int rc = pthread_create(&t, &at, cs_thread, &a);
            pthread_attr_destroy(&at);
            pthread_sigmask(SIG_SETMASK, &old, NULL);
            if (rc) { fprintf(stderr, "round %ld pthread_create: %s\n", r, strerror(rc)); bad |= BAD_EXIT; }
            else {
                if (kind == CS_CANCEL) { wait_until(&a.ready, 1); pthread_cancel(t); }
                void *ret = NULL;
                pthread_join(t, &ret);
                if (kind == CS_CANCEL ? ret != PTHREAD_CANCELED : ret != (void *)(long)(r ^ 0xabc)) bad |= BAD_EXIT;
                bad |= a.bad;
            }
        } else {
            void *top = (void *)(((uintptr_t)base + size) & ~15UL);
            long rc;
            int st = 0;
            if (kind == CS_RAWTHREAD) {
                unsigned long fl = CLONE_VM | CLONE_FS | CLONE_FILES | CLONE_SIGHAND | CLONE_THREAD | CLONE_SYSVSEM |
                                   CLONE_SETTLS | CLONE_PARENT_SETTID | CLONE_CHILD_CLEARTID;
                rc = raw_clone(fl, top, &a.ctid, a.tls, &a.ctid, cs_raw_child, &a);
                pthread_sigmask(SIG_SETMASK, &old, NULL);
                if (rc > 0) {
                    for (;;) {
                        int v = __atomic_load_n(&a.ctid, __ATOMIC_ACQUIRE);
                        if (!v) break;
                        syscall(SYS_futex, &a.ctid, FUTEX_WAIT, v, NULL, NULL, 0);
                    }
                }
            } else {
                unsigned long fl = CLONE_VM | SIGCHLD | (kind == CS_VFORK ? CLONE_VFORK : CLONE_SETTLS);
                rc = raw_clone(fl, top, NULL, a.tls, NULL, cs_raw_child, &a);
                pthread_sigmask(SIG_SETMASK, &old, NULL);
                if (rc > 0) {
                    if (waitpid((pid_t)rc, &st, 0) != rc || !WIFEXITED(st) || WEXITSTATUS(st) != (r & 0x3f) + 1) bad |= BAD_EXIT;
                }
            }
            if (rc <= 0) { fprintf(stderr, "round %ld %s: clone %ld errno %s\n", r, cs_names[kind], rc, strerror(errno)); bad |= BAD_EXIT; }
            else {
                if (a.result != (r ^ 0xabc)) bad |= BAD_EXIT;
                bad |= a.bad;
            }
        }
        if (bad && badBits[7] + badBits[2] < 5)
            fprintf(stderr, "round %ld %s size %zu base +%zu: bad %#x mask want %#lx kernel set %#lx\n", r, cs_names[kind], size,
                    (size_t)(base - region), bad, (unsigned long)mbits, (unsigned long)a.kmask);
        if (!(narrow >> 8 & 1)) bad &= ~(uint32_t)BAD_MASK;
        for (int b = 0; b < 8; b++) badBits[b] += bad >> b & 1;
        ok[kind] += !bad;
    }
    fl_finish(&cs_pub);
    for (int k = 0; k < CS_KINDS; k++) printf("kind %s rounds %ld ok %ld\n", cs_names[k], cnt[k], ok[k]);
    OUT("bad", "cookie %ld arg %ld mask %ld compute %ld stack %ld tls %ld jit %ld exit %ld", badBits[0], badBits[1], badBits[2],
        badBits[3], badBits[4], badBits[5], badBits[6], badBits[7]);
    OUT("publisherBad", "%ld", cs_pub.bad);
    return 1;
}

/* ==== 27. FlushMatrix ==== */

enum {
    FM_FUTEX, FM_NANOSLEEP, FM_READ, FM_ACCEPT, FM_EPOLL, FM_WAIT4, FM_SIGSUSPEND, FM_PAUSE, FM_PPOLL, FM_RECV,
    FM_VFORK, FM_JOIN, FM_SIGTIMEDWAIT, FM_IOURING, FM_GATED, /* rows below run work instead of blocking */
    FM_CLONE = FM_GATED, FM_PAGEFAULT, FM_SPINBLR, FM_SPINJIT, FM_CHARGE, FM_N
};
static const char *fm_names[FM_N] = {"futex", "nanosleep", "read", "accept", "epoll_wait", "wait4", "sigsuspend", "pause",
                                     "ppoll", "recvmsg", "vfork_parent", "pthread_join", "sigtimedwait", "io_uring_enter",
                                     "clone", "pagefault", "spin_blr", "spin_jit", "charge"};
/* The syscall each gated row blocks in (AArch64 numbers); pause is ppoll or rt_sigsuspend. */
static const int fm_nr[FM_GATED][2] = {{98, 98}, {101, 101}, {63, 63}, {202, 202}, {22, 22}, {260, 260}, {133, 133},
                                       {73, 133}, {73, 73}, {212, 212}, {220, 220}, {98, 98}, {137, 137}, {426, 426}};
static struct fm_row {
    int go, ann, done, word, fds[2];
    pid_t tid;
    long sum, eintr, resends, proof[3];
    double maxwait;
    pthread_t t;
    int pad[8];
} fm[FM_N];
static int fm_stop, fm_rounds, fm_lsock, fm_usr1, fm_usr2;
#define FM_ON(row) ((narrow >> (row)) & 1)
static struct sockaddr_un fm_addr;
static char fm_exit_arg[128][4], *fm_exit_av[128][4];
static socklen_t fm_addrlen;
static struct fl fm_fl;
static long fm_jitbad;
static uint8_t *fm_map;
static int fm_mapfd;
#define FM_MAP (32UL << 20)
static struct {
    int fd;
    char *sq, *cq;
    struct io_uring_sqe *sqes;
    struct io_uring_params p;
} fm_ring;
static void fm_on_usr1(int s) { (void)s; __atomic_add_fetch(&fm_usr1, 1, __ATOMIC_RELAXED); }
static void fm_on_usr2(int s) { (void)s; __atomic_add_fetch(&fm_usr2, 1, __ATOMIC_RELAXED); }

static uint64_t fm_read8(struct fm_row *f, int fd) {
    uint64_t v = 0;
    size_t got = 0;
    while (got < 8) {
        ssize_t n = read(fd, (char *)&v + got, 8 - got);
        if (n < 0 && errno == EINTR) { f->eintr++; continue; }
        if (n <= 0) break;
        got += n;
    }
    return v;
}
static void *fm_join_helper(void *arg) {
    struct fm_row *f = arg;
    int r = load(&f->ann);
    wait_until(&f->word, r);
    return (void *)(long)r;
}
static void *fm_clone_child(void *arg) { return (void *)((long)arg * 3); }
static __attribute__((noinline)) long fm_f0(long x) { return x + 1; }
static __attribute__((noinline)) long fm_f1(long x) { return x ^ 0x55; }
static __attribute__((noinline)) long fm_f2(long x) { return x * 3; }
static __attribute__((noinline)) long fm_f3(long x) { return x - 7; }
static long (*volatile fm_fns[4])(long) = {fm_f0, fm_f1, fm_f2, fm_f3};

static uint64_t fm_page_value(size_t page) { return (uint64_t)page * 0x9E3779B97F4A7C15ULL ^ (uint64_t)seed; }

static void fm_uring_read(struct fm_row *f, int r) {
    uint32_t mask = *(uint32_t *)(fm_ring.sq + fm_ring.p.sq_off.ring_mask), cmask = *(uint32_t *)(fm_ring.cq + fm_ring.p.cq_off.ring_mask);
    uint32_t tail = __atomic_load_n((uint32_t *)(fm_ring.sq + fm_ring.p.sq_off.tail), __ATOMIC_ACQUIRE), idx = tail & mask;
    static uint64_t buf;
    buf = 0;
    struct io_uring_sqe *e = &fm_ring.sqes[idx];
    memset(e, 0, sizeof *e);
    e->opcode = IORING_OP_READ;
    e->fd = f->fds[0];
    e->addr = (uintptr_t)&buf;
    e->len = 8;
    e->off = (uint64_t)-1;
    e->user_data = (uint64_t)r;
    ((uint32_t *)(fm_ring.sq + fm_ring.p.sq_off.array))[idx] = idx;
    __atomic_store_n((uint32_t *)(fm_ring.sq + fm_ring.p.sq_off.tail), tail + 1, __ATOMIC_RELEASE);
    int submit = 1;
    for (;;) {
        uint32_t head = __atomic_load_n((uint32_t *)(fm_ring.cq + fm_ring.p.cq_off.head), __ATOMIC_ACQUIRE);
        uint32_t ctail = __atomic_load_n((uint32_t *)(fm_ring.cq + fm_ring.p.cq_off.tail), __ATOMIC_ACQUIRE);
        if (head != ctail) {
            struct io_uring_cqe *c = (struct io_uring_cqe *)(fm_ring.cq + fm_ring.p.cq_off.cqes) + (head & cmask);
            if (c->user_data == (uint64_t)r && c->res == 8) f->sum += buf;
            __atomic_store_n((uint32_t *)(fm_ring.cq + fm_ring.p.cq_off.head), head + 1, __ATOMIC_RELEASE);
            return;
        }
        long rc = syscall(426, fm_ring.fd, submit, 1, IORING_ENTER_GETEVENTS, NULL, 0);
        if (rc < 0 && errno == EINTR) { f->eintr++; continue; }
        if (rc < 0) { fprintf(stderr, "io_uring_enter: %s\n", strerror(errno)); return; }
        submit = 0;
    }
}

static void fm_block(int row, int r) {
    struct fm_row *f = &fm[row];
    switch (row) {
    case FM_FUTEX:
        for (;;) {
            int v = load(&f->word);
            if (v >= r) break;
            if (futex(&f->word, FUTEX_WAIT_PRIVATE, v) < 0 && errno == EINTR) f->eintr++;
        }
        f->sum += load(&f->word);
        break;
    case FM_NANOSLEEP: {
        struct timespec t = {60, 0};
        long rc = syscall(SYS_nanosleep, &t, &t);
        f->sum += rc < 0 && errno == EINTR;
        break;
    }
    case FM_READ:
        f->sum += fm_read8(f, f->fds[0]);
        break;
    case FM_ACCEPT: {
        int c;
        while ((c = (int)syscall(SYS_accept, fm_lsock, NULL, NULL)) < 0 && errno == EINTR) f->eintr++;
        if (c < 0) { fprintf(stderr, "accept: %s\n", strerror(errno)); break; }
        f->sum += fm_read8(f, c);
        close(c);
        break;
    }
    case FM_EPOLL: {
        struct epoll_event ev;
        int n;
        while ((n = epoll_wait(f->fds[1], &ev, 1, -1)) < 0 && errno == EINTR) f->eintr++;
        f->sum += fm_read8(f, f->fds[0]);
        break;
    }
    case FM_WAIT4: {
        pid_t c = fork();
        if (c == 0) {
            uint8_t b = 0;
            while (read(f->fds[0], &b, 1) < 0 && errno == EINTR) {}
            _exit(b);
        }
        int st = 0;
        pid_t w;
        while ((w = wait4(c, &st, 0, NULL)) < 0 && errno == EINTR) f->eintr++;
        if (w == c && WIFEXITED(st)) f->sum += WEXITSTATUS(st);
        break;
    }
    case FM_SIGSUSPEND: {
        sigset_t m;
        pthread_sigmask(SIG_BLOCK, NULL, &m);
        sigdelset(&m, SIGUSR2);
        f->sum += sigsuspend(&m) < 0 && errno == EINTR;
        break;
    }
    case FM_PAUSE:
        f->sum += pause() < 0 && errno == EINTR;
        break;
    case FM_PPOLL: {
        struct pollfd pf = {f->fds[0], POLLIN, 0};
        while (ppoll(&pf, 1, NULL, NULL) < 0 && errno == EINTR) f->eintr++;
        f->sum += fm_read8(f, f->fds[0]);
        break;
    }
    case FM_RECV: {
        uint64_t v = 0;
        struct iovec iov = {&v, 8};
        struct msghdr mh = {0};
        mh.msg_iov = &iov;
        mh.msg_iovlen = 1;
        ssize_t n;
        while ((n = recvmsg(f->fds[0], &mh, 0)) < 0 && errno == EINTR) f->eintr++;
        if (n == 8) f->sum += v;
        break;
    }
    case FM_VFORK: {
        /* The child keeps the parent blocked in vfork until the byte arrives, then execs a
         * helper that exits with it (vfork children that exit without exec: clonestack). */
        pid_t c = vfork();
        if (c == 0) {
            uint8_t b = 0;
            while (syscall(SYS_read, f->fds[0], &b, 1) < 0) {}
            execve(self_path, fm_exit_av[b & 0x7f], environ);
            _exit(126);
        }
        int st = 0;
        if (c > 0 && waitpid(c, &st, 0) == c && WIFEXITED(st)) f->sum += WEXITSTATUS(st);
        break;
    }
    case FM_JOIN: {
        pthread_t h;
        void *ret = NULL;
        if (pthread_create(&h, NULL, fm_join_helper, f)) break;
        pthread_join(h, &ret);
        f->sum += (long)ret;
        break;
    }
    case FM_SIGTIMEDWAIT: {
        sigset_t s;
        sigemptyset(&s);
        sigaddset(&s, SIGRTMIN + 5);
        siginfo_t si;
        struct timespec t = {60, 0};
        int n;
        while ((n = sigtimedwait(&s, &si, &t)) < 0 && errno == EINTR) f->eintr++;
        if (n == SIGRTMIN + 5) f->sum += si.si_value.sival_int;
        break;
    }
    case FM_IOURING:
        fm_uring_read(f, r);
        break;
    case FM_CLONE:
        for (long k = 0; k < 3; k++) {
            pthread_t t;
            void *ret = NULL;
            if (pthread_create(&t, NULL, fm_clone_child, (void *)(r * 3 + k))) continue;
            pthread_join(t, &ret);
            f->sum += (long)ret;
        }
        break;
    case FM_PAGEFAULT: {
        size_t off = (size_t)(r % 32) << 20;
        posix_fadvise(fm_mapfd, off, 1 << 20, POSIX_FADV_DONTNEED);
        madvise(fm_map + off, 1 << 20, MADV_DONTNEED);
        for (size_t pg = 0; pg < 256; pg++) f->sum += *(volatile uint64_t *)(fm_map + off + pg * 4096) & 0xffff;
        break;
    }
    case FM_SPINBLR:
        for (long i = 0; i < 100000; i++) f->sum += fm_fns[i & 3](i) & 0xff;
        break;
    case FM_SPINJIT:
        for (long i = 0; i < 50000; i++) {
            int slot = (int)((i * 7 + r) % 64);
            uint32_t v = fl_call(&fm_fl, slot);
            if (!fl_check(&fm_fl, slot, v)) fm_jitbad++;
            f->sum += v & 63;
        }
        break;
    case FM_CHARGE:
        /* Each iteration is about 60000 instructions: more than one progress budget. */
        for (int i = 0; i < 20; i++) {
            unsigned long acc = 0;
            asm volatile("mov x9, #20000\n1: add %0, %0, x9\nsubs x9, x9, #1\nb.ne 1b" : "+r"(acc)::"x9", "cc");
            f->sum += acc & 0xffff;
        }
        break;
    }
}
static void fm_release(int row, int r) {
    struct fm_row *f = &fm[row];
    uint64_t v = (uint64_t)r;
    switch (row) {
    case FM_FUTEX:
    case FM_JOIN:
        set_wake(&f->word, r);
        break;
    case FM_NANOSLEEP:
    case FM_PAUSE:
        syscall(SYS_tgkill, getpid(), f->tid, SIGUSR1);
        break;
    case FM_SIGSUSPEND:
        syscall(SYS_tgkill, getpid(), f->tid, SIGUSR2);
        break;
    case FM_READ:
    case FM_PPOLL:
    case FM_IOURING:
        if (write(f->fds[1], &v, 8) != 8) perror("write");
        break;
    case FM_EPOLL:
        if (write(f->fds[0], &v, 8) != 8) perror("eventfd");
        break;
    case FM_RECV:
        if (send(f->fds[1], &v, 8, 0) != 8) perror("send");
        break;
    case FM_ACCEPT: {
        int s = socket(AF_UNIX, SOCK_STREAM, 0);
        if (s < 0 || connect(s, (struct sockaddr *)&fm_addr, fm_addrlen) || write(s, &v, 8) != 8) perror("connect");
        close(s);
        break;
    }
    case FM_WAIT4:
    case FM_VFORK: {
        uint8_t b = (uint8_t)(r & 0x7f);
        if (write(f->fds[1], &b, 1) != 1) perror("write");
        break;
    }
    case FM_SIGTIMEDWAIT: {
        siginfo_t si;
        memset(&si, 0, sizeof si);
        si.si_signo = SIGRTMIN + 5;
        si.si_code = SI_QUEUE;
        si.si_pid = getpid();
        si.si_uid = getuid();
        si.si_value.sival_int = r;
        syscall(SYS_rt_tgsigqueueinfo, getpid(), f->tid, SIGRTMIN + 5, &si);
        break;
    }
    }
}
static void *fm_thread(void *arg) {
    int row = (int)(long)arg;
    struct fm_row *f = &fm[row];
    sigset_t m;
    sigemptyset(&m);
    if (row == FM_SIGSUSPEND) sigaddset(&m, SIGUSR2);
    if (row == FM_SIGTIMEDWAIT) sigaddset(&m, SIGRTMIN + 5);
    pthread_sigmask(SIG_BLOCK, &m, NULL);
    f->tid = tid();
    set_wake(&f->ann, 0);
    for (int r = 1;; r++) {
        wait_until(&f->go, r);
        if (load(&fm_stop)) break;
        __atomic_store_n(&f->ann, r, __ATOMIC_RELEASE);
        fm_block(row, r);
        __atomic_store_n(&f->done, r, __ATOMIC_RELEASE);
    }
    return NULL;
}
/* 1 when the row's thread is in its blocking syscall, 2 when only its state says it sleeps. */
static int fm_blocked(int row) {
    int nr = task_syscall(fm[row].tid);
    if (nr == -2) return task_state(fm[row].tid) == 'S' ? 2 : 0;
    return nr == fm_nr[row][0] || nr == fm_nr[row][1];
}

/* Destructive rows: each runs in a forked child with its own flusher. */
static void *fd_spin(void *arg) {
    volatile long x = (long)arg;
    for (;;) x = fm_fns[x & 3](x);
    return NULL;
}
static void *fd_block(void *arg) {
    int w = 0;
    (void)arg;
    for (;;) futex(&w, FUTEX_WAIT_PRIVATE, 0);
    return NULL;
}
static void *fd_exit_thread(void *arg) {
    long k = (long)arg;
    if (k % 3 == 1) cs_exit_deep(2, k);
    if (k % 3 == 2) {
        pthread_setcanceltype(PTHREAD_CANCEL_ASYNCHRONOUS, NULL);
        for (volatile long x = 0;; x++) {}
    }
    return (void *)k;
}
static void *fd_exiter(void *arg) {
    sleep_us(2000);
    _exit((int)(long)arg);
}
static void *fd_execer(void *arg) {
    (void)arg;
    sleep_us(2000);
    char *av[] = {(char *)self_path, "child42", NULL};
    execve(self_path, av, environ);
    _exit(126);
}
/* Child phases. Returns the exit status the child should report. */
static int fd_phase(int phase, int variant) {
    struct fl f;
    fl_start(&f, 0, 1000);
    pthread_t t[4];
    for (long k = 0; k < 2; k++) pthread_create(&t[k], NULL, fd_spin, (void *)k);
    pthread_create(&t[2], NULL, fd_block, NULL);
    if (phase == 0) {
        /* Thread exit: return, pthread_exit and asynchronous cancellation of a spinning thread. */
        int bad = 0;
        for (long k = 0; k < 60; k++) {
            pthread_t e;
            if (pthread_create(&e, NULL, fd_exit_thread, (void *)k)) { bad++; continue; }
            if (k % 3 == 2) { sleep_us(300); pthread_cancel(e); }
            void *ret = NULL;
            pthread_join(e, &ret);
            bad += k % 3 == 2 ? ret != PTHREAD_CANCELED : ret != (void *)k;
        }
        bad += (int)f.bad;
        _exit(bad);
    }
    if (phase == 1) {
        /* Process exit from the main thread or another thread during flushes. */
        if (variant & 1) {
            pthread_create(&t[3], NULL, fd_exiter, (void *)18);
            for (;;) pause();
        }
        sleep_us(2000);
        exit(17);
    }
    /* execve from the main thread or another thread during flushes. */
    if (variant & 1) {
        pthread_create(&t[3], NULL, fd_execer, NULL);
        for (;;) pause();
    }
    fd_execer(NULL);
    return 0;
}

static int t_flushmatrix(void) {
    memset(&fm_ring.p, 0, sizeof fm_ring.p);
    fm_ring.fd = (int)syscall(425, 8, &fm_ring.p);
    if (fm_ring.fd < 0) { OUT("UNSUPPORTED", "io_uring %s", strerror(errno)); return 0; }
    fm_ring.sq = mmap(0, fm_ring.p.sq_off.array + 4 * fm_ring.p.sq_entries, PROT_READ | PROT_WRITE, MAP_SHARED, fm_ring.fd, IORING_OFF_SQ_RING);
    fm_ring.cq = mmap(0, fm_ring.p.cq_off.cqes + sizeof(struct io_uring_cqe) * fm_ring.p.cq_entries, PROT_READ | PROT_WRITE, MAP_SHARED, fm_ring.fd, IORING_OFF_CQ_RING);
    fm_ring.sqes = mmap(0, sizeof(struct io_uring_sqe) * fm_ring.p.sq_entries, PROT_READ | PROT_WRITE, MAP_SHARED, fm_ring.fd, IORING_OFF_SQES);
    struct sigaction sa;
    memset(&sa, 0, sizeof sa);
    sa.sa_handler = fm_on_usr1;
    sigaction(SIGUSR1, &sa, NULL);
    sa.sa_handler = fm_on_usr2;
    sigaction(SIGUSR2, &sa, NULL);
    for (int k = 0; k < 128; k++) {
        snprintf(fm_exit_arg[k], sizeof fm_exit_arg[k], "%d", k);
        fm_exit_av[k][0] = (char *)self_path;
        fm_exit_av[k][1] = "exitwith";
        fm_exit_av[k][2] = fm_exit_arg[k];
        fm_exit_av[k][3] = NULL;
    }
    /* Endpoints. */
    for (int row = 0; row < FM_N; row++) fm[row].fds[0] = fm[row].fds[1] = -1;
    int pipes[] = {FM_READ, FM_WAIT4, FM_PPOLL, FM_VFORK, FM_IOURING};
    for (size_t k = 0; k < sizeof pipes / sizeof pipes[0]; k++)
        if (pipe(fm[pipes[k]].fds)) { perror("pipe"); return 0; }
    if (socketpair(AF_UNIX, SOCK_STREAM, 0, fm[FM_RECV].fds)) { perror("socketpair"); return 0; }
    fm[FM_EPOLL].fds[0] = eventfd(0, 0);
    fm[FM_EPOLL].fds[1] = epoll_create1(0);
    struct epoll_event ev = {EPOLLIN, {0}};
    epoll_ctl(fm[FM_EPOLL].fds[1], EPOLL_CTL_ADD, fm[FM_EPOLL].fds[0], &ev);
    fm_lsock = socket(AF_UNIX, SOCK_STREAM, 0);
    memset(&fm_addr, 0, sizeof fm_addr);
    fm_addr.sun_family = AF_UNIX;
    int nl = snprintf(fm_addr.sun_path + 1, sizeof fm_addr.sun_path - 1, "cfzsync-fm-%d", (int)getpid());
    fm_addrlen = (socklen_t)(offsetof(struct sockaddr_un, sun_path) + 1 + nl);
    if (bind(fm_lsock, (struct sockaddr *)&fm_addr, fm_addrlen) || listen(fm_lsock, 8)) { perror("listen"); return 0; }
    /* A large file mapping for the page-fault row. */
    char path[] = "/tmp/cfzsync-XXXXXX";
    fm_mapfd = mkstemp(path);
    if (fm_mapfd < 0) { perror("mkstemp"); return 0; }
    unlink(path);
    uint64_t *page = calloc(512, 8);
    for (size_t pg = 0; pg < FM_MAP / 4096; pg++) {
        page[0] = fm_page_value(pg);
        if (write(fm_mapfd, page, 4096) != 4096) { perror("write"); return 0; }
    }
    free(page);
    fdatasync(fm_mapfd);
    fm_map = mmap(NULL, FM_MAP, PROT_READ, MAP_SHARED, fm_mapfd, 0);
    if (fm_map == MAP_FAILED) { perror("mmap file"); return 0; }

    fl_start(&fm_fl, 1, 1000);
    for (long row = 0; row < FM_N; row++) {
        if (!FM_ON(row)) continue;
        fm[row].ann = -1;
        pthread_create(&fm[row].t, NULL, fm_thread, (void *)row);
    }
    for (int row = 0; row < FM_N; row++) if (FM_ON(row)) wait_until(&fm[row].ann, 0);
    fm_rounds = 100 * (int)scale;
    double start = now();
    for (int r = 1; r <= fm_rounds; r++) {
        for (int row = 0; row < FM_N; row++) if (FM_ON(row)) set_wake(&fm[row].go, r);
        double t0 = now();
        for (int row = 0; row < FM_GATED; row++) {
            if (!FM_ON(row)) continue;
            for (;;) {
                int how = load(&fm[row].ann) == r ? fm_blocked(row) : 0;
                if (how) { fm[row].proof[how - 1]++; break; }
                if (load(&fm[row].done) >= r) { fm[row].proof[2]++; break; } /* released early by a stray signal */
                if (now() - t0 > 30) stuck(fm_names[row], r);
                sleep_us(100);
            }
        }
        sleep_us(3000); /* about three flushes while every row is blocked */
        int order[FM_GATED];
        for (int k = 0; k < FM_GATED; k++) order[k] = k;
        for (int k = FM_GATED - 1; k > 0; k--) {
            int j = (int)(next() % (uint64_t)(k + 1)), t = order[k];
            order[k] = order[j];
            order[j] = t;
        }
        for (int k = 0; k < FM_GATED; k++) {
            if (!FM_ON(order[k])) continue;
            fm_release(order[k], r);
            if (next() % 4 == 0) sleep_us(200); /* let some rows resume while others are still blocked */
        }
        for (int row = 0; row < FM_N; row++) {
            if (!FM_ON(row)) continue;
            double last = now();
            while (load(&fm[row].done) < r) {
                double n = now();
                if ((row == FM_NANOSLEEP || row == FM_PAUSE) && n - last > 0.02) {
                    /* The signal arrived before the thread entered the call; send another. */
                    fm[row].resends++;
                    fm_release(row, r);
                    last = n;
                }
                if (n - t0 > 30) stuck(fm_names[row], r);
                sleep_us(100);
            }
            if (now() - t0 > fm[row].maxwait) fm[row].maxwait = now() - t0;
        }
    }
    double elapsed = now() - start;
    __atomic_store_n(&fm_stop, 1, __ATOMIC_RELEASE);
    for (int row = 0; row < FM_N; row++) if (FM_ON(row)) set_wake(&fm[row].go, fm_rounds + 1);
    for (int row = 0; row < FM_N; row++) if (FM_ON(row)) pthread_join(fm[row].t, NULL);
    fl_finish(&fm_fl);
    long eintr = 0;
    for (int row = 0; row < FM_N; row++) {
        if (!FM_ON(row)) continue;
        printf("row %s rounds %d sum %ld\n", fm_names[row], load(&fm[row].done), fm[row].sum);
        eintr += fm[row].eintr;
        if (diag) fprintf(stderr, "row %s resends %ld eintr %ld proof syscall %ld state %ld early %ld maxround %.3fs\n", fm_names[row],
                fm[row].resends, fm[row].eintr, fm[row].proof[0], fm[row].proof[1], fm[row].proof[2], fm[row].maxwait);
    }
    if (diag) fprintf(stderr, "matrix %.2fs flushes %ld usr1 %d\n", elapsed, fm_fl.count, fm_usr1);
    OUT("sigsuspendSignals", "%d", fm_usr2);
    OUT("spuriousEintr", "%ld", eintr);
    OUT("flusherStale", "%ld", fm_fl.bad);
    OUT("jitBad", "%ld", fm_jitbad);

    /* Destructive rows in isolated child processes. */
    static const struct { const char *name; int phase, variants, want[2]; } dp[] = {
        {"threadexit", 0, 2, {0, 0}}, {"procexit", 1, 8, {17, 18}}, {"exec", 2, 6, {42, 42}}};
    fflush(stdout);
    for (size_t k = 0; k < sizeof dp / sizeof dp[0]; k++) {
        if (!FM_ON(FM_N + k)) continue;
        int ok = 0;
        for (int v = 0; v < dp[k].variants; v++) {
            pid_t c = fork();
            if (c == 0) _exit(fd_phase(dp[k].phase, v));
            int st = 0;
            double t0 = now();
            pid_t w;
            while ((w = waitpid(c, &st, WNOHANG)) == 0) {
                if (now() - t0 > 30) { kill(c, SIGKILL); stuck(dp[k].name, v); }
                sleep_us(1000);
            }
            int good = w == c && WIFEXITED(st) && WEXITSTATUS(st) == dp[k].want[v & 1];
            if (!good) fprintf(stderr, "%s variant %d: status %#x\n", dp[k].name, v, st);
            ok += good;
        }
        printf("destructive %s %d/%d\n", dp[k].name, ok, dp[k].variants);
    }
    return 1;
}

static const struct { const char *name; int (*fn)(void); } TESTS[] = {
    {"flushmatrix", t_flushmatrix}, {"syscallivau", t_syscallivau}, {"forkcallers", t_forkcallers},
    {"crossivau", t_crossivau},     {"clonestack", t_clonestack},
};

int main(int argc, char **argv) {
    if (argc > 1 && !strcmp(argv[1], "child42")) return 42;
    if (argc > 2 && !strcmp(argv[1], "exitwith")) return atoi(argv[2]);
    if (argc > 1 && !strcmp(argv[1], "fcchild")) return fc_child_main(argc, argv);
    static char exe[PATH_MAX];
    ssize_t n = readlink("/proc/self/exe", exe, sizeof exe - 1);
    self_path = strchr(argv[0], '/') ? argv[0] : n > 0 ? (exe[n] = 0, exe) : argv[0];
    if (argc < 2) {
        for (size_t i = 0; i < sizeof TESTS / sizeof TESTS[0]; i++) puts(TESTS[i].name);
        return 0;
    }
    diag = getenv("CFZ_DIAG") != NULL;
    seed = argc > 2 ? atol(argv[2]) : 1;
    scale = argc > 3 ? atol(argv[3]) : 1;
    if (argc > 4) narrow = strtoul(argv[4], NULL, 16);
    if (scale < 1) scale = 1;
    rng = (uint64_t)seed * 0x9E3779B97F4A7C15ULL | 1;
    setvbuf(stdout, NULL, _IOLBF, 0);
    cache_lines();
    for (size_t i = 0; i < sizeof TESTS / sizeof TESTS[0]; i++) {
        if (strcmp(argv[1], TESTS[i].name)) continue;
        test_name = TESTS[i].name;
        pthread_t w;
        pthread_create(&w, NULL, watchdog, (void *)(600L * scale));
        pthread_detach(w);
        if (TESTS[i].fn()) printf("OK %s\n", argv[1]);
        return 0;
    }
    fprintf(stderr, "unknown test %s\n", argv[1]);
    return 2;
}
