/* Source-derived process cases: Seedbox f32472b7fe470367b7eddecde1ac53ad299f3a3e
 * tools/jvm-fuzz/probes/c/cfzproc.c. Retains original counts and exec images.
 * Unrelated mapping/cache-range selectors and original timed watchdog excluded. */
#define _GNU_SOURCE
#include <dirent.h>
#include <dlfcn.h>
#include <errno.h>
#include <fcntl.h>
#include <grp.h>
#include <limits.h>
#include <linux/futex.h>
#include <poll.h>
#include <pthread.h>
#include <sched.h>
#include <signal.h>
#include <spawn.h>
#include <stdarg.h>
#include <stddef.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/resource.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#ifndef CLONE_CLEAR_SIGHAND
#define CLONE_CLEAR_SIGHAND 0x100000000ULL
#endif
#ifndef CLONE_INTO_CGROUP
#define CLONE_INTO_CGROUP 0x200000000ULL
#endif
#ifndef CLOSE_RANGE_CLOEXEC
#define CLOSE_RANGE_CLOEXEC (1U << 2)
#endif
#ifndef SYS_clone3
#define SYS_clone3 435
#endif
#ifndef SYS_close_range
#define SYS_close_range 436
#endif

extern char **environ;
static long seed = 1, scale = 1;
static int diag;
static uint64_t rng = 1;
static uint64_t next(void) { rng ^= rng << 13; rng ^= rng >> 7; rng ^= rng << 17; return rng; }
static uint64_t xnext(uint64_t *s) { *s ^= *s << 13; *s ^= *s >> 7; *s ^= *s << 17; return *s; }
static uint64_t xseed(long id) { return ((uint64_t)seed * 0x9E3779B97F4A7C15ULL + (uint64_t)id * 0xD1B54A32D192ED03ULL) | 1; }
#define OUT(k, fmt, ...) printf("%s " fmt "\n", k, __VA_ARGS__)
#define DIAG(...) do { if (diag) fprintf(stderr, __VA_ARGS__); } while (0)
static const char *self_path, *test_name;
static char self_abs[PATH_MAX];
static unsigned long narrow = ~0UL;
static const char *ename(int e) { const char *n = strerrorname_np(e); return n ? n : "E?"; }

/* ---- time, futexes, tasks, children ---- */

static double now(void) {
    struct timespec t;
    clock_gettime(CLOCK_MONOTONIC, &t);
    return t.tv_sec + t.tv_nsec / 1e9;
}
#include "support.h"
static long futex(int *u, int op, int v) { return syscall(SYS_futex, u, op, v, NULL, NULL, 0); }
static int load(int *u) { return __atomic_load_n(u, __ATOMIC_ACQUIRE); }
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
static int read_small(const char *path, char *buf, size_t n) {
    int fd = open(path, O_RDONLY);
    if (fd < 0) return -1;
    ssize_t k = read(fd, buf, n - 1);
    int e = errno;
    close(fd);
    errno = e;
    if (k <= 0) return -1;
    buf[k] = 0;
    return (int)k;
}
static void dump_tasks(void) {
    DIR *d = opendir("/proc/self/task");
    if (!d) return;
    struct dirent *e;
    while ((e = readdir(d))) {
        if (e->d_name[0] == '.') continue;
        char path[320], comm[64] = "?", sys[256] = "?", st[512] = "?";
        snprintf(path, sizeof path, "/proc/self/task/%s/comm", e->d_name);
        read_small(path, comm, sizeof comm);
        comm[strcspn(comm, "\n")] = 0;
        snprintf(path, sizeof path, "/proc/self/task/%s/syscall", e->d_name);
        read_small(path, sys, sizeof sys);
        sys[strcspn(sys, "\n")] = 0;
        snprintf(path, sizeof path, "/proc/self/task/%s/stat", e->d_name);
        read_small(path, st, sizeof st);
        char *p = strrchr(st, ')');
        fprintf(stderr, "task %s %s state %c syscall %s\n", e->d_name, comm, p && p[1] ? p[2] : '?', sys);
    }
    closedir(d);
}
static void stuck(const char *what, long round) {
    printf("STUCK %s round %ld\n", what, round);
    fflush(stdout);
    dump_tasks();
    _exit(3);
}
static pid_t wait_child(pid_t p, int *st, int flags) {
    for (;;) {
        pid_t r = waitpid(p, st, flags);
        if (r >= 0 || errno != EINTR) return r;
    }
}
/* Reads until EOF; -2 on timeout. */
static int read_all(int fd, char *buf, size_t n, int ms) {
    size_t got = 0;
    double end = now() + ms / 1000.0;
    for (;;) {
        int left = (int)((end - now()) * 1000);
        if (left <= 0) return -2;
        struct pollfd p = {fd, POLLIN, 0};
        int r = poll(&p, 1, left);
        if (r < 0 && errno == EINTR) continue;
        if (r < 0) return -1;
        if (r == 0) return -2;
        ssize_t k = read(fd, buf + got, n - 1 - got);
        if (k < 0 && errno == EINTR) continue;
        if (k <= 0) break;
        got += k;
        if (got >= n - 1) break;
    }
    buf[got] = 0;
    return (int)got;
}
/* Moves a descriptor to >= 30 so it never collides with the fixed numbers the tests use. */
static int high_fd(int fd, int cloexec) {
    int h = fcntl(fd, cloexec ? F_DUPFD_CLOEXEC : F_DUPFD, 30);
    close(fd);
    return h;
}
static int high_pipe(int p[2]) {
    int q[2];
    if (pipe(q)) return -1;
    p[0] = high_fd(q[0], 1);
    p[1] = high_fd(q[1], 1);
    return p[0] < 0 || p[1] < 0 ? -1 : 0;
}


static size_t pgsz = 4096;
typedef uint32_t (*fn_t)(void);
static void *jit_page(size_t n) { void *p=mmap(NULL,n,PROT_READ|PROT_WRITE|PROT_EXEC,MAP_PRIVATE|MAP_ANONYMOUS,-1,0); if(p==MAP_FAILED)abort();return p; }
struct fl { uint32_t *code; int id,stop; long period,bad,count; pthread_t t; };
static uint16_t fl_value(int id,uint32_t gen,int slot) { return (uint16_t)(id<<15 | (gen & 0x1ff)<<6 | slot); }
static int fl_check(const struct fl *f,int slot,uint32_t v) { return (v&63)==(uint32_t)slot && v>>15==(uint32_t)f->id && v<65536; }
static uint32_t fl_call(const struct fl *f,int slot) { return ((fn_t)(f->code+slot*2))(); }
static void *fl_main(void *p) { struct fl *f=p; for(unsigned i=1;!load(&f->stop);i++) { int slot=i%64; uint16_t v=fl_value(f->id,i,slot);unix_emit(f->code+slot*2,v); f->bad+=fl_call(f,slot)!=v; __atomic_add_fetch(&f->count,1,__ATOMIC_RELEASE); sched_yield(); } return NULL; }
static void fl_start(struct fl *f,int id,long period) { memset(f,0,sizeof *f);f->code=jit_page(4096);f->id=id;f->period=period;for(int s=0;s<64;s++)unix_emit(f->code+s*2,fl_value(id,0,s));if(pthread_create(&f->t,NULL,fl_main,f))abort();while(!__atomic_load_n(&f->count,__ATOMIC_ACQUIRE))sched_yield(); }
static void fl_finish(struct fl *f) { __atomic_store_n(&f->stop,1,__ATOMIC_RELEASE);pthread_join(f->t,NULL);munmap(f->code,4096); }

static uint64_t native_thread_pointer(void) {
#if defined(__aarch64__)
    uint64_t tp;asm volatile("mrs %0,tpidr_el0":"=r"(tp));return tp;
#else
    uint64_t tp=0;
    register long ax asm("rax")=SYS_arch_prctl,di asm("rdi")=0x1003,si asm("rsi")=(long)&tp;
    asm volatile("syscall":"+r"(ax):"r"(di),"r"(si):"rcx","r11","memory");return tp;
#endif
}
static void emit_tag(uint32_t *p,uint32_t v) {
#if defined(__aarch64__)
    p[0]=0x52800000u | (v & 0xffff)<<5;p[1]=0x72a00000u | (v>>16)<<5;p[2]=0xd65f03c0u;
#else
    unix_emit(p,v);
#endif
    __builtin___clear_cache((char *)p,(char *)p+12);
}
/* ==== 29. CompetingForkCallers ==== */

#define FC_PHASES 4
static int fc_go, fc_done[2], fc_rec[2], fc_k, fc_exec_stop;
static struct fl fc_w[2];
static uint32_t *fc_phasefn;
static int fc_reaped[FC_PHASES][2], fc_ok[FC_PHASES][2];
static long fc_exec_bad;
/* First claimant among failing executors owns the record. Only the controller
 * reads it, after all executor joins; no publication spin or hot-path lock. */
static int fc_bad_claimed;
static struct { uint32_t value; int writer, slot; long executor, tid; } fc_first_bad;
static void fc_record_bad(uint32_t value, int writer, int slot, long executor) {
    int expected = 0;
    if (__atomic_compare_exchange_n(&fc_bad_claimed, &expected, 1, 0,
                                    __ATOMIC_RELAXED, __ATOMIC_RELAXED)) {
        fc_first_bad.value = value;
        fc_first_bad.writer = writer;
        fc_first_bad.slot = slot;
        fc_first_bad.executor = executor;
        fc_first_bad.tid = syscall(SYS_gettid);
    }
}
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
        uint32_t value = fl_call(&fc_w[w], slot);
        int invalid = !fl_check(&fc_w[w], slot, value);
        bad += invalid;
        if (invalid) fc_record_bad(value, w, slot, k);
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
    if (fc_bad_claimed)
        fprintf(stderr, "forkcallers first executor mismatch: value=0x%08x writer=%d slot=%d phase=%ld executor=%ld tid=%ld\n",
                fc_first_bad.value, fc_first_bad.writer, fc_first_bad.slot,
                fc_first_bad.executor / 3, fc_first_bad.executor, fc_first_bad.tid);
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
    tp = native_thread_pointer();
    {
#if defined(__aarch64__)
        register long x0 asm("x0") = SIG_BLOCK;
        register long x1 asm("x1") = 0;
        register long x2 asm("x2") = (long)&kmask;
        register long x3 asm("x3") = 8;
        register long x8 asm("x8") = SYS_rt_sigprocmask;
        asm volatile("svc #0" : "+r"(x0) : "r"(x1), "r"(x2), "r"(x3), "r"(x8) : "memory");
#else
        register long ax asm("rax")=SYS_rt_sigprocmask,di asm("rdi")=SIG_BLOCK,si asm("rsi")=0,dx asm("rdx")=(long)&kmask,r10 asm("r10")=8;
        asm volatile("syscall" : "+r"(ax) : "r"(di),"r"(si),"r"(dx),"r"(r10) : "rcx","r11","memory");
#endif
    }
    uint32_t bad = cs_common(a, kmask);
    if (tp != a->tls) bad |= BAD_TLS;
    a->bad = bad;
    a->result = a->round ^ 0xabc;
    return (a->round & 0x3f) + 1;
}
#if defined(__aarch64__)
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
#else
long raw_clone(unsigned long,void *,int *,unsigned long,int *,long (*)(void *),void *);
asm(".text\n.type raw_clone,@function\nraw_clone:\nmov 8(%rsp),%rax\nmov %rax,-8(%rsi)\nmov %r9,-16(%rsi)\nsub $16,%rsi\nmov %r8,%r10\nmov %rcx,%r8\nmov $56,%eax\nsyscall\ntest %rax,%rax\njnz 1f\nmov 8(%rsp),%rdi\ncall *(%rsp)\nmov %rax,%rdi\nmov $60,%eax\nsyscall\nud2\n1: ret\n");
#endif
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
    parent_tp = native_thread_pointer();
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


int main(int argc,char **argv) {
    if(argc>1 && !strcmp(argv[1],"fcchild"))return fc_child_main(argc,argv);
    if(argc!=2)return 2;self_path=argv[0];setvbuf(stdout,NULL,_IOLBF,0);
    if(!strcmp(argv[1],"forkcallers"))return !t_forkcallers();
    if(!strcmp(argv[1],"clonestack"))return !t_clonestack();
    return 2;
}
