/*
 * Native AArch64 probes for the translator and the guest kernel, one per test:
 *   cfz TEST SEED SCALE
 * Each prints deterministic "key value" lines and ends with "OK <test>", or
 * "UNSUPPORTED <what>" when the host lacks a feature (the same on every run).
 */
#define _GNU_SOURCE
#include <dlfcn.h>
#include <errno.h>
#include <fcntl.h>
#include <linux/filter.h>
#include <linux/futex.h>
#include <linux/membarrier.h>
#include <linux/seccomp.h>
#include <poll.h>
#include <pthread.h>
#include <sched.h>
#include <setjmp.h>
#include <signal.h>
#include <stddef.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <linux/io_uring.h>
#include <sys/auxv.h>
#include <sys/ptrace.h>
#include <sys/rseq.h>
#include <sys/socket.h>
#include <sys/uio.h>
#include <elf.h>
#include <sys/epoll.h>
#include <sys/eventfd.h>
#include <sys/mman.h>
#include <sys/prctl.h>
#include <sys/select.h>
#include <sys/signalfd.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/timerfd.h>
#include <sys/wait.h>
#include <time.h>
#include <ucontext.h>
#include <unistd.h>
#include <unwind.h>
#include <linux/bpf.h>
#include <linux/perf_event.h>
#include <sys/personality.h>
#include <sys/random.h>
#include <limits.h>
#include <sys/file.h>
#include <sys/resource.h>
#include <sys/sem.h>
#include <sys/un.h>

static long seed = 1, scale = 1;
static uint64_t rng = 1;
static uint64_t next(void) { rng ^= rng << 13; rng ^= rng >> 7; rng ^= rng << 17; return rng; }
#define OUT(k, fmt, ...) printf("%s " fmt "\n", k, __VA_ARGS__)

static void publish(void *w, void *x, size_t n) {
    for (size_t i = 0; i < n; i += 4) asm volatile("dc cvau, %0" ::"r"((char *)w + i) : "memory");
    asm volatile("dsb ish" ::: "memory");
    for (size_t i = 0; i < n; i += 4) asm volatile("ic ivau, %0" ::"r"((char *)x + i) : "memory");
    asm volatile("dsb ish\nisb" ::: "memory");
}
/* movz w0, #imm ; ret */
static void emit_const(uint32_t *p, uint16_t imm) { p[0] = 0x52800000 | ((uint32_t)imm << 5); p[1] = 0xd65f03c0; }

/* ---- ISA ---- */

static uint64_t pair[2] __attribute__((aligned(16)));
static void *casp_worker(void *arg) {
    long n = (long)arg;
    for (long i = 0; i < n; i++) {
        for (;;) {
            register uint64_t lo asm("x0") = __atomic_load_n(&pair[0], __ATOMIC_RELAXED);
            register uint64_t hi asm("x1") = __atomic_load_n(&pair[1], __ATOMIC_RELAXED);
            uint64_t elo = lo, ehi = hi;
            register uint64_t nlo asm("x2") = lo + 1;
            register uint64_t nhi asm("x3") = hi + 2;
            register uint64_t *p asm("x4") = pair;
            asm volatile(".arch_extension lse\ncaspal x0, x1, x2, x3, [x4]" : "+r"(lo), "+r"(hi) : "r"(nlo), "r"(nhi), "r"(p) : "memory");
            if (lo == elo && hi == ehi) break;
        }
    }
    return NULL;
}
static int t_casp(void) {
    if (!(getauxval(AT_HWCAP) & (1 << 8))) { puts("UNSUPPORTED lse"); return 0; }
    pthread_t t[3];
    long n = 200000 * scale;
    for (int i = 0; i < 3; i++) pthread_create(&t[i], 0, casp_worker, (void *)n);
    for (int i = 0; i < 3; i++) pthread_join(t[i], 0);
    OUT("pair", "%lu %lu", (unsigned long)pair[0], (unsigned long)pair[1]);
    return 1;
}

static volatile int lse2_stop;
static void *lse2_writer(void *arg) {
    (void)arg;
    for (uint64_t i = 0; !lse2_stop; i++) asm volatile("stp %0, %0, [%1]" ::"r"(i), "r"(pair) : "memory");
    return NULL;
}
static int t_lse2(void) {
    if (!(getauxval(AT_HWCAP) & (1 << 25))) { puts("UNSUPPORTED uscat"); return 0; }
    pthread_t t;
    pthread_create(&t, 0, lse2_writer, 0);
    unsigned long bad = 0;
    for (long i = 0; i < 2000000 * scale; i++) {
        uint64_t a, b;
        asm volatile("ldp %0, %1, [%2]" : "=&r"(a), "=&r"(b) : "r"(pair) : "memory");
        bad += a != b;
    }
    lse2_stop = 1;
    pthread_join(t, 0);
    OUT("torn", "%lu", bad);
    return 1;
}

static int t_ldapr(void) {
    if (!(getauxval(AT_HWCAP) & (1 << 15))) { puts("UNSUPPORTED lrcpc"); return 0; }
    uint64_t x = 0, sum = 0;
    for (long i = 0; i < 100000 * scale; i++) {
        x = i * 3;
        uint64_t y;
        asm volatile(".arch_extension rcpc\nldapr %0, [%1]" : "=r"(y) : "r"(&x) : "memory");
        sum += y;
    }
    OUT("sum", "%lu", (unsigned long)sum);
    return 1;
}

static int t_dczva(void) {
    unsigned long z;
    asm volatile("mrs %0, dczid_el0" : "=r"(z));
    if (z & 16) { puts("UNSUPPORTED dczva"); return 0; }
    size_t n = 4UL << (z & 15);
    unsigned char *p;
    if (posix_memalign((void **)&p, n, 8 * n)) return 0;
    unsigned long ok = 0;
    for (long r = 0; r < 20000 * scale; r++) {
        memset(p, 0xa5, 8 * n);
        size_t blk = 1 + next() % 6, off = next() % n;
        asm volatile("dc zva, %0" ::"r"(p + blk * n + off) : "memory");
        size_t zeros = 0;
        for (size_t i = 0; i < 8 * n; i++) zeros += p[i] == 0;
        ok += zeros == n && p[blk * n - 1] == 0xa5 && p[(blk + 1) * n] == 0xa5 && p[blk * n] == 0;
    }
    OUT("blocks", "%zu", n);
    OUT("ok", "%lu", ok);
    return 1;
}

static int t_carry(void) {
    unsigned long total = 0;
    for (long r = 0; r < 20 * scale; r++) {
        unsigned long sum = 0;
        /* SUBS sets C; the loop never touches the flags, so every ADC adds 1. */
        asm volatile("subs xzr, xzr, xzr\nmov x9, #60000\n1: adc %0, %0, xzr\nsub x9, x9, #1\ncbnz x9, 1b"
                     : "+r"(sum)::"x9", "cc");
        total += sum;
    }
    OUT("carried", "%lu", total);
    return 1;
}

static int t_fpcr(void) {
    uint64_t oldc, olds;
    asm volatile("mrs %0, fpcr\nmrs %1, fpsr" : "=r"(oldc), "=r"(olds));
    uint64_t h = 0, inexact = 0;
    for (int mode = 0; mode < 4; mode++) {
        uint64_t c = (oldc & ~(3UL << 22)) | ((uint64_t)mode << 22);
        asm volatile("msr fpcr, %0\nmsr fpsr, xzr" ::"r"(c) : "memory");
        double x = 1.0;
        for (long i = 0; i < 200000 * scale; i++) {
            double y = 10.0 + (i & 7), r;
            asm volatile("fdiv %d0, %d1, %d2" : "=w"(r) : "w"(x), "w"(y));
            uint64_t b;
            memcpy(&b, &r, 8);
            h = h * 31 + b;
            x = r * 7.0 + 1.0;
        }
        uint64_t s, c2;
        asm volatile("mrs %0, fpsr\nmrs %1, fpcr" : "=r"(s), "=r"(c2));
        inexact = inexact * 2 + ((s >> 4) & 1);
        h = h * 31 + ((c2 >> 22) & 3);
    }
    asm volatile("msr fpcr, %0\nmsr fpsr, %1" ::"r"(oldc), "r"(olds) : "memory");
    OUT("hash", "%lx", (unsigned long)h);
    OUT("inexact", "%lu", (unsigned long)inexact);
    return 1;
}

static int t_tbi(void) {
    char *p = mmap(0, 4096, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    long sum = 0;
    for (long i = 0; i < 100000 * scale; i++) {
        char *tagged = (char *)((uintptr_t)p | ((uint64_t)(i & 0xff) << 56));
        tagged[i & 4095] = (char)i;
        sum += p[i & 4095];
    }
    int rc = prctl(55, 1UL, 0UL, 0UL, 0UL); /* PR_SET_TAGGED_ADDR_CTRL */
    ssize_t w = -1;
    if (rc == 0) {
        p[0] = 'Q';
        int fd = open("/dev/null", O_WRONLY);
        w = write(fd, (char *)((uintptr_t)p | (0xabUL << 56)), 1);
        close(fd);
    }
    OUT("sum", "%ld", sum);
    OUT("taggedSyscall", "%d", rc == 0 ? (int)w : -1);
    return 1;
}

/* ---- code ---- */

static int t_rwrx(void) {
    int fd = memfd_create("jit", 0);
    if (fd < 0 || ftruncate(fd, 4096)) { puts("UNSUPPORTED memfd"); return 0; }
    uint32_t *w = mmap(0, 4096, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    void *x = mmap(0, 4096, PROT_READ | PROT_EXEC, MAP_SHARED, fd, 0);
    long sum = 0, bad = 0;
    for (long i = 0; i < 5000 * scale; i++) {
        uint16_t v = (uint16_t)(i * 7 + 1);
        int slot = (int)(i % 64) * 4;
        emit_const(w + slot, v);
        publish(w + slot, (uint32_t *)x + slot, 8);
        int got = ((int (*)(void))((uint32_t *)x + slot))();
        sum += got;
        bad += got != v;
    }
    OUT("sum", "%ld", sum);
    OUT("stale", "%ld", bad);
    return 1;
}

static int t_mremapcode(void) {
    long sum = 0, bad = 0;
    uint32_t *p = mmap(0, 4096, PROT_READ | PROT_WRITE | PROT_EXEC, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    void *spare = mmap(0, 4096, PROT_NONE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    for (long i = 0; i < 2000 * scale; i++) {
        uint16_t v = (uint16_t)(i + 1);
        emit_const(p, v);
        __builtin___clear_cache((char *)p, (char *)p + 8);
        int a = ((int (*)(void))p)();
        void *q = mremap(p, 4096, 4096, MREMAP_MAYMOVE | MREMAP_FIXED, spare);
        int b = ((int (*)(void))q)();
        spare = p;
        p = q;
        sum += a + b;
        bad += a != v || b != v;
    }
    OUT("sum", "%ld", sum);
    OUT("wrong", "%ld", bad);
    return 1;
}

static int t_membarrier(void) {
    int q = syscall(SYS_membarrier, MEMBARRIER_CMD_QUERY, 0, 0);
    if (q < 0 || !(q & MEMBARRIER_CMD_PRIVATE_EXPEDITED_SYNC_CORE)) { puts("UNSUPPORTED sync-core"); return 0; }
    if (syscall(SYS_membarrier, MEMBARRIER_CMD_REGISTER_PRIVATE_EXPEDITED_SYNC_CORE, 0, 0)) { puts("UNSUPPORTED register"); return 0; }
    long ok = 0;
    for (long i = 0; i < 3000 * scale; i++)
        ok += syscall(SYS_membarrier, MEMBARRIER_CMD_PRIVATE_EXPEDITED_SYNC_CORE, 0, 0) == 0;
    OUT("ok", "%ld", ok);
    return 1;
}

static int t_dlopen(void) {
    long sum = 0;
    for (long i = 0; i < 300 * scale; i++) {
        void *h = dlopen("/fuzz/bin/libfzdl.so", RTLD_NOW | RTLD_LOCAL);
        if (!h) { OUT("error", "%s", dlerror()); return 0; }
        int (*f)(int) = (int (*)(int))dlsym(h, "fzdl_value");
        for (int k = 0; k < 100; k++) sum += f(k + (int)i);
        dlclose(h);
    }
    OUT("sum", "%ld", sum);
    return 1;
}

/* ---- signals ---- */

static char *heap;
static volatile sig_atomic_t faults;
static void barrier(int s, siginfo_t *si, void *uc) {
    (void)s; (void)uc;
    faults++;
    syscall(SYS_mprotect, (void *)((uintptr_t)si->si_addr & ~4095UL), 4096, PROT_READ | PROT_WRITE);
}
static int t_segvbarrier(void) {
    struct sigaction a = {0};
    a.sa_sigaction = barrier;
    a.sa_flags = SA_SIGINFO;
    sigaction(SIGSEGV, &a, 0);
    size_t pages = 64;
    heap = mmap(0, pages * 4096, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    long sum = 0;
    for (long r = 0; r < 50 * scale; r++) {
        mprotect(heap, pages * 4096, PROT_READ);
        for (size_t i = 0; i < pages * 4096; i += 1024) {
            asm volatile("strb %w0, [%1]" ::"r"((int)(r + i)), "r"(heap + i) : "memory");
            sum += heap[i];
        }
    }
    signal(SIGSEGV, SIG_DFL);
    OUT("faults", "%d", (int)faults);
    OUT("sum", "%ld", sum);
    return 1;
}

static sigjmp_buf jb;
static void jump_out(int s) { siglongjmp(jb, s); }
static int t_siglongjmp(void) {
    signal(SIGSEGV, jump_out);
    signal(SIGFPE, jump_out);
    long caught = 0;
    for (long i = 0; i < 20000 * scale; i++) {
        if (sigsetjmp(jb, 1) == 0) {
            volatile int *bad = (volatile int *)(uintptr_t)(16 + (i & 7) * 8);
            *bad = 1;
        } else {
            caught++;
        }
    }
    signal(SIGSEGV, SIG_DFL);
    OUT("caught", "%ld", caught);
    return 1;
}

static ucontext_t caller_ctx, co_ctx;
static char co_stack[65536];
static unsigned long uc_bad, uc_rounds;
static void co_body(void) {
    for (unsigned long i = 0; i < uc_rounds; i++) {
        register unsigned long x asm("x28") = 0x12345678 + i;
        asm volatile("" : "+r"(x));
        swapcontext(&co_ctx, &caller_ctx);
        asm volatile("" : "+r"(x));
        uc_bad += x != 0x12345678 + i;
    }
}
static int t_ucontext(void) {
    uc_rounds = 20000 * scale;
    getcontext(&co_ctx);
    co_ctx.uc_stack.ss_sp = co_stack;
    co_ctx.uc_stack.ss_size = sizeof co_stack;
    co_ctx.uc_link = &caller_ctx;
    makecontext(&co_ctx, co_body, 0);
    for (unsigned long i = 0; i <= uc_rounds; i++) swapcontext(&caller_ctx, &co_ctx);
    OUT("x28Changed", "%lu", uc_bad);
    return 1;
}

static int cancel_ready[2], cancel_block[2];
static volatile int cleaned;
static void cleanup(void *p) { (void)p; __atomic_add_fetch(&cleaned, 1, __ATOMIC_SEQ_CST); }
static void *cancel_worker(void *p) {
    char b;
    pthread_cleanup_push(cleanup, NULL);
    if (write(cancel_ready[1], "r", 1) != 1) return NULL;
    if ((long)p & 1) { for (;;) pause(); }
    if (read(cancel_block[0], &b, 1) < 0) return NULL;
    pthread_cleanup_pop(1);
    return NULL;
}
static int t_pthreadcancel(void) {
    if (pipe(cancel_ready) || pipe(cancel_block)) return 0;
    long canceled = 0, n = 200 * scale;
    for (long i = 0; i < n; i++) {
        pthread_t t;
        char b;
        pthread_create(&t, 0, cancel_worker, (void *)i);
        if (read(cancel_ready[0], &b, 1) != 1) return 0;
        pthread_cancel(t);
        void *r;
        pthread_join(t, &r);
        canceled += r == PTHREAD_CANCELED;
    }
    OUT("canceled", "%ld", canceled);
    OUT("cleaned", "%d", cleaned);
    return 1;
}

static void sigsys(int s, siginfo_t *si, void *ucv) {
    (void)s; (void)si;
    ucontext_t *uc = ucv;
    uc->uc_mcontext.regs[0] = 1234; /* emulated getppid result */
}
static int t_seccomp(void) {
    pid_t child = fork();
    if (child == 0) {
        struct sigaction a = {0};
        a.sa_sigaction = sigsys;
        a.sa_flags = SA_SIGINFO;
        sigaction(SIGSYS, &a, 0);
        struct sock_filter f[] = {
            BPF_STMT(BPF_LD | BPF_W | BPF_ABS, offsetof(struct seccomp_data, nr)),
            BPF_JUMP(BPF_JMP | BPF_JEQ | BPF_K, SYS_getppid, 0, 1),
            BPF_STMT(BPF_RET | BPF_K, SECCOMP_RET_TRAP),
            BPF_STMT(BPF_RET | BPF_K, SECCOMP_RET_ALLOW),
        };
        struct sock_fprog prog = {sizeof f / sizeof f[0], f};
        if (prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) || prctl(PR_SET_SECCOMP, SECCOMP_MODE_FILTER, &prog)) _exit(99);
        long sum = 0;
        for (long i = 0; i < 2000 * scale; i++) sum += syscall(SYS_getppid);
        _exit(sum == 1234L * 2000 * scale ? 0 : 1);
    }
    int st;
    waitpid(child, &st, 0);
    if (WIFEXITED(st) && WEXITSTATUS(st) == 99) { puts("UNSUPPORTED seccomp"); return 0; }
    OUT("emulated", "%d", WIFEXITED(st) && WEXITSTATUS(st) == 0);
    return 1;
}

static volatile sig_atomic_t autodisarm_seen;
static void ad_handler(int s) {
    (void)s;
    stack_t a;
    sigaltstack(NULL, &a);
    autodisarm_seen += !!(a.ss_flags & SS_DISABLE);
}
static int t_autodisarm(void) {
    stack_t s = {.ss_sp = malloc(65536), .ss_size = 65536, .ss_flags = (int)(1U << 31)};
    if (sigaltstack(&s, 0)) { puts("UNSUPPORTED autodisarm"); return 0; }
    struct sigaction a = {0};
    a.sa_handler = ad_handler;
    a.sa_flags = SA_ONSTACK;
    sigaction(SIGUSR1, &a, 0);
    for (long i = 0; i < 1000 * scale; i++) raise(SIGUSR1);
    sigaltstack(NULL, &s);
    OUT("disarmedInHandler", "%d", (int)autodisarm_seen);
    OUT("stillAutoDisarm", "%d", !!(s.ss_flags & (1U << 31)));
    return 1;
}

static int t_signalfd(void) {
    sigset_t m;
    sigemptyset(&m);
    sigaddset(&m, SIGRTMIN + 1);
    sigprocmask(SIG_BLOCK, &m, 0);
    int fd = signalfd(-1, &m, 0);
    long sum = 0, order = 1;
    for (long r = 0; r < 200 * scale; r++) {
        for (int k = 0; k < 8; k++) {
            union sigval v = {.sival_int = (int)(r * 8 + k)};
            sigqueue(getpid(), SIGRTMIN + 1, v);
        }
        for (int k = 0; k < 8; k++) {
            struct signalfd_siginfo si;
            if (read(fd, &si, sizeof si) != sizeof si) return 0;
            sum += si.ssi_int;
            order &= si.ssi_int == (int)(r * 8 + k);
        }
    }
    OUT("sum", "%ld", sum);
    OUT("inOrder", "%ld", order);
    return 1;
}

static volatile sig_atomic_t pselect_hits;
static void ps_handler(int s) { (void)s; pselect_hits++; }
static int t_pselect(void) {
    signal(SIGUSR2, ps_handler);
    sigset_t block, empty;
    sigemptyset(&block);
    sigaddset(&block, SIGUSR2);
    sigemptyset(&empty);
    sigprocmask(SIG_BLOCK, &block, 0);
    long eintr = 0;
    for (long i = 0; i < 500 * scale; i++) {
        raise(SIGUSR2); /* pending while blocked */
        struct timespec t = {5, 0};
        int r = pselect(0, 0, 0, 0, &t, &empty);
        eintr += r < 0 && errno == EINTR;
    }
    sigprocmask(SIG_UNBLOCK, &block, 0);
    OUT("eintr", "%ld", eintr);
    OUT("handled", "%d", (int)pselect_hits);
    return 1;
}

/* sigsuspend must return at once for a signal that its mask unblocks and that is
 * already pending, as HotSpot's suspend/resume handler relies on. */
static volatile sig_atomic_t suspend_hits;
static void ss_handler(int s) { (void)s; suspend_hits++; }
static int t_sigsuspend(void) {
    signal(SIGUSR2, ss_handler);
    sigset_t block, empty;
    sigemptyset(&block);
    sigaddset(&block, SIGUSR2);
    sigemptyset(&empty);
    sigprocmask(SIG_BLOCK, &block, 0);
    long eintr = 0;
    for (long i = 0; i < 500 * scale; i++) {
        raise(SIGUSR2); /* pending while blocked */
        int r = sigsuspend(&empty);
        eintr += r < 0 && errno == EINTR;
    }
    sigprocmask(SIG_UNBLOCK, &block, 0);
    OUT("eintr", "%ld", eintr);
    OUT("handled", "%d", (int)suspend_hits);
    return 1;
}

static volatile sig_atomic_t cpu_ticks;
static void cpu_tick(int s) { (void)s; cpu_ticks++; }
static int t_cputimer(void) {
    signal(SIGPROF, cpu_tick);
    timer_t t;
    struct sigevent ev = {.sigev_notify = SIGEV_SIGNAL, .sigev_signo = SIGPROF};
    if (timer_create(CLOCK_PROCESS_CPUTIME_ID, &ev, &t)) { puts("UNSUPPORTED cputimer"); return 0; }
    struct itimerspec its = {{0, 2000000}, {0, 2000000}};
    timer_settime(t, 0, &its, 0);
    volatile uint64_t h = 1;
    while (cpu_ticks < 20 * scale) h = h * 6364136223846793005ULL + 1;
    timer_delete(t);
    OUT("ticksAtLeast", "%d", cpu_ticks >= 20 * scale);
    return 1;
}

/* ---- kernel interfaces ---- */

static int t_futexwaitv(void) {
    struct W { uint64_t val, uaddr; uint32_t flags, reserved; };
    uint32_t a = 0, b = 0;
    struct W w[2] = {{0, (uintptr_t)&a, 2 | 128, 0}, {1, (uintptr_t)&b, 2 | 128, 0}};
    long r = syscall(449, w, 2, 0, NULL, CLOCK_MONOTONIC);
    int e = errno;
    if (r < 0 && e == ENOSYS) { puts("UNSUPPORTED futex_waitv"); return 0; }
    w[1].val = 0;
    struct timespec end;
    clock_gettime(CLOCK_MONOTONIC, &end);
    end.tv_nsec += 2000000;
    if (end.tv_nsec >= 1000000000) { end.tv_sec++; end.tv_nsec -= 1000000000; }
    long s = syscall(449, w, 2, 0, &end, CLOCK_MONOTONIC);
    int f = errno;
    OUT("mismatchEagain", "%d", r == -1 && e == EAGAIN);
    OUT("deadlineTimedout", "%d", s == -1 && f == ETIMEDOUT);
    return 1;
}

static int t_epolloneshot(void) {
    int ep = epoll_create1(0), ev = eventfd(1, 0);
    struct epoll_event e = {.events = EPOLLIN | EPOLLONESHOT, .data.u32 = 7};
    epoll_ctl(ep, EPOLL_CTL_ADD, ev, &e);
    long seen = 0, suppressed = 0;
    for (long i = 0; i < 2000 * scale; i++) {
        struct epoll_event out;
        seen += epoll_wait(ep, &out, 1, 0) == 1;
        suppressed += epoll_wait(ep, &out, 1, 0) == 0;
        epoll_ctl(ep, EPOLL_CTL_MOD, ev, &e);
    }
    OUT("seen", "%ld", seen);
    OUT("suppressed", "%ld", suppressed);
    return 1;
}

static int t_pidfd(void) {
    long ok = 0;
    for (long i = 0; i < 20 * scale; i++) {
        pid_t p = fork();
        if (!p) { pause(); _exit(0); }
        int fd = syscall(434, p, 0);
        if (fd < 0) { puts("UNSUPPORTED pidfd"); kill(p, SIGKILL); waitpid(p, 0, 0); return 0; }
        syscall(424, fd, SIGTERM, NULL, 0);
        struct pollfd pf = {fd, POLLIN, 0};
        poll(&pf, 1, -1);
        siginfo_t info = {0};
        int r = waitid((idtype_t)3, fd, &info, WEXITED);
        ok += r == 0 && info.si_code == CLD_KILLED && info.si_status == SIGTERM;
        close(fd);
    }
    OUT("ok", "%ld", ok);
    return 1;
}

static void usr1(int s) { (void)s; }
static int t_clone3(void) {
    struct C { uint64_t flags, pidfd, child_tid, parent_tid, exit_signal, stack, stack_size, tls, set_tid, set_tid_size, cgroup; };
    signal(SIGUSR1, usr1);
    long ok = 0;
    for (long i = 0; i < 20 * scale; i++) {
        int fd = -1;
        struct C a = {.flags = 0x1000ULL | 0x100000000ULL, .pidfd = (uintptr_t)&fd, .exit_signal = SIGCHLD};
        long child = syscall(435, &a, sizeof a);
        if (child < 0) { puts("UNSUPPORTED clone3"); return 0; }
        if (!child) {
            struct sigaction s;
            sigaction(SIGUSR1, NULL, &s);
            _exit(s.sa_handler == SIG_DFL ? 42 : 1);
        }
        int st;
        waitpid(child, &st, 0);
        ok += fd >= 0 && WEXITSTATUS(st) == 42;
        close(fd);
    }
    OUT("ok", "%ld", ok);
    return 1;
}

static int t_memfdexec(void) {
    long ok = 0;
    char self[4096];
    ssize_t n = readlink("/proc/self/exe", self, sizeof self - 1);
    if (n < 0) return 0;
    self[n] = 0;
    for (long i = 0; i < 10 * scale; i++) {
        int src = open(self, O_RDONLY);
        int fd = memfd_create("cfz", MFD_CLOEXEC);
        char buf[65536];
        ssize_t k;
        while ((k = read(src, buf, sizeof buf)) > 0)
            if (write(fd, buf, k) != k) return 0;
        close(src);
        pid_t p = fork();
        if (!p) {
            char *argv[] = {"cfz", "child", NULL};
            fexecve(fd, argv, environ);
            _exit(127);
        }
        int st;
        waitpid(p, &st, 0);
        ok += WIFEXITED(st) && WEXITSTATUS(st) == 42;
        close(fd);
    }
    OUT("ok", "%ld", ok);
    return 1;
}

static int t_vmops(void) {
    /* MADV_FREE rescue, MREMAP_DONTUNMAP refault, F_SEAL_FUTURE_WRITE. */
    long a = 0, b = 0, c = 0;
    for (long i = 0; i < 500 * scale; i++) {
        char *p = mmap(0, 4096, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
        p[0] = 1;
        madvise(p, 4096, MADV_FREE);
        p[0] = 42;
        int before = p[0];
        madvise(p, 4096, MADV_DONTNEED);
        a += before == 42 && p[0] == 0;
        p[0] = 42;
        char *q = mremap(p, 4096, 4096, MREMAP_MAYMOVE | 4 /* DONTUNMAP */);
        if (q != MAP_FAILED) { b += p[0] == 0 && q[0] == 42; munmap(q, 4096); }
        munmap(p, 4096);
    }
    int fd = memfd_create("seal", MFD_ALLOW_SEALING);
    if (fd >= 0 && !ftruncate(fd, 4096)) {
        char *p = mmap(0, 4096, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
        fcntl(fd, F_ADD_SEALS, 0x10);
        p[0] = 42;
        ssize_t r = pwrite(fd, "X", 1, 0);
        int e = errno;
        char x = 0;
        if (pread(fd, &x, 1, 0) == 1) c = x == 42 && r == -1 && e == EPERM;
    }
    OUT("madvFree", "%ld", a);
    OUT("dontUnmap", "%ld", b);
    OUT("sealFutureWrite", "%ld", c);
    return 1;
}

static int t_timerfd(void) {
    int fd = timerfd_create(CLOCK_MONOTONIC, 0);
    struct itimerspec t = {{0, 1000000}, {0, 1000000}};
    timerfd_settime(fd, 0, &t, 0);
    long total = 0;
    for (long r = 0; r < 20 * scale; r++) {
        for (volatile unsigned i = 0; i < 2000000; i++);
        uint64_t n;
        if (read(fd, &n, 8) == 8) total += n > 0;
    }
    OUT("readsWithExpiry", "%ld", total);
    return 1;
}


/* ---- batch 2 ---- */

static volatile sig_atomic_t rseq_alarms;
static void rseq_alarm(int s) { (void)s; rseq_alarms++; }
static int t_rseq(void) {
    if (__rseq_size < 20) { puts("UNSUPPORTED rseq"); return 0; }
    struct rseq *r = (struct rseq *)((char *)__builtin_thread_pointer() + __rseq_offset);
    signal(SIGALRM, rseq_alarm);
    struct itimerval it = {{0, 100}, {0, 100}};
    setitimer(ITIMER_REAL, &it, NULL);
    long commits = 0, aborts = 0;
    for (long i = 0; i < 2000 * scale; i++) {
        unsigned result;
        asm volatile(
            ".pushsection __rseq_cs, \"aw\"\n.balign 32\n1: .long 0, 0\n.quad 2f, 3f-2f, 4f\n.popsection\n"
            "adrp x9, 1b\nadd x9, x9, :lo12:1b\nstr x9, [%[r], #8]\n"
            "2: mov x10, #20000\n5: subs x10, x10, #1\nb.ne 5b\nmov %w[v], #1\n3: b 6f\n"
            ".inst 0xd428bc00\n4: mov %w[v], #2\n6: str xzr, [%[r], #8]\n"
            : [v] "=&r"(result) : [r] "r"(r) : "x9", "x10", "cc", "memory");
        if (result == 1) commits++; else aborts++;
    }
    struct itimerval off = {{0, 0}, {0, 0}};
    setitimer(ITIMER_REAL, &off, NULL);
    OUT("attempts", "%ld", commits + aborts);
    OUT("bothOutcomes", "%d", commits > 0 && aborts > 0);
    return 1;
}

static int t_iouring(void) {
    struct io_uring_params p;
    memset(&p, 0, sizeof p);
    int fd = syscall(425, 8, &p);
    if (fd < 0) { OUT("UNSUPPORTED", "io_uring %s", strerror(errno)); return 0; }
    char *sq = mmap(0, p.sq_off.array + 4 * p.sq_entries, PROT_READ | PROT_WRITE, MAP_SHARED, fd, IORING_OFF_SQ_RING);
    char *cq = mmap(0, p.cq_off.cqes + sizeof(struct io_uring_cqe) * p.cq_entries, PROT_READ | PROT_WRITE, MAP_SHARED, fd, IORING_OFF_CQ_RING);
    struct io_uring_sqe *e = mmap(0, sizeof(*e) * p.sq_entries, PROT_READ | PROT_WRITE, MAP_SHARED, fd, IORING_OFF_SQES);
    long ok = 0;
    uint32_t mask = *(uint32_t *)(sq + p.sq_off.ring_mask), cmask = *(uint32_t *)(cq + p.cq_off.ring_mask);
    for (long i = 0; i < 2000 * scale; i++) {
        uint32_t tail = __atomic_load_n((uint32_t *)(sq + p.sq_off.tail), __ATOMIC_ACQUIRE);
        uint32_t idx = tail & mask;
        memset(&e[idx], 0, sizeof e[idx]);
        e[idx].opcode = IORING_OP_NOP;
        e[idx].user_data = (uint64_t)i + 42;
        if (i & 1) e[idx].flags = IOSQE_ASYNC;
        ((uint32_t *)(sq + p.sq_off.array))[idx] = idx;
        __atomic_store_n((uint32_t *)(sq + p.sq_off.tail), tail + 1, __ATOMIC_RELEASE);
        if (syscall(426, fd, 1, 1, IORING_ENTER_GETEVENTS, NULL, 0) < 0) break;
        uint32_t head = __atomic_load_n((uint32_t *)(cq + p.cq_off.head), __ATOMIC_ACQUIRE);
        uint32_t ctail = __atomic_load_n((uint32_t *)(cq + p.cq_off.tail), __ATOMIC_ACQUIRE);
        if (head != ctail) {
            struct io_uring_cqe *c = (struct io_uring_cqe *)(cq + p.cq_off.cqes) + (head & cmask);
            ok += c->user_data == (uint64_t)i + 42 && c->res == 0;
            __atomic_store_n((uint32_t *)(cq + p.cq_off.head), head + 1, __ATOMIC_RELEASE);
        }
    }
    OUT("completed", "%ld", ok);
    return 1;
}

static int t_ptrace(void) {
    long ok = 0;
    for (long i = 0; i < 10 * scale; i++) {
        pid_t p = fork();
        if (!p) { ptrace(PTRACE_TRACEME, 0, 0, 0); raise(SIGSTOP); _exit(42); }
        int st;
        waitpid(p, &st, 0);
        uint64_t regs[34] = {0};
        struct iovec v = {regs, sizeof regs};
        long r = ptrace(PTRACE_GETREGSET, p, (void *)NT_PRSTATUS, &v);
        if (r < 0 && i == 0) { OUT("UNSUPPORTED", "ptrace %s", strerror(errno)); kill(p, SIGKILL); waitpid(p, 0, 0); return 0; }
        ptrace(PTRACE_CONT, p, 0, 0);
        waitpid(p, &st, 0);
        ok += r == 0 && v.iov_len == sizeof regs && WIFEXITED(st) && WEXITSTATUS(st) == 42;
    }
    OUT("ok", "%ld", ok);
    return 1;
}

static int t_wipeonfork(void) {
    char *p = mmap(0, 4096, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (madvise(p, 4096, MADV_WIPEONFORK)) { puts("UNSUPPORTED wipeonfork"); return 0; }
    long ok = 0;
    for (long i = 0; i < 10 * scale; i++) {
        p[0] = 42;
        pid_t c = fork();
        if (!c) _exit(p[0] == 0 ? 7 : 1);
        int st;
        waitpid(c, &st, 0);
        ok += WEXITSTATUS(st) == 7 && p[0] == 42;
    }
    OUT("wiped", "%ld", ok);
    return 1;
}

static int t_scmrights(void) {
    long ok = 0;
    for (long i = 0; i < 500 * scale; i++) {
        int s[2];
        socketpair(AF_UNIX, SOCK_STREAM, 0, s);
        int fd = eventfd((unsigned)(i + 7), 0);
        char byte = 'x';
        struct iovec io = {&byte, 1};
        union { struct cmsghdr align; char bytes[CMSG_SPACE(sizeof(int))]; } ctl;
        memset(&ctl, 0, sizeof ctl);
        struct msghdr m = {.msg_iov = &io, .msg_iovlen = 1, .msg_control = ctl.bytes, .msg_controllen = sizeof ctl};
        struct cmsghdr *c = CMSG_FIRSTHDR(&m);
        c->cmsg_level = SOL_SOCKET;
        c->cmsg_type = SCM_RIGHTS;
        c->cmsg_len = CMSG_LEN(sizeof(int));
        memcpy(CMSG_DATA(c), &fd, sizeof fd);
        sendmsg(s[0], &m, 0);
        union { struct cmsghdr align; char bytes[CMSG_SPACE(sizeof(int))]; } rctl;
        struct msghdr rm = {.msg_iov = &io, .msg_iovlen = 1, .msg_control = rctl.bytes, .msg_controllen = sizeof rctl};
        if (recvmsg(s[1], &rm, MSG_CMSG_CLOEXEC) == 1) {
            int got;
            memcpy(&got, CMSG_DATA(CMSG_FIRSTHDR(&rm)), sizeof got);
            uint64_t v = 0;
            ok += read(got, &v, 8) == 8 && v == (uint64_t)(i + 7) && (fcntl(got, F_GETFD) & FD_CLOEXEC);
            close(got);
        }
        close(fd);
        close(s[0]);
        close(s[1]);
    }
    OUT("ok", "%ld", ok);
    return 1;
}

static int ifunc_answer(void) { return 42; }
static int (*ifunc_resolve(void))(void) { return ifunc_answer; }
int ifunc_value(void) __attribute__((ifunc("ifunc_resolve")));
static int t_ifunc(void) {
    long sum = 0;
    for (long i = 0; i < 100000 * scale; i++) sum += ifunc_value();
    OUT("sum", "%ld", sum);
    return 1;
}

/* ---- batch 3 ---- */

static const char *argv0 = "cfz";

/* Robust (optionally PI) mutexes whose owners die: robust-list traversal at
 * thread and process exit, owner-death handoff to a waiter blocked in the
 * kernel, and the not-recoverable state after an unrepaired unlock. */
static pthread_mutex_t rpi;
static void *rpi_die(void *p) { (void)p; pthread_mutex_lock(&rpi); return NULL; }
static int rpi_waiting(pthread_mutex_t *m) {
    /* glibc keeps the futex word first; FUTEX_WAITERS appears once a waiter is in the kernel. */
    return (__atomic_load_n((uint32_t *)m, __ATOMIC_ACQUIRE) & FUTEX_WAITERS) != 0;
}
static int rpi_ready[2];
static void *rpi_die_contended(void *p) {
    (void)p;
    pthread_mutex_lock(&rpi);
    if (write(rpi_ready[1], "r", 1) != 1) return NULL;
    for (long i = 0; i < 10000000 && !rpi_waiting(&rpi); i++) sched_yield();
    return NULL;
}
static int rpi_init(pthread_mutex_t *m, int pi, int pshared) {
    pthread_mutexattr_t a;
    pthread_mutexattr_init(&a);
    pthread_mutexattr_setrobust(&a, PTHREAD_MUTEX_ROBUST);
    if (pi) pthread_mutexattr_setprotocol(&a, PTHREAD_PRIO_INHERIT);
    if (pshared) pthread_mutexattr_setpshared(&a, PTHREAD_PROCESS_SHARED);
    int r = pthread_mutex_init(m, &a);
    pthread_mutexattr_destroy(&a);
    return r;
}
static int robust_suite(int pi) {
    long dead = 0, relocked = 0, contended = 0, procdead = 0, unrecoverable = 0;
    int r = rpi_init(&rpi, pi, 0);
    if (r) { OUT("UNSUPPORTED", "%s mutex %s", pi ? "pi" : "robust", strerror(r)); return 0; }
    for (long i = 0; i < 50 * scale; i++) {
        pthread_t t;
        pthread_create(&t, 0, rpi_die, 0);
        pthread_join(t, 0);
        r = pthread_mutex_lock(&rpi);
        dead += r == EOWNERDEAD;
        if (r == EOWNERDEAD) pthread_mutex_consistent(&rpi);
        pthread_mutex_unlock(&rpi);
        relocked += pthread_mutex_lock(&rpi) == 0 && pthread_mutex_unlock(&rpi) == 0;
    }
    if (pipe(rpi_ready)) return 0;
    for (long i = 0; i < 20 * scale; i++) {
        pthread_t t;
        char b;
        pthread_create(&t, 0, rpi_die_contended, 0);
        if (read(rpi_ready[0], &b, 1) != 1) return 0;
        r = pthread_mutex_lock(&rpi); /* blocks in the kernel until the owner exits */
        contended += r == EOWNERDEAD;
        if (r == EOWNERDEAD) pthread_mutex_consistent(&rpi);
        pthread_mutex_unlock(&rpi);
        pthread_join(t, 0);
    }
    pthread_mutex_t *shared = mmap(0, 4096, PROT_READ | PROT_WRITE, MAP_SHARED | MAP_ANONYMOUS, -1, 0);
    if (rpi_init(shared, pi, 1)) return 0;
    for (long i = 0; i < 10 * scale; i++) {
        pid_t c = fork();
        if (!c) { pthread_mutex_lock(shared); _exit(0); }
        waitpid(c, 0, 0);
        r = pthread_mutex_lock(shared);
        procdead += r == EOWNERDEAD;
        if (r == EOWNERDEAD) pthread_mutex_consistent(shared);
        pthread_mutex_unlock(shared);
    }
    pthread_t t;
    pthread_create(&t, 0, rpi_die, 0);
    pthread_join(t, 0);
    r = pthread_mutex_lock(&rpi);
    if (r == EOWNERDEAD) pthread_mutex_unlock(&rpi); /* without pthread_mutex_consistent */
    unrecoverable = pthread_mutex_lock(&rpi) == ENOTRECOVERABLE;
    OUT("ownerDead", "%ld", dead);
    OUT("relocked", "%ld", relocked);
    OUT("waiterOwnerDead", "%ld", contended);
    OUT("processOwnerDead", "%ld", procdead);
    OUT("notRecoverable", "%ld", unrecoverable);
    return 1;
}
static int t_robust(void) { return robust_suite(0); }
static int t_robustpi(void) { return robust_suite(1); }

/* A failed MAP_FIXED_NOREPLACE must leave data, code and neighbouring holes unchanged. */
static int mapped(void *p) { unsigned char v; return mincore(p, 4096, &v) == 0; }
static int t_mapnoreplace(void) {
    long data = 0, partial = 0, code = 0, hole = 0, eexist = 0;
    for (long i = 0; i < 200 * scale; i++) {
        char *p = mmap(0, 4 * 4096, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
        munmap(p + 2 * 4096, 2 * 4096);
        p[0] = 42;
        p[4096] = 43;
        void *q = mmap(p, 4096, PROT_NONE, MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED_NOREPLACE, -1, 0);
        int e = errno;
        p[1] = 1; /* still writable */
        data += q == MAP_FAILED && p[0] == 42;
        eexist += q == MAP_FAILED && e == EEXIST;
        q = mmap(p + 4096, 2 * 4096, PROT_NONE, MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED_NOREPLACE, -1, 0);
        e = errno;
        partial += q == MAP_FAILED && p[4096] == 43 && !mapped(p + 2 * 4096);
        eexist += q == MAP_FAILED && e == EEXIST;
        q = mmap(p + 2 * 4096, 4096, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED_NOREPLACE, -1, 0);
        hole += q == p + 2 * 4096 && ((char *)q)[0] == 0;
        munmap(p, 3 * 4096);
        uint32_t *x = mmap(0, 4096, PROT_READ | PROT_WRITE | PROT_EXEC, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
        uint16_t v = (uint16_t)(i + 5);
        emit_const(x, v);
        __builtin___clear_cache((char *)x, (char *)x + 8);
        int a = ((int (*)(void))x)();
        q = mmap(x, 4096, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED_NOREPLACE, -1, 0);
        e = errno;
        int b = ((int (*)(void))x)();
        code += q == MAP_FAILED && a == v && b == v;
        eexist += q == MAP_FAILED && e == EEXIST;
        munmap(x, 4096);
    }
    OUT("data", "%ld", data);
    OUT("partialOverlap", "%ld", partial);
    OUT("hole", "%ld", hole);
    OUT("code", "%ld", code);
    OUT("eexist", "%ld", eexist);
    return 1;
}

/* process_vm_readv/writev stop at the first inaccessible byte and return the completed prefix. */
static int t_vmreadv(void) {
    char *p = mmap(0, 3 * 4096, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    memset(p, 42, 4096);
    memset(p + 2 * 4096, 44, 4096);
    mprotect(p + 4096, 4096, PROT_NONE);
    char *ro = mmap(0, 4096, PROT_READ, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    char *hole = mmap(0, 4096, PROT_NONE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    static char out[3 * 4096];
    pid_t me = getpid();
    struct iovec local = {out, sizeof out}, remote[2] = {{p, 4096}, {p + 4096, 4096}};
    ssize_t n = process_vm_readv(me, &local, 1, remote, 2, 0);
    if (n < 0 && (errno == ENOSYS || errno == EPERM)) { OUT("UNSUPPORTED", "process_vm_readv %s", strerror(errno)); return 0; }
    OUT("secondIovec", "%zd %d", n, out[4095]);
    long sum = 0, ok = 0;
    for (long i = 0; i < 200 * scale; i++) {
        size_t off = 1 + next() % 4095;
        struct iovec r1 = {p + 4096 - off, 4096};
        memset(out, 0, 4096);
        ssize_t m = process_vm_readv(me, &local, 1, &r1, 1, 0);
        sum += m;
        ok += m == (ssize_t)off && out[off - 1] == 42 && out[off] == 0;
    }
    OUT("prefixSum", "%ld", sum);
    OUT("prefixOk", "%ld", ok);
    struct iovec l2[2] = {{out, 1000}, {hole, 1000}}, r2 = {p + 2 * 4096, 2000};
    n = process_vm_readv(me, l2, 2, &r2, 1, 0);
    OUT("localFault", "%zd %d", n, out[999]);
    char src[200];
    memset(src, 7, sizeof src);
    struct iovec l3 = {src, 200}, r3[2] = {{p + 2 * 4096, 100}, {ro, 100}};
    n = process_vm_writev(me, &l3, 1, r3, 2, 0);
    OUT("writeReadOnly", "%zd %d %d", n, p[2 * 4096 + 99], ro[0]);
    /* Another process at the same addresses after fork. */
    int ready[2], go[2];
    if (pipe(ready) || pipe(go)) return 0;
    pid_t c = fork();
    if (!c) {
        char b;
        memset(p, 99, 4096);
        if (write(ready[1], "r", 1) != 1) _exit(1);
        if (read(go[0], &b, 1) != 1) _exit(1);
        _exit(p[0] == 5 && p[1] == 5 && p[2] == 99 ? 0 : 1);
    }
    char b;
    if (read(ready[0], &b, 1) != 1) return 0;
    struct iovec l4 = {out, 4096}, r4 = {p, 4096};
    ssize_t rn = process_vm_readv(c, &l4, 1, &r4, 1, 0);
    char five[2] = {5, 5};
    struct iovec l5 = {five, 2}, r5 = {p, 2};
    ssize_t wn = process_vm_writev(c, &l5, 1, &r5, 1, 0);
    if (write(go[1], "g", 1) != 1) return 0;
    int st;
    waitpid(c, &st, 0);
    OUT("child", "%zd %d %zd %d %d", rn, out[0], wn, WIFEXITED(st) && WEXITSTATUS(st) == 0, p[0]);
    return 1;
}

/* A nested PID namespace: its init is PID 1, reaps its own children and
 * adopted orphans, and the namespace accepts no new process once init exits. */
static int nspid_last(void) {
    FILE *f = fopen("/proc/self/status", "r");
    char line[256];
    int last = -1;
    while (f && fgets(line, sizeof line, f)) {
        if (strncmp(line, "NSpid:", 6)) continue;
        for (char *s = strtok(line + 6, " \t\n"); s; s = strtok(NULL, " \t\n")) last = atoi(s);
    }
    if (f) fclose(f);
    return last;
}
static int t_pidns(void) {
    int *sh = mmap(0, 4096, PROT_READ | PROT_WRITE, MAP_SHARED | MAP_ANONYMOUS, -1, 0);
    long rounds = 3 * scale, good = 0;
    for (long round = 0; round < rounds; round++) {
        memset(sh, 0xff, 4096);
        pid_t c = fork();
        if (!c) {
            if (unshare(CLONE_NEWUSER | CLONE_NEWPID) && unshare(CLONE_NEWPID)) _exit(99);
            pid_t init = fork();
            if (!init) {
                sh[0] = getpid();
                sh[1] = getppid();
                sh[2] = nspid_last();
                pid_t k = fork();
                if (!k) _exit(7);
                int st;
                waitpid(k, &st, 0);
                sh[3] = k;
                sh[4] = WEXITSTATUS(st);
                pid_t mid = fork();
                if (!mid) {
                    pid_t o = fork();
                    if (!o) {
                        for (long j = 0; j < 100000000 && getppid() != 1; j++) sched_yield();
                        sh[5] = getppid();
                        _exit(9);
                    }
                    _exit(5);
                }
                int n = 0, sum = 0;
                for (;;) {
                    pid_t w = wait(&st);
                    if (w < 0) break;
                    n++;
                    sum += WEXITSTATUS(st);
                }
                sh[6] = n;
                sh[7] = sum;
                _exit(3);
            }
            int st;
            waitpid(init, &st, 0);
            sh[8] = WIFEXITED(st) ? WEXITSTATUS(st) : -1;
            pid_t again = fork();
            if (!again) _exit(0);
            sh[9] = again < 0 ? errno : 0;
            if (again > 0) waitpid(again, 0, 0);
            _exit(0);
        }
        int st;
        waitpid(c, &st, 0);
        if (WIFEXITED(st) && WEXITSTATUS(st) == 99) { OUT("UNSUPPORTED", "pidns %s", "unshare"); return 0; }
        if (round == 0) {
            OUT("init", "%d %d %d", sh[0], sh[1], sh[2]);
            OUT("child", "%d %d", sh[3], sh[4]);
            OUT("orphanParent", "%d", sh[5]);
            OUT("reaped", "%d %d", sh[6], sh[7]);
            OUT("initExit", "%d", sh[8]);
            OUT("forkAfterInit", "%s", sh[9] ? strerror(sh[9]) : "ok");
        }
        good += sh[0] == 1 && sh[1] == 0 && sh[4] == 7 && sh[5] == 1 && sh[6] == 2 && sh[7] == 14 && sh[8] == 3 && sh[9] == ENOMEM;
    }
    OUT("rounds", "%ld/%ld", good, rounds);
    return 1;
}

/* getrandom sizes, flags and fork: structural results are deterministic, bytes are not. */
static void fill_random(unsigned char *p, size_t n) {
    while (n) {
        ssize_t k = getrandom(p, n, 0);
        if (k < 0 && errno == EINTR) continue;
        if (k <= 0) _exit(2);
        p += k;
        n -= (size_t)k;
    }
}
static uint64_t fnv(const unsigned char *p, size_t n, uint64_t h) {
    for (size_t i = 0; i < n; i++) h = (h ^ p[i]) * 0x100000001b3ULL;
    return h;
}
struct fork_entropy { unsigned char before[257], child[257], after[257]; };
static int fork_entropy(struct fork_entropy *e) {
    fill_random(e->before, sizeof e->before);
    int fd[2];
    if (pipe(fd)) return 0;
    pid_t c = fork();
    if (!c) {
        unsigned char b[257];
        fill_random(b, sizeof b);
        _exit(write(fd[1], b, sizeof b) == sizeof b ? 0 : 1);
    }
    size_t off = 0;
    while (off < sizeof e->child) {
        ssize_t n = read(fd[0], e->child + off, sizeof e->child - off);
        if (n <= 0) return 0;
        off += (size_t)n;
    }
    waitpid(c, 0, 0);
    close(fd[0]);
    close(fd[1]);
    fill_random(e->after, sizeof e->after);
    return 1;
}
static int t_getrandom(void) {
    static unsigned char big[1 << 20];
    size_t sizes[] = {0, 1, 255, 256, 257, 4096, 65537, sizeof big};
    unsigned flags[] = {0, GRND_NONBLOCK, 4 /* GRND_INSECURE */, GRND_RANDOM | GRND_NONBLOCK};
    for (size_t f = 0; f < 4; f++) {
        char line[256];
        int len = 0;
        for (size_t i = 0; i < sizeof sizes / sizeof sizes[0]; i++) {
            ssize_t r = getrandom(big, sizes[i], flags[f]);
            len += snprintf(line + len, sizeof line - len, " %zd", r < 0 ? -errno : r);
        }
        OUT("sizes", "%u%s", flags[f], line);
    }
    errno = 0;
    OUT("badFlags", "%zd %s", getrandom(big, 16, 0x80), strerror(errno));
    errno = 0;
    OUT("badBuffer", "%zd %s", getrandom(NULL, 16, 0), strerror(errno));
    long distinct = 0, rounds = 20 * scale;
    for (long i = 0; i < rounds; i++) {
        struct fork_entropy e;
        if (!fork_entropy(&e)) return 0;
        distinct += memcmp(e.before, e.child, 257) && memcmp(e.child, e.after, 257) && memcmp(e.before, e.after, 257);
    }
    OUT("forkDistinct", "%ld/%ld", distinct, rounds);
    return 1;
}
/* replay-only: the digests of the same sequence. */
static int t_getrandomhash(void) {
    uint64_t h = 0xcbf29ce484222325ULL;
    for (long i = 0; i < 5 * scale; i++) {
        struct fork_entropy e;
        if (!fork_entropy(&e)) return 0;
        h = fnv(e.after, 257, fnv(e.child, 257, fnv(e.before, 257, h)));
    }
    static unsigned char big[65537];
    fill_random(big, sizeof big);
    OUT("forkDigest", "%016lx", (unsigned long)h);
    OUT("largeDigest", "%016lx", (unsigned long)fnv(big, sizeof big, 0xcbf29ce484222325ULL));
    return 1;
}

/* vmsplice references user pages from a pipe; tee duplicates the references. */
static int t_vmsplice(void) {
    long basic = 0, alias = 0, cow = 0;
    char last[8] = "-------";
    char *pg = mmap(0, 4096, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    static char buf[4096], buf2[4096];
    for (long i = 0; i < 100 * scale; i++) {
        int a[2], b[2];
        if (pipe(a) || pipe(b)) return 0;
        char source[] = "abc", x[4] = {0}, y[4] = {0};
        struct iovec v = {source, 3};
        ssize_t s = vmsplice(a[1], &v, 1, 0), t = tee(a[0], b[1], 3, 0);
        basic += s == 3 && t == 3 && read(a[0], x, 3) == 3 && read(b[0], y, 3) == 3 && !strcmp(x, "abc") && !strcmp(y, "abc");
        /* Writing the page after vmsplice: both pipes reference it. */
        memset(pg, 'x', 4096);
        struct iovec pv = {pg, 4096};
        s = vmsplice(a[1], &pv, 1, 0);
        t = tee(a[0], b[1], 4096, 0);
        pg[0] = 'y';
        ssize_t ra = read(a[0], buf, 4096), rb = read(b[0], buf2, 4096);
        last[0] = buf[0];
        last[1] = buf2[0];
        alias += s == 4096 && t == 4096 && ra == 4096 && rb == 4096 && buf[0] == 'y' && buf2[0] == 'y' && buf[1] == 'x';
        /* Writing it while a child shares it: the write copies, the pipe keeps the old page. */
        memset(pg, 'p', 4096);
        s = vmsplice(a[1], &pv, 1, 0);
        int go[2];
        if (pipe(go)) return 0;
        pid_t c = fork();
        if (!c) { char z; _exit(read(go[0], &z, 1) == 1 && pg[0] == 'p' ? 0 : 1); }
        pg[0] = 'q';
        if (write(go[1], "g", 1) != 1) return 0;
        int st;
        waitpid(c, &st, 0);
        ra = read(a[0], buf, 4096);
        last[2] = buf[0];
        last[3] = pg[0];
        cow += s == 4096 && ra == 4096 && buf[0] == 'p' && pg[0] == 'q' && WIFEXITED(st) && WEXITSTATUS(st) == 0;
        close(go[0]);
        close(go[1]);
        close(a[0]);
        close(a[1]);
        close(b[0]);
        close(b[1]);
    }
    OUT("basic", "%ld", basic);
    OUT("alias", "%ld", alias);
    OUT("forkCopy", "%ld", cow);
    OUT("observed", "%.4s", last);
    return 1;
}

/* libgcc DWARF unwinding through recursion, a signal frame and a thread start,
 * printed as function identities rather than addresses. */
struct uw { int n; uintptr_t ip[96], start[96]; };
static _Unwind_Reason_Code uw_note(struct _Unwind_Context *c, void *p) {
    struct uw *u = p;
    if (u->n < 96) { u->ip[u->n] = _Unwind_GetIP(c); u->start[u->n] = _Unwind_GetRegionStart(c); }
    u->n++;
    return _URC_NO_REASON;
}
static uintptr_t vdso_lo, vdso_hi;
static void vdso_range(void) {
    uintptr_t base = getauxval(AT_SYSINFO_EHDR);
    if (!base) return;
    const Elf64_Ehdr *eh = (const Elf64_Ehdr *)base;
    const Elf64_Phdr *ph = (const Elf64_Phdr *)(base + eh->e_phoff);
    vdso_lo = base;
    for (int i = 0; i < eh->e_phnum; i++)
        if (ph[i].p_type == PT_LOAD && base + ph[i].p_vaddr + ph[i].p_memsz > vdso_hi) vdso_hi = base + ph[i].p_vaddr + ph[i].p_memsz;
}
static int uw_result;
static __attribute__((noipa)) int uw_walk(int depth, struct uw *u) {
    if (depth) { int n = uw_walk(depth - 1, u); asm volatile("" : "+r"(n)); return n; }
    uw_result = _Unwind_Backtrace(uw_note, u);
    return u->n;
}
static struct uw uw_sig;
static sigjmp_buf uw_jb;
static __attribute__((noipa)) void uw_handler(int s) {
    (void)s;
    memset(&uw_sig, 0, sizeof uw_sig);
    _Unwind_Backtrace(uw_note, &uw_sig);
    siglongjmp(uw_jb, 1);
}
static __attribute__((noipa)) int uw_fault(int depth, volatile int *p) {
    if (depth) { int n = uw_fault(depth - 1, p); asm volatile("" : "+r"(n)); return n; }
    *p = 1;
    return 0;
}
static int t_unwind(void);
static void *uw_thread(void *p);
int main(int argc, char **argv);
static void uw_names(const char *key, const struct uw *u) {
    const struct { uintptr_t f; const char *name; } own[] = {
        {(uintptr_t)uw_walk, "walk"}, {(uintptr_t)uw_fault, "fault"}, {(uintptr_t)uw_handler, "handler"},
        {(uintptr_t)t_unwind, "test"}, {(uintptr_t)uw_thread, "thread"}, {(uintptr_t)main, "main"}};
    char line[512];
    int len = 0, outer = 0, n = u->n < 96 ? u->n : 96;
    const char *prev = NULL;
    int reps = 0;
    for (int i = 0; i <= n; i++) {
        const char *name = NULL;
        if (i < n) {
            /* A frame without an FDE (libgcc's signal-frame fallback) keeps the previous region start. */
            if (u->ip[i] >= vdso_lo && u->ip[i] < vdso_hi) name = "vdso";
            for (size_t k = 0; k < sizeof own / sizeof own[0] && !name; k++)
                if (u->start[i] == own[k].f) name = own[k].name;
            if (!name && !u->start[i]) name = "nofde";
            if (!name) { outer++; name = "other"; }
        }
        if (i < n && prev && !strcmp(name, prev)) { reps++; continue; }
        if (prev) len += snprintf(line + len, sizeof line - len, reps > 1 ? " %s*%d" : " %s", prev, reps);
        prev = name;
        reps = 1;
    }
    OUT(key, "%d%s", u->n, len ? line : "");
}
static void *uw_thread(void *p) {
    struct uw *u = p;
    uw_walk(3, u);
    return NULL;
}
static int t_unwind(void) {
    vdso_range();
    long same = 0, depthOk = 0;
    for (long i = 0; i < 200 * scale; i++) {
        struct uw a, b;
        memset(&a, 0, sizeof a);
        memset(&b, 0, sizeof b);
        int d = 1 + (int)(next() % 24);
        int na = uw_walk(d, &a), nb = uw_walk(0, &b);
        depthOk += na - nb == d && uw_result == _URC_END_OF_STACK;
        same += a.start[a.n - 1] == b.start[b.n - 1];
    }
    struct uw fixed;
    memset(&fixed, 0, sizeof fixed);
    uw_walk(5, &fixed);
    uw_names("recursion", &fixed);
    OUT("depthOk", "%ld", depthOk);
    OUT("sameOutermost", "%ld", same);
    signal(SIGSEGV, uw_handler);
    long sigOk = 0;
    for (long i = 0; i < 50 * scale; i++) {
        if (!sigsetjmp(uw_jb, 1)) uw_fault(4, (volatile int *)(uintptr_t)8);
        int fault = 0;
        for (int k = 0; k < uw_sig.n && k < 96; k++) fault += uw_sig.start[k] == (uintptr_t)uw_fault;
        sigOk += fault == 5;
    }
    signal(SIGSEGV, SIG_DFL);
    uw_names("signal", &uw_sig);
    OUT("signalOk", "%ld", sigOk);
    struct uw tu;
    memset(&tu, 0, sizeof tu);
    pthread_t t;
    pthread_create(&t, 0, uw_thread, &tu);
    pthread_join(t, 0);
    uw_names("thread", &tu);
    return 1;
}

/* pthread key destructors that reinstall values, plus a C++-style thread_local
 * destructor, across return, pthread_exit and cancellation. */
extern int __cxa_thread_atexit_impl(void (*)(void *), void *, void *);
extern void *__dso_handle;
static pthread_key_t tk1, tk2;
static char tl_log[64];
static int tl_len, tl_calls1, tl_once;
static void tl_note(char c) { if (tl_len < 63) tl_log[tl_len++] = c; }
static void tl_d1(void *p) { (void)p; tl_note('a'); if (++tl_calls1 < 10) pthread_setspecific(tk1, (void *)1); }
static void tl_d2(void *p) { (void)p; tl_note('b'); if (!tl_once++) pthread_setspecific(tk1, (void *)2); }
static void tl_cxa(void *p) { (void)p; tl_note('c'); pthread_setspecific(tk2, (void *)3); }
static int tl_ready[2];
static void *tl_worker(void *p) {
    long mode = (long)p;
    __cxa_thread_atexit_impl(tl_cxa, NULL, &__dso_handle);
    pthread_setspecific(tk1, (void *)1);
    pthread_setspecific(tk2, (void *)1);
    if (mode == 1) pthread_exit(NULL);
    if (mode == 2) {
        if (write(tl_ready[1], "r", 1) != 1) return NULL;
        for (;;) pause();
    }
    return NULL;
}
static int t_tlsdtor(void) {
    pthread_key_create(&tk1, tl_d1);
    pthread_key_create(&tk2, tl_d2);
    if (pipe(tl_ready)) return 0;
    char logs[3][64];
    long same = 0, rounds = 30 * scale;
    for (long i = 0; i < rounds; i++) {
        for (long mode = 0; mode < 3; mode++) {
            tl_len = 0;
            tl_calls1 = 0;
            tl_once = 0;
            pthread_t t;
            pthread_create(&t, 0, tl_worker, (void *)mode);
            if (mode == 2) {
                char b;
                if (read(tl_ready[0], &b, 1) != 1) return 0;
                pthread_cancel(t);
            }
            pthread_join(t, 0);
            tl_log[tl_len] = 0;
            if (i == 0) strcpy(logs[mode], tl_log);
            same += !strcmp(logs[mode], tl_log);
        }
    }
    OUT("return", "%s", logs[0]);
    OUT("pthreadExit", "%s", logs[1]);
    OUT("cancel", "%s", logs[2]);
    OUT("same", "%ld/%ld", same, 3 * rounds);
    return 1;
}

/* A static-PIE helper sets ADDR_NO_RANDOMIZE and re-executes itself through
 * /proc/self/exe; each pass checks its own relocation and personality. */
static int staticpie(int show_address) {
    char path[4096];
    const char *slash = strrchr(argv0, '/');
    if (slash) snprintf(path, sizeof path, "%.*s/cfzpie", (int)(slash - argv0), argv0);
    else snprintf(path, sizeof path, "cfzpie");
    if (access(path, X_OK)) { puts("UNSUPPORTED cfzpie"); return 0; }
    int fd[2];
    if (pipe(fd)) return 0;
    long execs = 0, sameAddr = 0, good = 0, rounds = 3 * scale;
    char first[2][256] = {"", ""};
    for (long round = 0; round < rounds; round++) {
        pid_t c = fork();
        if (!c) {
            dup2(fd[1], 1);
            char *args[] = {path, "start", NULL};
            execve(path, args, environ);
            _exit(127);
        }
        int st;
        waitpid(c, &st, 0);
        char buf[4096];
        int fl = fcntl(fd[0], F_GETFL);
        fcntl(fd[0], F_SETFL, fl | O_NONBLOCK);
        ssize_t n = read(fd[0], buf, sizeof buf - 1);
        fcntl(fd[0], F_SETFL, fl);
        buf[n > 0 ? n : 0] = 0;
        if (!strncmp(buf, "UNSUPPORTED", 11)) { fputs(buf, stdout); return 0; }
        char addr[2][64] = {"", ""}, rest[2][256] = {"", ""};
        int passes = 0;
        for (char *line = strtok(buf, "\n"); line; line = strtok(NULL, "\n")) {
            int pass;
            char *a = strstr(line, " addr ");
            if (sscanf(line, "pass %d", &pass) != 1 || pass < 1 || pass > 2 || !a) {
                if (round == 0) OUT("line", "%s", line);
                continue;
            }
            snprintf(addr[pass - 1], sizeof addr[0], "%s", a + 6);
            snprintf(rest[pass - 1], sizeof rest[0], "%.*s", (int)(a - line), line);
            passes++;
        }
        execs += passes;
        sameAddr += passes == 2 && !strcmp(addr[0], addr[1]);
        good += WIFEXITED(st) && WEXITSTATUS(st) == 0;
        if (round == 0) {
            memcpy(first, rest, sizeof first);
            if (show_address) OUT("address", "%s", addr[0]);
        }
    }
    OUT("first", "%s", first[0]);
    OUT("second", "%s", first[1]);
    OUT("execs", "%ld", execs);
    OUT("sameAddress", "%ld", sameAddr);
    OUT("statusOk", "%ld", good);
    return 1;
}
static int t_staticpie(void) { return staticpie(0); }
static int t_staticpieaddr(void) { return staticpie(1); }

/* ---- determinism escapes ---- */

/* Minimal vDSO symbol lookup (SysV hash; the vDSO is fully mapped). */
static void *vdso_sym(const char *name) {
    uintptr_t base = getauxval(AT_SYSINFO_EHDR);
    if (!base) return NULL;
    const Elf64_Ehdr *eh = (const Elf64_Ehdr *)base;
    const Elf64_Phdr *ph = (const Elf64_Phdr *)(base + eh->e_phoff);
    uintptr_t load = 0;
    const Elf64_Dyn *dyn = NULL;
    int have = 0;
    for (int i = 0; i < eh->e_phnum; i++) {
        if (ph[i].p_type == PT_LOAD && !have) { load = base + ph[i].p_offset - ph[i].p_vaddr; have = 1; }
        if (ph[i].p_type == PT_DYNAMIC) dyn = (const Elf64_Dyn *)(base + ph[i].p_offset);
    }
    if (!dyn) return NULL;
    const Elf64_Sym *sym = NULL;
    const char *str = NULL;
    const Elf32_Word *hash = NULL;
    for (; dyn->d_tag != DT_NULL; dyn++) {
        if (dyn->d_tag == DT_SYMTAB) sym = (const Elf64_Sym *)(load + dyn->d_un.d_ptr);
        if (dyn->d_tag == DT_STRTAB) str = (const char *)(load + dyn->d_un.d_ptr);
        if (dyn->d_tag == DT_HASH) hash = (const Elf32_Word *)(load + dyn->d_un.d_ptr);
    }
    if (!sym || !str || !hash) return NULL;
    for (Elf32_Word i = 0; i < hash[1]; i++)
        if (sym[i].st_shndx != SHN_UNDEF && ELF64_ST_TYPE(sym[i].st_info) <= STT_FUNC && !strcmp(str + sym[i].st_name, name))
            return (void *)(load + sym[i].st_value);
    return NULL;
}

static sigjmp_buf ill_jb;
static void ill_jump(int s) { siglongjmp(ill_jb, s); }
/* Runs fn and returns 0, or the signal (SIGILL/SIGSEGV) it raised. */
static int guarded(void (*fn)(uint64_t *), uint64_t *v) {
    struct sigaction a, oi, os;
    memset(&a, 0, sizeof a);
    a.sa_handler = ill_jump;
    sigemptyset(&a.sa_mask);
    sigaction(SIGILL, &a, &oi);
    sigaction(SIGSEGV, &a, &os);
    int sig = sigsetjmp(ill_jb, 1);
    if (!sig) fn(v);
    sigaction(SIGILL, &oi, 0);
    sigaction(SIGSEGV, &os, 0);
    return sig;
}
static const char *sigword(int s) { return s == 0 ? "ok" : s == SIGILL ? "sigill" : s == SIGSEGV ? "sigsegv" : "signal"; }

static void rd_cntfrq(uint64_t *v) { asm volatile("mrs %0, cntfrq_el0" : "=r"(*v)::"memory"); }
static void rd_cntvct(uint64_t *v) { asm volatile("isb\nmrs %0, cntvct_el0" : "=r"(*v)::"memory"); }
static void rd_cntvctss(uint64_t *v) { asm volatile("mrs %0, s3_3_c14_c0_6" : "=r"(*v)::"memory"); }
static void rd_cntpct(uint64_t *v) { asm volatile("isb\nmrs %0, cntpct_el0" : "=r"(*v)::"memory"); }
static void rd_cntpctss(uint64_t *v) { asm volatile("mrs %0, s3_3_c14_c0_5" : "=r"(*v)::"memory"); }
static uint64_t mono_ns(clockid_t c) {
    struct timespec t;
    clock_gettime(c, &t);
    return (uint64_t)t.tv_sec * 1000000000ULL + (uint64_t)t.tv_nsec;
}
static void fixed_work(long n) { for (volatile long i = 0; i < n; i++); }

/* replay-only: the architectural counters, read directly. */
static int t_counters(void) {
    uint64_t freq = 0;
    int fs = guarded(rd_cntfrq, &freq);
    OUT("cntfrq", "%s %lu", sigword(fs), (unsigned long)freq);
    struct { const char *name; void (*fn)(uint64_t *); } c[] = {
        {"cntvct", rd_cntvct}, {"cntvctss", rd_cntvctss}, {"cntpct", rd_cntpct}, {"cntpctss", rd_cntpctss}};
    for (size_t k = 0; k < 4; k++) {
        uint64_t v;
        int s = guarded(c[k].fn, &v);
        if (s) { OUT(c[k].name, "%s", sigword(s)); continue; }
        long mono = 0, n = 200 * scale;
        uint64_t prev = 0;
        for (long i = 0; i < n; i++) {
            c[k].fn(&v);
            mono += v >= prev;
            prev = v;
            fixed_work(100);
        }
        uint64_t a, b, m0 = mono_ns(CLOCK_MONOTONIC);
        c[k].fn(&a);
        fixed_work(2000000);
        c[k].fn(&b);
        uint64_t m1 = mono_ns(CLOCK_MONOTONIC);
        double ns = freq ? (double)(b - a) * 1e9 / (double)freq : 0, mns = (double)(m1 - m0);
        int agree = freq && ns <= mns * 1.2 + 2e6 && ns >= mns * 0.5 - 2e6;
        OUT(c[k].name, "ok monotonic %ld/%ld advanced %d agreesWithMonotonic %d delta %lu", mono, n, b > a, agree,
            (unsigned long)(b - a));
    }
    /* Self-synchronizing reads, when readable, interleave with ordinary ones. */
    uint64_t v;
    if (!guarded(rd_cntvctss, &v)) {
        long ordered = 0;
        for (long i = 0; i < 200 * scale; i++) {
            uint64_t x, y, z;
            rd_cntvct(&x);
            rd_cntvctss(&y);
            rd_cntvct(&z);
            ordered += x <= y && y <= z;
        }
        OUT("vctssOrdered", "%ld", ordered);
    }
    return 1;
}

/* det: vDSO clock helpers agree with syscalls, read in between them. */
typedef int (*vgettime_t)(clockid_t, struct timespec *);
static int t_vdsoclock(void) {
    vgettime_t vg = (vgettime_t)vdso_sym("__kernel_clock_gettime");
    int (*vres)(clockid_t, struct timespec *) = (int (*)(clockid_t, struct timespec *))vdso_sym("__kernel_clock_getres");
    int (*vtod)(struct timeval *, void *) = (int (*)(struct timeval *, void *))vdso_sym("__kernel_gettimeofday");
    if (!vg || !vres || !vtod) { puts("UNSUPPORTED vdso"); return 0; }
    struct { clockid_t id; const char *name; } clocks[] = {
        {CLOCK_REALTIME, "realtime"}, {CLOCK_MONOTONIC, "monotonic"}, {CLOCK_MONOTONIC_RAW, "raw"},
        {CLOCK_REALTIME_COARSE, "realtimeCoarse"}, {CLOCK_MONOTONIC_COARSE, "monotonicCoarse"},
        {CLOCK_BOOTTIME, "boottime"}, {CLOCK_TAI, "tai"}, {CLOCK_PROCESS_CPUTIME_ID, "processCpu"}};
    long n = 300 * scale;
    for (size_t k = 0; k < sizeof clocks / sizeof clocks[0]; k++) {
        long ordered = 0, libc = 0;
        for (long i = 0; i < n; i++) {
            struct timespec s1, v, s2, l, s3;
            syscall(SYS_clock_gettime, clocks[k].id, &s1);
            int r = vg(clocks[k].id, &v);
            syscall(SYS_clock_gettime, clocks[k].id, &s2);
            clock_gettime(clocks[k].id, &l);
            syscall(SYS_clock_gettime, clocks[k].id, &s3);
            uint64_t a = s1.tv_sec * 1000000000ULL + s1.tv_nsec, b = v.tv_sec * 1000000000ULL + v.tv_nsec,
                     c = s2.tv_sec * 1000000000ULL + s2.tv_nsec, d = l.tv_sec * 1000000000ULL + l.tv_nsec,
                     e = s3.tv_sec * 1000000000ULL + s3.tv_nsec;
            ordered += r == 0 && a <= b && b <= c;
            libc += c <= d && d <= e;
        }
        struct timespec rs, rv;
        int sr = (int)syscall(SYS_clock_getres, clocks[k].id, &rs), vr = vres(clocks[k].id, &rv);
        OUT(clocks[k].name, "ordered %ld/%ld libc %ld/%ld getresEqual %d", ordered, n, libc, n,
            sr == vr && rs.tv_sec == rv.tv_sec && rs.tv_nsec == rv.tv_nsec);
    }
    long tod = 0;
    for (long i = 0; i < n; i++) {
        struct timespec s1, s2;
        struct timeval tv;
        syscall(SYS_clock_gettime, CLOCK_REALTIME, &s1);
        int r = vtod(&tv, NULL);
        syscall(SYS_clock_gettime, CLOCK_REALTIME, &s2);
        uint64_t a = s1.tv_sec * 1000000ULL + s1.tv_nsec / 1000, b = tv.tv_sec * 1000000ULL + tv.tv_usec,
                 c = s2.tv_sec * 1000000ULL + s2.tv_nsec / 1000;
        tod += r == 0 && a <= b && b <= c;
    }
    OUT("gettimeofday", "ordered %ld/%ld", tod, n);
    errno = 0;
    struct timespec bad;
    OUT("badClock", "%d %d", vg((clockid_t)1234, &bad), (int)syscall(SYS_clock_gettime, 1234, &bad));
    return 1;
}

/* replay-only: direct vDSO getrandom (Linux 6.11+), negotiated as the kernel documents. */
struct vgetrandom_params { uint32_t size_of_opaque_state, mmap_prot, mmap_flags, reserved[13]; };
typedef ssize_t (*vgetrandom_t)(void *, size_t, unsigned, void *, size_t);
static int t_vdsogetrandom(void) {
    vgetrandom_t vr = (vgetrandom_t)vdso_sym("__kernel_getrandom");
    OUT("symbol", "%d", vr != NULL);
    if (!vr) return 1;
    struct vgetrandom_params params;
    memset(&params, 0, sizeof params);
    ssize_t q = vr(NULL, 0, 0, &params, ~0UL);
    OUT("query", "%zd %u", q, params.size_of_opaque_state);
    if (q != 0 || !params.size_of_opaque_state) return 1;
    size_t len = (params.size_of_opaque_state + 4095) & ~4095UL;
    void *state = mmap(0, len, params.mmap_prot, params.mmap_flags, -1, 0);
    if (state == MAP_FAILED) { OUT("stateMap", "%s", strerror(errno)); return 1; }
    unsigned char a[257], b[257], c[257];
    long ok = 0, distinct = 0;
    uint64_t h = 0xcbf29ce484222325ULL;
    for (long i = 0; i < 10 * scale; i++) {
        ok += vr(a, sizeof a, 0, state, params.size_of_opaque_state) == (ssize_t)sizeof a;
        int fd[2];
        if (pipe(fd)) return 0;
        pid_t p = fork();
        if (!p) {
            ssize_t r = vr(b, sizeof b, 0, state, params.size_of_opaque_state);
            _exit(r == (ssize_t)sizeof b && write(fd[1], b, sizeof b) == sizeof b ? 0 : 1);
        }
        int st;
        waitpid(p, &st, 0);
        if (read(fd[0], b, sizeof b) != sizeof b) memset(b, 0, sizeof b);
        ok += vr(c, sizeof c, 0, state, params.size_of_opaque_state) == (ssize_t)sizeof c;
        distinct += memcmp(a, b, sizeof a) && memcmp(b, c, sizeof b) && memcmp(a, c, sizeof a);
        h = fnv(c, sizeof c, fnv(b, sizeof b, fnv(a, sizeof a, h)));
        close(fd[0]);
        close(fd[1]);
    }
    OUT("calls", "%ld", ok);
    OUT("forkDistinct", "%ld", distinct);
    OUT("digest", "%016lx", (unsigned long)h);
    return 1;
}

/* replay-only: RNDR/RNDRRS, only when advertised. */
static int t_rndr(void) {
    if (!(getauxval(AT_HWCAP2) & (1UL << 16))) { puts("UNSUPPORTED rng"); return 0; }
    uint64_t h = 0xcbf29ce484222325ULL;
    long ok[2] = {0, 0};
    for (int k = 0; k < 2; k++) {
        for (int i = 0; i < 64; i++) {
            uint64_t v, nz;
            if (k == 0) asm volatile("mrs %0, s3_3_c2_c4_0\nmrs %1, nzcv" : "=r"(v), "=r"(nz)::"cc");
            else asm volatile("mrs %0, s3_3_c2_c4_1\nmrs %1, nzcv" : "=r"(v), "=r"(nz)::"cc");
            if (nz & (1UL << 30)) continue; /* Z set: no entropy this time */
            ok[k]++;
            h = fnv((unsigned char *)&v, 8, h);
        }
    }
    OUT("rndr", "%ld", ok[0]);
    OUT("rndrrs", "%ld", ok[1]);
    OUT("digest", "%016lx", (unsigned long)h);
    return 1;
}

/* replay-only: TPIDRRO_EL0 is zero natively; Seedbox publishes an epoch there on every return to EL0. */
static uint64_t rd_tpidrro(void) { uint64_t v; asm volatile("mrs %0, tpidrro_el0" : "=r"(v)::"memory"); return v; }
static void wr_tpidrro(uint64_t *v) { asm volatile("msr tpidrro_el0, %0" ::"r"(*v) : "memory"); }
static volatile sig_atomic_t tp_seen;
static void tp_handler(int s) { (void)s; tp_seen++; }
static int t_tpidrro(void) {
    uint64_t start = rd_tpidrro();
    uint64_t a = rd_tpidrro();
    syscall(SYS_getpid);
    uint64_t b = rd_tpidrro();
    long nondecreasing = 0, n = 100 * scale;
    uint64_t prev = b, total0 = b;
    for (long i = 0; i < n; i++) {
        syscall(SYS_getppid);
        uint64_t v = rd_tpidrro();
        nondecreasing += v >= prev;
        prev = v;
    }
    uint64_t c = rd_tpidrro();
    fixed_work(1000);
    uint64_t d = rd_tpidrro();
    signal(SIGUSR1, tp_handler);
    uint64_t e = rd_tpidrro();
    raise(SIGUSR1);
    uint64_t f = rd_tpidrro();
    uint64_t w = 5;
    int ws = guarded(wr_tpidrro, &w);
    OUT("initialZero", "%d", start == 0);
    OUT("getpidDelta", "%lu", (unsigned long)(b - a));
    OUT("syscallsDelta", "%lu %ld/%ld", (unsigned long)(c - total0), nondecreasing, n);
    OUT("computeDelta", "%lu", (unsigned long)(d - c));
    OUT("signalDelta", "%lu %d", (unsigned long)(f - e), (int)tp_seen);
    OUT("write", "%s", sigword(ws));
    return 1;
}

/* WFE/WFI/WFET/WFIT complete within a bounded number of executions. */
static void do_wfe(uint64_t *n) { for (uint64_t i = 0; i < *n; i++) asm volatile("sevl\nwfe" ::: "memory"); }
static void do_wfi(uint64_t *n) { for (uint64_t i = 0; i < *n; i++) asm volatile("wfi" ::: "memory"); }
static uint64_t wait_ticks; /* one millisecond of counter ticks */
static void do_wfet(uint64_t *n) {
    for (uint64_t i = 0; i < *n; i++) {
        uint64_t now;
        rd_cntvct(&now);
        register uint64_t deadline asm("x9") = now + wait_ticks;
        asm volatile(".inst 0xd5031009" ::"r"(deadline) : "memory"); /* wfet x9 */
    }
}
static void do_wfit(uint64_t *n) {
    for (uint64_t i = 0; i < *n; i++) {
        uint64_t now;
        rd_cntvct(&now);
        register uint64_t deadline asm("x9") = now + wait_ticks;
        asm volatile(".inst 0xd5031029" ::"r"(deadline) : "memory"); /* wfit x9 */
    }
}
static int t_waits(void) {
    struct { const char *name; void (*fn)(uint64_t *); int need_wfxt; } w[] = {
        {"wfe", do_wfe, 0}, {"wfi", do_wfi, 0}, {"wfet", do_wfet, 1}, {"wfit", do_wfit, 1}};
    int wfxt = (getauxval(AT_HWCAP2) >> 31) & 1;
    uint64_t freq = 0;
    guarded(rd_cntfrq, &freq);
    wait_ticks = freq / 1000;
    for (size_t k = 0; k < 4; k++) {
        if (w[k].need_wfxt && !wfxt) { OUT(w[k].name, "%s", "absent"); continue; }
        uint64_t n = 64 * scale;
        uint64_t t0 = mono_ns(CLOCK_MONOTONIC);
        int s = guarded(w[k].fn, &n);
        uint64_t t1 = mono_ns(CLOCK_MONOTONIC);
        OUT(w[k].name, "%s bounded %d", sigword(s), t1 - t0 < 5000000000ULL);
    }
    return 1;
}

/* replay-only: PR_SET_TSC on arm64 controls EL0 counter access. */
static int t_prtsc(void) {
    int fd[2];
    if (pipe(fd)) return 0;
    pid_t c = fork();
    if (!c) {
        char line[512];
        int len = 0, mode = -1;
        errno = 0;
        int g = prctl(25 /* PR_GET_TSC */, &mode, 0, 0, 0);
        len += snprintf(line + len, sizeof line - len, "get %s %d\n", g ? strerror(errno) : "ok", mode);
        uint64_t before = 0, v = 0;
        rd_cntvct(&before);
        errno = 0;
        int s = prctl(26 /* PR_SET_TSC */, 2 /* PR_TSC_SIGSEGV */, 0, 0, 0);
        int r = guarded(rd_cntvct, &v);
        len += snprintf(line + len, sizeof line - len, "sigsegvMode %s %s\n", s ? strerror(errno) : "ok", sigword(r));
        errno = 0;
        s = prctl(26, 1 /* PR_TSC_ENABLE */, 0, 0, 0);
        r = guarded(rd_cntvct, &v);
        len += snprintf(line + len, sizeof line - len, "enableMode %s %s after %d\n", s ? strerror(errno) : "ok", sigword(r), v >= before);
        g = prctl(25, &mode, 0, 0, 0);
        len += snprintf(line + len, sizeof line - len, "getAgain %d %d\n", g, mode);
        _exit(write(fd[1], line, len) == len ? 0 : 1);
    }
    waitpid(c, 0, 0);
    char buf[512];
    ssize_t n = read(fd[0], buf, sizeof buf - 1);
    buf[n > 0 ? n : 0] = 0;
    fputs(buf, stdout);
    return 1;
}

/* det: pinning to the current CPU holds for new threads; absent CPUs are rejected. */
static int af_cpu;
static void *af_worker(void *p) {
    (void)p;
    cpu_set_t s;
    CPU_ZERO(&s);
    int ok = sched_getaffinity(0, sizeof s, &s) == 0 && CPU_COUNT(&s) == 1 && CPU_ISSET(af_cpu, &s);
    for (int i = 0; i < 1000; i++) { ok &= sched_getcpu() == af_cpu; sched_yield(); }
    return (void *)(long)ok;
}
static int t_affinity(void) {
    cpu_set_t orig, one;
    if (sched_getaffinity(0, sizeof orig, &orig)) return 0;
    af_cpu = sched_getcpu();
    OUT("currentInMask", "%d", CPU_ISSET(af_cpu, &orig));
    CPU_ZERO(&one);
    CPU_SET(af_cpu, &one);
    OUT("pin", "%d", sched_setaffinity(0, sizeof one, &one));
    long ok = 0;
    for (int i = 0; i < 4 * scale; i++) {
        pthread_t t;
        void *r;
        pthread_create(&t, 0, af_worker, 0);
        pthread_join(t, &r);
        ok += (long)r;
    }
    OUT("threadsOnCpu", "%ld", ok);
    pid_t c = fork();
    if (!c) _exit(sched_getcpu() == af_cpu ? 0 : 1);
    int st;
    waitpid(c, &st, 0);
    OUT("childOnCpu", "%d", WIFEXITED(st) && WEXITSTATUS(st) == 0);
    cpu_set_t *big = CPU_ALLOC(4096);
    size_t bs = CPU_ALLOC_SIZE(4096);
    CPU_ZERO_S(bs, big);
    CPU_SET_S(4000, bs, big);
    errno = 0;
    int r = sched_setaffinity(0, bs, big);
    OUT("absentCpu", "%d %s", r, strerror(errno));
    CPU_FREE(big);
    sched_setaffinity(0, sizeof orig, &orig);
    return 1;
}

/* replay-only: exclusive-store failures with and without a syscall inside the reservation. */
static int t_exclusive(void) {
    static uint64_t word __attribute__((aligned(64)));
    long plain = 0, across = 0, n = 2000 * scale;
    for (long i = 0; i < n; i++) {
        uint64_t v;
        unsigned fail;
        asm volatile("ldxr %0, [%2]\nadd %0, %0, #1\nstxr %w1, %0, [%2]" : "=&r"(v), "=&r"(fail) : "r"(&word) : "memory");
        plain += fail != 0;
    }
    for (long i = 0; i < n; i++) {
        register uint64_t x0 asm("x0");
        register uint64_t x8 asm("x8") = SYS_getpid;
        uint64_t v;
        unsigned fail;
        asm volatile("ldxr %0, [%3]\nadd %0, %0, #1\nsvc #0\nstxr %w1, %0, [%3]"
                     : "=&r"(v), "=&r"(fail), "=r"(x0) : "r"(&word), "r"(x8) : "memory");
        across += fail != 0;
    }
    OUT("plainFailures", "%ld/%ld", plain, n);
    OUT("acrossSyscallFailures", "%ld/%ld", across, n);
    return 1;
}

/* What this host offers: syscalls, files and CPU features. Differences from a
 * native run describe the guest; they are reported, not failures. */
static void cap(const char *name, long r) { OUT(name, "%s", r < 0 ? strerror(errno) : "ok"); }

/* Runs fn in a child; returns its result (0 or an errno), or 128+signal. */
static int in_child(int (*fn)(void)) {
    pid_t p = fork();
    if (!p) _exit(fn());
    int st;
    if (waitpid(p, &st, 0) < 0) return -1;
    return WIFEXITED(st) ? WEXITSTATUS(st) : 128 + WTERMSIG(st);
}
static const char *child_word(int r) {
    static char buf[64];
    if (r == 0) return "ok";
    if (r >= 128) { snprintf(buf, sizeof buf, "signal %d", r - 128); return buf; }
    return strerror(r);
}
static int file_has(const char *path, const char *needle) {
    static char buf[1 << 20];
    int fd = open(path, O_RDONLY);
    if (fd < 0) return -1;
    size_t len = 0;
    ssize_t n;
    while (len < sizeof buf - 1 && (n = read(fd, buf + len, sizeof buf - 1 - len)) > 0) len += (size_t)n;
    close(fd);
    buf[len] = 0;
    return strstr(buf, needle) != NULL;
}
static int c_pac_reset(void) {
    /* Reset keys and exit without returning through any signed frame. */
    register long x0 asm("x0") = 54 /* PR_PAC_RESET_KEYS */;
    register long x1 asm("x1") = 0, x2 asm("x2") = 0, x3 asm("x3") = 0, x4 asm("x4") = 0;
    register long x8 asm("x8") = SYS_prctl;
    asm volatile("svc #0\nneg x0, x0\nmov x8, #94\nsvc #0" : "+r"(x0) : "r"(x1), "r"(x2), "r"(x3), "r"(x4), "r"(x8) : "memory");
    return 0;
}
static int ns_flag;
static int c_unshare(void) { return unshare(ns_flag) ? errno : 0; }
static int c_aarch32(void) {
    unsigned char elf[256] = {0x7f, 'E', 'L', 'F', 1, 1, 1};
    Elf32_Ehdr *h = (Elf32_Ehdr *)elf;
    Elf32_Phdr *p = (Elf32_Phdr *)(elf + sizeof *h);
    h->e_type = ET_EXEC; h->e_machine = EM_ARM; h->e_version = 1; h->e_entry = 0x10054;
    h->e_phoff = sizeof *h; h->e_ehsize = sizeof *h; h->e_phentsize = sizeof *p; h->e_phnum = 1;
    p->p_type = PT_LOAD; p->p_vaddr = p->p_paddr = 0x10000; p->p_filesz = p->p_memsz = sizeof elf;
    p->p_flags = PF_R | PF_X; p->p_align = 0x1000;
    int fd = memfd_create("armhf", 0);
    if (fd < 0 || write(fd, elf, sizeof elf) != sizeof elf) return errno;
    char *argv[] = {"armhf", NULL};
    fexecve(fd, argv, environ);
    return errno;
}
static void rd_pmccntr(uint64_t *v) { asm volatile("mrs %0, pmccntr_el0" : "=r"(*v)::"memory"); }
static void bti_call(uint64_t *v) { *v = (uint64_t)((int (*)(void))(uintptr_t)*v)(); }
static void capabilities_more(void) {
    unsigned long hw2 = getauxval(AT_HWCAP2);
    const struct { int bit; const char *name; } h2[] = {{1, "sve2"}, {16, "rng"}, {17, "bti"}, {18, "mte"}, {19, "ecv"},
                                                         {22, "mte3"}, {23, "sme"}, {31, "wfxt"}};
    for (size_t i = 0; i < sizeof h2 / sizeof h2[0]; i++) OUT("hwcap2", "%s %d", h2[i].name, (int)((hw2 >> h2[i].bit) & 1));
    struct { int op; const char *name; } vl[] = {{51, "sveGetVl"}, {64, "smeGetVl"}};
    for (int i = 0; i < 2; i++) {
        errno = 0;
        int r = prctl(vl[i].op, 0, 0, 0, 0);
        if (r < 0) OUT("prctl", "%s %s", vl[i].name, strerror(errno));
        else OUT("prctl", "%s %d", vl[i].name, r);
    }
    int oldtag = prctl(56 /* PR_GET_TAGGED_ADDR_CTRL */, 0, 0, 0, 0);
    errno = 0;
    int mte = prctl(55, 1UL | (1UL << 1) /* ENABLE | MTE_TCF_SYNC */, 0, 0, 0);
    OUT("prctl", "mteTcfSync %s", mte < 0 ? strerror(errno) : "ok");
    if (oldtag >= 0) prctl(55, (unsigned long)oldtag, 0, 0, 0);
    OUT("prctl", "pacResetKeys %s", child_word(in_child(c_pac_reset)));
    uint64_t v = 0;
    OUT("pmccntr", "%s", sigword(guarded(rd_pmccntr, &v)));
    if (hw2 & (1UL << 17)) {
        uint32_t *code = mmap(0, 4096, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
        emit_const(code, 5);                    /* no landing pad */
        code[16] = 0xd503245f;                  /* bti c */
        emit_const(code + 17, 6);
        __builtin___clear_cache((char *)code, (char *)code + 128);
        int prot = mprotect(code, 4096, PROT_READ | PROT_EXEC | 0x10 /* PROT_BTI */);
        uint64_t bare = (uintptr_t)code, pad = (uintptr_t)(code + 16);
        int sb = guarded(bti_call, &bare), sp = guarded(bti_call, &pad);
        OUT("bti", "protBti %s bare %s landing %s", prot ? strerror(errno) : "ok", sigword(sb), sigword(sp));
        munmap(code, 4096);
    }
    struct perf_event_attr pa;
    memset(&pa, 0, sizeof pa);
    pa.type = PERF_TYPE_SOFTWARE; pa.size = sizeof pa; pa.config = PERF_COUNT_SW_CPU_CLOCK;
    pa.disabled = 1; pa.exclude_kernel = 1; pa.exclude_hv = 1;
    long r = syscall(SYS_perf_event_open, &pa, 0, -1, -1, 0);
    OUT("syscall", "perf_sw_cpu_clock %s", r < 0 ? strerror(errno) : "ok");
    if (r >= 0) close((int)r);
    r = syscall(SYS_userfaultfd, O_CLOEXEC | O_NONBLOCK | 1 /* UFFD_USER_MODE_ONLY */);
    OUT("syscall", "userfaultfd_user_mode %s", r < 0 ? strerror(errno) : "ok");
    if (r >= 0) close((int)r);
    union bpf_attr ba;
    memset(&ba, 0, sizeof ba);
    ba.map_type = BPF_MAP_TYPE_ARRAY; ba.key_size = 4; ba.value_size = 4; ba.max_entries = 1;
    r = syscall(SYS_bpf, BPF_MAP_CREATE, &ba, sizeof ba);
    OUT("syscall", "bpf_map_create %s", r < 0 ? strerror(errno) : "ok");
    if (r >= 0) close((int)r);
    r = syscall(SYS_keyctl, 0 /* KEYCTL_GET_KEYRING_ID */, -2 /* KEY_SPEC_PROCESS_KEYRING */, 0);
    OUT("syscall", "keyctl %s", r < 0 ? strerror(errno) : "ok");
    unsigned long ctx = 0;
    r = syscall(SYS_io_setup, 1, &ctx);
    OUT("syscall", "io_setup %s", r < 0 ? strerror(errno) : "ok");
    if (r == 0) syscall(SYS_io_destroy, ctx);
    r = syscall(SYS_kcmp, getpid(), getpid(), 2 /* KCMP_FILES */, 0, 0);
    OUT("syscall", "kcmp_files %s", r < 0 ? strerror(errno) : "ok");
    unsigned int mmsize = 0;
    r = prctl(35 /* PR_SET_MM */, 14 /* PR_SET_MM_MAP_SIZE */, &mmsize, 0, 0);
    OUT("prctl", "setMmMapSize %s", r < 0 ? strerror(errno) : "ok");
    r = syscall(SYS_finit_module, -1, "", 0);
    OUT("syscall", "finit_module %s", r < 0 ? strerror(errno) : "ok");
    OUT("exec", "aarch32 %s", child_word(in_child(c_aarch32)));
    uint32_t fw = 0;
    r = syscall(SYS_futex, &fw, FUTEX_UNLOCK_PI, 0, 0, 0, 0);
    OUT("syscall", "futex_unlock_pi_unowned %s", r < 0 ? strerror(errno) : "ok");
    char pvbuf[8], pvsrc[8] = "abcdefg";
    struct iovec pvl = {pvbuf, 8}, pvr = {pvsrc, 8};
    r = process_vm_readv(getpid(), &pvl, 1, &pvr, 1, 0);
    OUT("syscall", "process_vm_readv %s", r < 0 ? strerror(errno) : "ok");
    struct { int flag; const char *name; } ns[] = {{CLONE_NEWNS, "mnt"}, {CLONE_NEWUTS, "uts"}, {CLONE_NEWIPC, "ipc"},
                                                   {CLONE_NEWNET, "net"}, {CLONE_NEWPID, "pid"}, {CLONE_NEWCGROUP, "cgroup"},
                                                   {0x80 /* CLONE_NEWTIME */, "time"}};
    for (size_t i = 0; i < sizeof ns / sizeof ns[0]; i++) {
        ns_flag = ns[i].flag;
        int plain = in_child(c_unshare);
        ns_flag = ns[i].flag | CLONE_NEWUSER;
        int user = in_child(c_unshare);
        OUT("unshare", "%s %s", ns[i].name, child_word(plain));
        OUT("unshare", "user+%s %s", ns[i].name, child_word(user));
    }
    const char *fsnames[] = {"cgroup2", "cgroup", "hugetlbfs", "tmpfs", "proc", "mqueue", "bpf"};
    for (size_t i = 0; i < sizeof fsnames / sizeof fsnames[0]; i++) {
        char needle[32];
        snprintf(needle, sizeof needle, "\t%s\n", fsnames[i]);
        OUT("filesystem", "%s %d", fsnames[i], file_has("/proc/filesystems", needle));
    }
    char task[64];
    snprintf(task, sizeof task, "/proc/self/task/%ld/children", (long)syscall(SYS_gettid));
    const char *paths[] = {"/proc/self/cgroup", "/sys/fs/cgroup/cgroup.controllers", "/proc/self/ns/time",
                           "/proc/self/ns/pid", "/proc/self/ns/user", "/sys/kernel/mm/transparent_hugepage",
                           "/proc/swaps", "/proc/pressure/cpu", "/proc/kcore", task, "/proc/self/smaps_rollup",
                           "/proc/self/clear_refs", "/proc/self/pagemap", "/dev/kvm", "/dev/hwrng", "/dev/rtc0",
                           "/dev/ptp0", "/dev/dri/renderD128", "/sys/bus/pci/devices", "/proc/self/ns/cgroup"};
    for (size_t i = 0; i < sizeof paths / sizeof paths[0]; i++) {
        int fd = open(paths[i], O_RDONLY);
        OUT("open", "%s %s", i == 9 ? "/proc/self/task/TID/children" : paths[i], fd < 0 ? strerror(errno) : "ok");
        if (fd >= 0) close(fd);
    }
    OUT("swapTotalZero", "%d", file_has("/proc/meminfo", "SwapTotal:              0 kB"));
    char *huge = mmap(0, 8 << 20, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    char *aligned = (char *)(((uintptr_t)huge + (2 << 20) - 1) & ~((uintptr_t)(2 << 20) - 1));
    cap("madvHugepage", madvise(aligned, 4 << 20, 14 /* MADV_HUGEPAGE */));
    cap("madvPageout", madvise(aligned, 4 << 20, 21 /* MADV_PAGEOUT */));
    munmap(huge, 8 << 20);
    void *ht = mmap(0, 2 << 20, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS | MAP_HUGETLB, -1, 0);
    OUT("mapHugetlb", "%s", ht == MAP_FAILED ? strerror(errno) : "ok");
    if (ht != MAP_FAILED) munmap(ht, 2 << 20);
    char *named = mmap(0, 4096, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    r = prctl(0x53564d41 /* PR_SET_VMA */, 0 /* PR_SET_VMA_ANON_NAME */, named, 4096, "cfzprobe");
    OUT("prctl", "setVmaAnonName %s %d", r < 0 ? strerror(errno) : "ok", file_has("/proc/self/maps", "[anon:cfzprobe]"));
    munmap(named, 4096);
    cpu_set_t cs;
    CPU_ZERO(&cs);
    sched_getaffinity(0, sizeof cs, &cs);
    OUT("affinityCpus", "%d", CPU_COUNT(&cs));
    OUT("sched_getcpu", "%s", sched_getcpu() >= 0 ? "ok" : strerror(errno)); /* the CPU depends on pinning */
    OUT("interpreter", "%d", getauxval(AT_BASE) != 0);
    const char *vsyms[] = {"__kernel_clock_gettime", "__kernel_gettimeofday", "__kernel_clock_getres",
                           "__kernel_rt_sigreturn", "__kernel_getrandom"};
    for (size_t i = 0; i < sizeof vsyms / sizeof vsyms[0]; i++) OUT("vdso", "%s %d", vsyms[i], vdso_sym(vsyms[i]) != NULL);
    OUT("tpidrro", "%lx", (unsigned long)rd_tpidrro());
}
static int t_capabilities(void) {
    unsigned long hw = getauxval(AT_HWCAP), hw2 = getauxval(AT_HWCAP2);
    const char *bits[] = {"fp", "asimd", "evtstrm", "aes", "pmull", "sha1", "sha2", "crc32", "atomics", "fphp",
                          "asimdhp", "cpuid", "asimdrdm", "jscvt", "fcma", "lrcpc", "dcpop", "sha3", "sm3", "sm4",
                          "asimddp", "sha512", "sve", "asimdfhm", "dit", "uscat", "ilrcpc", "flagm", "ssbs", "sb",
                          "paca", "pacg"};
    for (int i = 0; i < 32; i++) OUT("hwcap", "%s %d", bits[i], (int)((hw >> i) & 1));
    OUT("hwcap2", "%lx", hw2);
    errno = 0;
    struct { long nr; const char *name; long a, b, c; } calls[] = {
        {425, "io_uring_setup", 1, 0, 0},     {SYS_userfaultfd, "userfaultfd", 0, 0, 0},
        {241, "perf_event_open", 0, 0, 0},    {SYS_bpf, "bpf", 0, 0, 0},
        {447, "memfd_secret", 0, 0, 0},       {444, "landlock_create_ruleset", 0, 0, 1},
        {SYS_membarrier, "membarrier", 0, 0, 0}, {SYS_getrandom, "getrandom", 0, 0, 0},
        {SYS_seccomp, "seccomp_actions", 2, 0, 0}, {SYS_kcmp, "kcmp", 0, 0, 0},
    };
    for (size_t i = 0; i < sizeof calls / sizeof calls[0]; i++) {
        long r = syscall(calls[i].nr, calls[i].a, calls[i].b, calls[i].c, 0, 0, 0);
        if (r >= 0 && calls[i].nr != SYS_membarrier && calls[i].nr != SYS_getrandom) close((int)r);
        OUT("syscall", "%s %s", calls[i].name, r < 0 ? strerror(errno) : "ok");
    }
    cap("mlock", mlock(&hw, 8));
    cap("timerfd", timerfd_create(CLOCK_BOOTTIME, 0));
    cap("inotify", syscall(SYS_inotify_init1, 0));
    ns_flag = CLONE_NEWUSER; /* in a child, so later probes run in the original namespaces */
    OUT("unshare_user", "%s", child_word(in_child(c_unshare)));
    const char *files[] = {"/proc/self/maps", "/proc/self/pagemap", "/proc/self/smaps", "/proc/cpuinfo",
                           "/proc/self/auxv", "/sys/devices/system/cpu/online", "/dev/random", "/dev/urandom",
                           "/dev/shm", "/proc/sys/kernel/random/boot_id", "/sys/kernel/mm/transparent_hugepage/enabled"};
    for (size_t i = 0; i < sizeof files / sizeof files[0]; i++) OUT("file", "%s %d", files[i], access(files[i], F_OK) == 0);
    OUT("cpus", "%ld", sysconf(_SC_NPROCESSORS_ONLN));
    OUT("rseqRegistered", "%d", __rseq_size > 0);
    capabilities_more();
    return 1;
}

/* ---- signals: frames, contexts, restarts and queues ---- */

static void cfz_sleep_us(long us) {
    struct timespec t = {us / 1000000, (us % 1000000) * 1000};
    while (nanosleep(&t, &t) && errno == EINTR) {}
}
static void cfz_wake(volatile int *w) { syscall(SYS_futex, w, FUTEX_WAKE_PRIVATE, INT_MAX, NULL, NULL, 0); }
/* Sleeps while *w == v, at most ms milliseconds. */
static void cfz_wait(volatile int *w, int v, long ms) {
    struct timespec t = {ms / 1000, (ms % 1000) * 1000000};
    syscall(SYS_futex, w, FUTEX_WAIT_PRIVATE, v, &t, NULL, 0);
}
/* Waits until the counter *w reaches want; 0 after ms milliseconds without it. */
static int cfz_wait_for(volatile int *w, int want, long ms) {
    uint64_t end = mono_ns(CLOCK_MONOTONIC) + (uint64_t)ms * 1000000ULL;
    int v;
    while ((v = __atomic_load_n(w, __ATOMIC_ACQUIRE)) < want) {
        if (mono_ns(CLOCK_MONOTONIC) > end) return 0;
        cfz_wait(w, v, 10);
    }
    return 1;
}
static uint64_t kmask_get(void) { uint64_t m = 0; syscall(SYS_rt_sigprocmask, SIG_BLOCK, NULL, &m, 8); return m; }
static uint64_t kpending(void) { uint64_t m = 0; syscall(SYS_rt_sigpending, &m, 8); return m; }
#define SIGBIT(s) (1ULL << ((s) - 1))
/* Thread-directed SI_QUEUE signal with a value, as sigqueue sends it. */
static int cfz_queue(pid_t tid, int sig, int val) {
    siginfo_t si;
    memset(&si, 0, sizeof si);
    si.si_signo = sig;
    si.si_code = SI_QUEUE;
    si.si_pid = getpid();
    si.si_uid = getuid();
    si.si_value.sival_int = val;
    return (int)syscall(SYS_rt_tgsigqueueinfo, getpid(), tid, sig, &si);
}

/* A JIT-style writer: rewrite a slot, flush it with IC IVAU, call it. */
struct jitw { uint32_t *page; long calls, stale; };
static void jit_init(struct jitw *j) {
    j->page = mmap(0, 4096, PROT_READ | PROT_WRITE | PROT_EXEC, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    j->calls = j->stale = 0;
}
static void jit_step(struct jitw *j, long i) {
    uint32_t *slot = j->page + (i % 64) * 4;
    uint16_t v = (uint16_t)(i * 7 + 3);
    emit_const(slot, v);
    __builtin___clear_cache((char *)slot, (char *)slot + 8);
    int got = ((int (*)(void))slot)();
    j->calls++;
    j->stale += got != v;
}
static volatile int fl_stop;
static void *flusher(void *p) {
    struct jitw *j = p;
    for (long i = 0; !__atomic_load_n(&fl_stop, __ATOMIC_ACQUIRE); i++) {
        jit_step(j, i);
        sched_yield();
    }
    return NULL;
}

/* Signal frame records (arch/arm64/include/uapi/asm/sigcontext.h). */
#define CFZ_FPSIMD_MAGIC 0x46508001u
#define CFZ_ESR_MAGIC 0x45535201u
#define CFZ_EXTRA_MAGIC 0x45585401u
struct cfz_ctx { uint32_t magic, size; };
struct cfz_fpsimd { struct cfz_ctx head; uint32_t fpsr, fpcr; __uint128_t vregs[32]; };
struct cfz_esr { struct cfz_ctx head; uint64_t esr; };
struct cfz_extra { struct cfz_ctx head; uint64_t datap; uint32_t size, reserved[3]; };
struct cfz_walk { struct cfz_fpsimd *fp; int fpsimd, fpsize, esr, extra, extraok, term, bad; uint64_t esrv; };
static void ctx_walk(ucontext_t *uc, struct cfz_walk *w) {
    memset(w, 0, sizeof *w);
    unsigned char *p = uc->uc_mcontext.__reserved;
    size_t lim = sizeof uc->uc_mcontext.__reserved, off = 0;
    int in_extra = 0;
    for (int n = 0; n < 64; n++) {
        if (off + 8 > lim) { w->bad++; break; }
        struct cfz_ctx *h = (struct cfz_ctx *)(p + off);
        if (h->magic == 0 && h->size == 0) { w->term = 1; w->extraok = in_extra; break; }
        if (h->size < 8 || h->size % 16 || off + h->size > lim) { w->bad++; break; }
        if (h->magic == CFZ_FPSIMD_MAGIC) { w->fpsimd++; w->fpsize = (int)h->size; w->fp = (struct cfz_fpsimd *)h; }
        else if (h->magic == CFZ_ESR_MAGIC) { w->esr++; w->esrv = ((struct cfz_esr *)h)->esr; }
        else if (h->magic == CFZ_EXTRA_MAGIC && !in_extra) {
            struct cfz_extra *e = (struct cfz_extra *)h;
            w->extra++;
            p = (unsigned char *)(uintptr_t)e->datap;
            lim = e->size;
            off = 0;
            in_extra = 1;
            continue;
        }
        off += h->size;
    }
}

#define CFZ_FN(n) ".globl " n "\n.hidden " n "\n.type " n ", %function\n" n ":\n"
#define CFZ_AT(n) ".globl " n "\n.hidden " n "\n" n ":\n"
#define CFZ_HIDDEN __attribute__((visibility("hidden")))
/* A raw syscall whose svc and return addresses are known: cfz_rawsys(a0..a5, nr). */
CFZ_HIDDEN long cfz_rawsys(long, long, long, long, long, long, long);
CFZ_HIDDEN extern const char cfz_svc_at[], cfz_svc_after[];
/* Fault sites: f(addr, nzcv << 28, &sp) records sp, sets NZCV, then faults at f_at. */
CFZ_HIDDEN void cfz_sf_store(void *, uint64_t, uint64_t *);
CFZ_HIDDEN void cfz_sf_ldar(void *, uint64_t, uint64_t *);
CFZ_HIDDEN void cfz_sf_brk(void *, uint64_t, uint64_t *);
CFZ_HIDDEN void cfz_sf_udf(void *, uint64_t, uint64_t *);
CFZ_HIDDEN void cfz_sf_spin(volatile int *, uint64_t, uint64_t *);
CFZ_HIDDEN extern const char cfz_sf_store_at[], cfz_sf_ldar_at[], cfz_sf_brk_at[], cfz_sf_udf_at[], cfz_sf_spin_loop[],
    cfz_sf_spin_end[];
asm(".text\n.p2align 2\n"
    CFZ_FN("cfz_rawsys") "mov x8, x6\n" CFZ_AT("cfz_svc_at") "svc #0\n" CFZ_AT("cfz_svc_after") "ret\n"
    CFZ_FN("cfz_sf_store") "mov x9, sp\nstr x9, [x2]\nmsr nzcv, x1\n" CFZ_AT("cfz_sf_store_at") "str x1, [x0]\nret\n"
    CFZ_FN("cfz_sf_ldar") "mov x9, sp\nstr x9, [x2]\nmsr nzcv, x1\n" CFZ_AT("cfz_sf_ldar_at") "ldar x9, [x0]\nret\n"
    CFZ_FN("cfz_sf_brk") "mov x9, sp\nstr x9, [x2]\nmsr nzcv, x1\n" CFZ_AT("cfz_sf_brk_at") "brk #0x7\nret\n"
    CFZ_FN("cfz_sf_udf") "mov x9, sp\nstr x9, [x2]\nmsr nzcv, x1\n" CFZ_AT("cfz_sf_udf_at") ".inst 0x00000000\nret\n"
    CFZ_FN("cfz_sf_spin") "mov x9, sp\nstr x9, [x2]\nmsr nzcv, x1\n" CFZ_AT("cfz_sf_spin_loop")
    "1: ldr w9, [x0]\ncbz w9, 1b\n" CFZ_AT("cfz_sf_spin_end") "ret\n");

/* det: the frame a handler sees for each fault kind and for an asynchronous tgkill,
 * on the thread stack and on an alternate stack, printed as predicates. */
struct sfrec {
    int valid, signo, code, err, sysno;
    unsigned arch;
    uintptr_t addr, call, hsp;
    long pid, uid;
    uint64_t pc, sp, pstate, fault, x0, flags, mask, cur;
    stack_t st;
    struct cfz_walk w;
};
static struct sfrec sfr;
static volatile int sf_flag;
static pid_t sf_tid;
static void sf_handler(int s, siginfo_t *si, void *ucv) {
    ucontext_t *uc = ucv;
    uint64_t pc = uc->uc_mcontext.pc;
    if (s == SIGUSR1 && (pc < (uintptr_t)cfz_sf_spin_loop || pc >= (uintptr_t)cfz_sf_spin_end)) return;
    int saved = errno;
    struct sfrec *r = &sfr;
    volatile char here = 0;
    r->signo = s;
    r->code = si->si_code;
    r->err = si->si_errno;
    r->addr = (uintptr_t)si->si_addr;
    r->pid = si->si_pid;
    r->uid = si->si_uid;
    if (s == SIGSYS) { r->sysno = si->si_syscall; r->arch = si->si_arch; r->call = (uintptr_t)si->si_call_addr; }
    r->pc = pc;
    r->sp = uc->uc_mcontext.sp;
    r->pstate = uc->uc_mcontext.pstate;
    r->fault = uc->uc_mcontext.fault_address;
    r->x0 = uc->uc_mcontext.regs[0];
    r->flags = uc->uc_flags;
    r->st = uc->uc_stack;
    memcpy(&r->mask, &uc->uc_sigmask, 8);
    r->cur = kmask_get();
    r->hsp = (uintptr_t)&here;
    ctx_walk(uc, &r->w);
    r->w.fp = NULL;
    r->valid = 1;
    if (s == SIGUSR1) sf_flag = 1;
    else if (s != SIGSYS) uc->uc_mcontext.pc = pc + 4;
    errno = saved;
}
static void *sf_sender(void *p) {
    (void)p;
    while (!__atomic_load_n(&sf_flag, __ATOMIC_ACQUIRE)) {
        syscall(SYS_tgkill, getpid(), sf_tid, SIGUSR1);
        cfz_sleep_us(300);
    }
    return NULL;
}
enum { SF_MAPERR, SF_ACCERR, SF_BUS, SF_BRK, SF_UDF, SF_TGKILL, SF_KINDS };
static const char *const sf_names[SF_KINDS] = {"segvMaperr", "segvAccerr", "busAdraln", "trapBrk", "illOpc", "tgkill"};
static void sf_line(char *b, size_t n, int kind, const struct sfrec *r, uintptr_t obj, uintptr_t site, uint64_t spat,
                    unsigned nzcv, const stack_t *alt, uint64_t tm) {
    int len = 0;
#define SFP(...) len += snprintf(b + len, n - (size_t)len, __VA_ARGS__)
    if (!r->valid) { snprintf(b, n, "noSignal"); return; }
    SFP("sig %d code %d", r->signo, r->code);
    if (kind <= SF_BUS) SFP(" addr %+ld fault %+ld", (long)(r->addr - obj), (long)(r->fault - obj));
    if (kind == SF_BRK || kind == SF_UDF) SFP(" addrIsPc %d", r->addr == site);
    if (kind == SF_UDF) SFP(" faultZero %d", r->fault == 0);
    if (kind == SF_TGKILL) SFP(" pidOk %d uidOk %d pcInLoop 1", r->pid == getpid(), r->uid == (long)getuid());
    else SFP(" pcOk %d", r->pc == site);
    SFP(" spOk %d nzcvOk %d daif %u el %u", r->sp == spat, (unsigned)((r->pstate >> 28) & 15) == nzcv,
        (unsigned)((r->pstate >> 6) & 15), (unsigned)(r->pstate & 15));
    if (kind <= SF_BUS) { /* data aborts; whether other faults carry an ESR record varies by kernel */
        if (r->w.esr) SFP(" esr ec %#x wnr %d", (unsigned)(r->w.esrv >> 26) & 0x3f, (int)(r->w.esrv >> 6) & 1);
        else SFP(" esr absent");
    }
    SFP(" fpsimd %d/%d term %d bad %d extraOk %d ucFlags %lu", r->w.fpsimd, r->w.fpsize, r->w.term, r->w.bad,
        !r->w.extra || r->w.extraok, (unsigned long)r->flags);
    if (alt)
        SFP(" stack sp %d size %d flags %d onAlt %d", r->st.ss_sp == alt->ss_sp, r->st.ss_size == alt->ss_size, r->st.ss_flags,
            r->hsp >= (uintptr_t)alt->ss_sp && r->hsp < (uintptr_t)alt->ss_sp + alt->ss_size);
    else SFP(" stack flags %d sp %d size %zu", r->st.ss_flags, r->st.ss_sp != NULL, r->st.ss_size);
    SFP(" mask %d handlerMask %d", r->mask == tm, r->cur == (tm | SIGBIT(r->signo)));
#undef SFP
}
static int t_sigframe(void) {
    char *m = mmap(0, 3 * 4096, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    munmap(m + 4096, 4096);
    mprotect(m + 2 * 4096, 4096, PROT_READ);
    static char abuf[64] __attribute__((aligned(16)));
    static char altmem[65536] __attribute__((aligned(16)));
    stack_t alt = {.ss_sp = altmem, .ss_flags = 0, .ss_size = sizeof altmem};
    uint64_t tm = SIGBIT(SIGUSR2) | SIGBIT(SIGRTMIN + 3);
    sigset_t set;
    sigemptyset(&set);
    sigaddset(&set, SIGUSR2);
    sigaddset(&set, SIGRTMIN + 3);
    const int sigs[] = {SIGSEGV, SIGBUS, SIGTRAP, SIGILL, SIGUSR1};
    sf_tid = gettid();
    static char first[2][SF_KINDS][512];
    long same = 0, rounds = 20 * scale;
    for (long round = 0; round < rounds; round++) {
        for (int arm = 0; arm < 2; arm++) {
            stack_t ss = arm ? alt : (stack_t){.ss_sp = NULL, .ss_flags = SS_DISABLE, .ss_size = 0};
            if (sigaltstack(&ss, NULL)) { OUT("UNSUPPORTED", "sigaltstack %s", strerror(errno)); return 0; }
            struct sigaction a;
            memset(&a, 0, sizeof a);
            a.sa_sigaction = sf_handler;
            a.sa_flags = SA_SIGINFO | (arm ? SA_ONSTACK : 0);
            sigemptyset(&a.sa_mask);
            for (size_t k = 0; k < sizeof sigs / sizeof sigs[0]; k++) sigaction(sigs[k], &a, NULL);
            for (int kind = 0; kind < SF_KINDS; kind++) {
                unsigned nzcv = (unsigned)(next() & 15);
                uint64_t spat = 0, flags = (uint64_t)nzcv << 28;
                uintptr_t obj = 0, site = 0;
                sigset_t old;
                memset(&sfr, 0, sizeof sfr);
                sigprocmask(SIG_SETMASK, &set, &old);
                switch (kind) {
                case SF_MAPERR:
                    obj = (uintptr_t)(m + 4096);
                    site = (uintptr_t)cfz_sf_store_at;
                    cfz_sf_store((void *)(obj + 16), flags, &spat);
                    break;
                case SF_ACCERR:
                    obj = (uintptr_t)(m + 2 * 4096);
                    site = (uintptr_t)cfz_sf_store_at;
                    cfz_sf_store((void *)(obj + 24), flags, &spat);
                    break;
                case SF_BUS: /* an 8-byte acquire load across a 16-byte boundary */
                    obj = (uintptr_t)abuf;
                    site = (uintptr_t)cfz_sf_ldar_at;
                    cfz_sf_ldar((void *)(obj + 15), flags, &spat);
                    break;
                case SF_BRK:
                    obj = site = (uintptr_t)cfz_sf_brk_at;
                    cfz_sf_brk(NULL, flags, &spat);
                    break;
                case SF_UDF:
                    obj = site = (uintptr_t)cfz_sf_udf_at;
                    cfz_sf_udf(NULL, flags, &spat);
                    break;
                default: {
                    pthread_t t;
                    sf_flag = 0;
                    pthread_create(&t, 0, sf_sender, 0);
                    cfz_sf_spin(&sf_flag, flags, &spat);
                    pthread_join(t, 0);
                }
                }
                sigprocmask(SIG_SETMASK, &old, NULL);
                char line[512];
                sf_line(line, sizeof line, kind, &sfr, obj, site, spat, nzcv, arm ? &alt : NULL, tm);
                if (round == 0) strcpy(first[arm][kind], line);
                same += !strcmp(first[arm][kind], line);
            }
        }
    }
    stack_t off = {.ss_sp = NULL, .ss_flags = SS_DISABLE, .ss_size = 0};
    sigaltstack(&off, NULL);
    for (size_t k = 0; k < sizeof sigs / sizeof sigs[0]; k++) signal(sigs[k], SIG_DFL);
    for (int arm = 0; arm < 2; arm++)
        for (int kind = 0; kind < SF_KINDS; kind++) {
            char key[64];
            snprintf(key, sizeof key, "%s%s", sf_names[kind], arm ? "Alt" : "");
            OUT(key, "%s", first[arm][kind]);
        }
    OUT("same", "%ld/%ld", same, rounds * 2 * SF_KINDS);
    /* SIGSYS from a seccomp filter, in a child since the filter is permanent. */
    int pfd[2];
    if (pipe(pfd)) return 0;
    pid_t c = fork();
    if (!c) {
        struct sigaction a;
        memset(&a, 0, sizeof a);
        a.sa_sigaction = sf_handler;
        a.sa_flags = SA_SIGINFO;
        sigaction(SIGSYS, &a, NULL);
        sigprocmask(SIG_SETMASK, &set, NULL);
        struct sock_filter f[] = {
            BPF_STMT(BPF_LD | BPF_W | BPF_ABS, offsetof(struct seccomp_data, nr)),
            BPF_JUMP(BPF_JMP | BPF_JEQ | BPF_K, SYS_getppid, 0, 1),
            BPF_STMT(BPF_RET | BPF_K, SECCOMP_RET_TRAP | 0x42),
            BPF_STMT(BPF_RET | BPF_K, SECCOMP_RET_ALLOW),
        };
        struct sock_fprog prog = {sizeof f / sizeof f[0], f};
        char line[512];
        if (prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) || prctl(PR_SET_SECCOMP, SECCOMP_MODE_FILTER, &prog)) {
            snprintf(line, sizeof line, "unsupported");
        } else {
            memset(&sfr, 0, sizeof sfr);
            long r = cfz_rawsys(0x5eed, 0, 0, 0, 0, 0, SYS_getppid);
            const struct sfrec *q = &sfr;
            snprintf(line, sizeof line,
                     "sig %d code %d data %#x nr %d archOk %d callAddrOk %d pcOk %d x0IsArg %d retIsArg %d mask %d "
                     "handlerMask %d fpsimd %d/%d term %d bad %d",
                     q->signo, q->code, q->err, q->sysno, q->arch == 0xc00000b7, q->call == (uintptr_t)cfz_svc_after,
                     q->pc == (uintptr_t)cfz_svc_after, q->x0 == 0x5eed, r == 0x5eed, q->mask == tm,
                     q->cur == (tm | SIGBIT(SIGSYS)), q->w.fpsimd, q->w.fpsize, q->w.term, q->w.bad);
        }
        _exit(write(pfd[1], line, strlen(line)) > 0 ? 0 : 1);
    }
    close(pfd[1]);
    char buf[600];
    size_t got = 0;
    ssize_t k;
    while (got < sizeof buf - 1 && (k = read(pfd[0], buf + got, sizeof buf - 1 - got)) > 0) got += (size_t)k;
    buf[got] = 0;
    close(pfd[0]);
    waitpid(c, NULL, 0);
    OUT("sigsys", "%s", buf);
    return 1;
}

/* det: handlers for a brk and for tgkill (three loop shapes) rewrite pc, sp, x0-x30,
 * NZCV, FPCR, FPSR, v0-v31 and the mask; assembly at the new pc records them all. */
CFZ_HIDDEN void cfz_cr_enter(const uint64_t *xs, const void *vs, const uint64_t *ctl, volatile int *ready);
CFZ_HIDDEN extern const char cfz_cr_brk_at[], cfz_cr_spin[], cfz_cr_ibl[], cfz_cr_ibl_ret[], cfz_cr_ibl_end[], cfz_cr_long[],
    cfz_cr_long_end[], cfz_cr_callee[], cfz_cr_landing[];
CFZ_HIDDEN __attribute__((used, noreturn)) void cfz_cr_landed(uint64_t *rec);
/* ctl: nzcv << 28, fpcr, fpsr, mode (0 brk, 1 spin, 2 blr/ret, 3 long), round, [out] sp. */
asm(".text\n.p2align 2\n"
    ".macro cfz_loadx\n"
    "ldp x0, x1, [x30, #0]\nldp x2, x3, [x30, #16]\nldp x4, x5, [x30, #32]\nldp x6, x7, [x30, #48]\n"
    "ldp x8, x9, [x30, #64]\nldp x10, x11, [x30, #80]\nldp x12, x13, [x30, #96]\nldp x14, x15, [x30, #112]\n"
    "ldp x16, x17, [x30, #128]\nldp x18, x19, [x30, #144]\nldp x20, x21, [x30, #160]\nldp x22, x23, [x30, #176]\n"
    "ldp x24, x25, [x30, #192]\nldp x26, x27, [x30, #208]\nldp x28, x29, [x30, #224]\nldr x30, [x30, #240]\n"
    ".endm\n"
    CFZ_FN("cfz_cr_enter")
    "mov x30, x0\nmov x9, sp\nstr x9, [x2, #40]\nldr x10, [x2, #24]\nldr x11, [x2, #32]\n"
    "ld1 {v0.2d, v1.2d, v2.2d, v3.2d}, [x1], #64\nld1 {v4.2d, v5.2d, v6.2d, v7.2d}, [x1], #64\n"
    "ld1 {v8.2d, v9.2d, v10.2d, v11.2d}, [x1], #64\nld1 {v12.2d, v13.2d, v14.2d, v15.2d}, [x1], #64\n"
    "ld1 {v16.2d, v17.2d, v18.2d, v19.2d}, [x1], #64\nld1 {v20.2d, v21.2d, v22.2d, v23.2d}, [x1], #64\n"
    "ld1 {v24.2d, v25.2d, v26.2d, v27.2d}, [x1], #64\nld1 {v28.2d, v29.2d, v30.2d, v31.2d}, [x1], #64\n"
    "ldr x9, [x2, #8]\nmsr fpcr, x9\nldr x9, [x2, #16]\nmsr fpsr, x9\nldr x9, [x2]\nmsr nzcv, x9\n"
    "cbz x10, 1f\nstlr w11, [x3]\nmov x0, x3\nmov x1, #129\nmov x2, #1\nmov x8, #98\nsvc #0\n"
    "1:\ntbnz x10, #1, 3f\ntbnz x10, #0, 2f\n"
    "cfz_loadx\n" CFZ_AT("cfz_cr_brk_at") "brk #0x9\nb .\n"
    "2:\ncfz_loadx\n" CFZ_AT("cfz_cr_spin") "b .\n"
    "3:\ntbnz x10, #0, 4f\ncfz_loadx\n" CFZ_AT("cfz_cr_ibl") "5: blr x15\n" CFZ_AT("cfz_cr_ibl_ret") "b 5b\n"
    CFZ_AT("cfz_cr_ibl_end")
    "4:\ncfz_loadx\n" CFZ_AT("cfz_cr_long") "6:\n.rept 2048\nnop\n.endr\nb 6b\n" CFZ_AT("cfz_cr_long_end")
    CFZ_AT("cfz_cr_callee") "ret\n"
    CFZ_FN("cfz_cr_landing")
    "sub sp, sp, #800\n"
    "stp x0, x1, [sp, #0]\nstp x2, x3, [sp, #16]\nstp x4, x5, [sp, #32]\nstp x6, x7, [sp, #48]\n"
    "stp x8, x9, [sp, #64]\nstp x10, x11, [sp, #80]\nstp x12, x13, [sp, #96]\nstp x14, x15, [sp, #112]\n"
    "stp x16, x17, [sp, #128]\nstp x18, x19, [sp, #144]\nstp x20, x21, [sp, #160]\nstp x22, x23, [sp, #176]\n"
    "stp x24, x25, [sp, #192]\nstp x26, x27, [sp, #208]\nstp x28, x29, [sp, #224]\nstr x30, [sp, #240]\n"
    "add x0, sp, #800\nstr x0, [sp, #248]\n"
    "mrs x0, nzcv\nstr x0, [sp, #256]\nmrs x0, fpcr\nstr x0, [sp, #264]\nmrs x0, fpsr\nstr x0, [sp, #272]\n"
    "add x0, sp, #288\n"
    "st1 {v0.2d, v1.2d, v2.2d, v3.2d}, [x0], #64\nst1 {v4.2d, v5.2d, v6.2d, v7.2d}, [x0], #64\n"
    "st1 {v8.2d, v9.2d, v10.2d, v11.2d}, [x0], #64\nst1 {v12.2d, v13.2d, v14.2d, v15.2d}, [x0], #64\n"
    "st1 {v16.2d, v17.2d, v18.2d, v19.2d}, [x0], #64\nst1 {v20.2d, v21.2d, v22.2d, v23.2d}, [x0], #64\n"
    "st1 {v24.2d, v25.2d, v26.2d, v27.2d}, [x0], #64\nst1 {v28.2d, v29.2d, v30.2d, v31.2d}, [x0], #64\n"
    "mov x0, sp\nbl cfz_cr_landed\nb .\n");
#define CR_FPCR_MASK ((3UL << 22) | (3UL << 24))
#define CR_FPCR_A (3UL << 22)
#define CR_FPCR_B ((1UL << 22) | (1UL << 25))
#define CR_FPSR_MASK 0x0800009fUL
#define CR_FPSR_A 0x11UL
#define CR_FPSR_B 0x08000002UL
static uint64_t cr_a[31], cr_b[31], cr_ctl[6], cr_cx, cr_cv, cr_nb, cr_top, cr_fpcr0, cr_masknow;
static __uint128_t cr_av[32] __attribute__((aligned(16))), cr_bv[32];
static uint64_t cr_rec[100] __attribute__((aligned(16)));
static volatile int cr_ready, cr_hit, cr_quit, cr_round;
static int cr_mode;
static long cr_capbad;
static pid_t cr_tid;
static sigjmp_buf cr_jb;
void cfz_cr_landed(uint64_t *rec) {
    memcpy(cr_rec, rec, 800);
    asm volatile("msr fpcr, %0\nmsr fpsr, xzr" ::"r"(cr_fpcr0) : "memory");
    cr_masknow = kmask_get();
    siglongjmp(cr_jb, 1);
}
static int cr_in(int s, uintptr_t pc) {
    switch (cr_mode) {
    case 0: return s == SIGTRAP;
    case 1: return s == SIGUSR1 && pc == (uintptr_t)cfz_cr_spin;
    case 2: return s == SIGUSR1 && ((pc >= (uintptr_t)cfz_cr_ibl && pc < (uintptr_t)cfz_cr_ibl_end) || pc == (uintptr_t)cfz_cr_callee);
    default: return s == SIGUSR1 && pc >= (uintptr_t)cfz_cr_long && pc < (uintptr_t)cfz_cr_long_end;
    }
}
static void cr_handler(int s, siginfo_t *si, void *ucv) {
    (void)si;
    ucontext_t *uc = ucv;
    mcontext_t *m = &uc->uc_mcontext;
    if (!cr_in(s, m->pc) || cr_hit == cr_round) return;
    struct cfz_walk w;
    ctx_walk(uc, &w);
    long bad = cr_mode == 0 && m->pc != (uintptr_t)cfz_cr_brk_at;
    for (int i = 0; i < 31; i++) bad += m->regs[i] != cr_a[i];
    bad += m->sp != cr_ctl[5];
    bad += ((m->pstate >> 28) & 15) != (cr_ctl[0] >> 28);
    if (!w.fp) bad += 1000;
    else {
        for (int i = 0; i < 32; i++) bad += w.fp->vregs[i] != cr_av[i];
        bad += (w.fp->fpcr & CR_FPCR_MASK) != CR_FPCR_A;
        bad += (w.fp->fpsr & CR_FPSR_MASK) != CR_FPSR_A;
        for (int i = 0; i < 32; i++)
            if (cr_cv >> i & 1) w.fp->vregs[i] = cr_bv[i];
        w.fp->fpcr = (uint32_t)((w.fp->fpcr & ~CR_FPCR_MASK) | CR_FPCR_B);
        w.fp->fpsr = (uint32_t)CR_FPSR_B;
    }
    for (int i = 0; i < 31; i++)
        if (cr_cx >> i & 1) m->regs[i] = cr_b[i];
    m->pstate = (m->pstate & ~(15ULL << 28)) | (cr_nb << 28);
    m->sp = cr_top;
    m->pc = (uintptr_t)cfz_cr_landing;
    sigaddset(&uc->uc_sigmask, SIGUSR2);
    cr_capbad = bad;
    __atomic_store_n(&cr_hit, cr_round, __ATOMIC_RELEASE);
}
static void *cr_sender(void *p) {
    (void)p;
    int last = 0;
    for (;;) {
        int r;
        while ((r = __atomic_load_n(&cr_ready, __ATOMIC_ACQUIRE)) == last) cfz_wait(&cr_ready, last, 1000);
        last = r;
        if (__atomic_load_n(&cr_quit, __ATOMIC_ACQUIRE)) return NULL;
        while (__atomic_load_n(&cr_hit, __ATOMIC_ACQUIRE) != r) {
            syscall(SYS_tgkill, getpid(), cr_tid, SIGUSR1);
            cfz_sleep_us(300);
        }
    }
}
static int t_ctxrewrite(void) {
    static char stack[65536] __attribute__((aligned(16)));
    cr_top = ((uintptr_t)stack + sizeof stack) & ~15UL;
    asm volatile("mrs %0, fpcr" : "=r"(cr_fpcr0));
    cr_tid = gettid();
    struct sigaction a;
    memset(&a, 0, sizeof a);
    a.sa_sigaction = cr_handler;
    a.sa_flags = SA_SIGINFO;
    sigemptyset(&a.sa_mask);
    sigaction(SIGTRAP, &a, NULL);
    sigaction(SIGUSR1, &a, NULL);
    pthread_t t;
    pthread_create(&t, 0, cr_sender, 0);
    const char *names[4] = {"brk", "spin", "blrRet", "longLoop"};
    long rounds = 50 * scale;
    for (int mode = 0; mode < 4; mode++) {
        volatile long cap = 0, res = 0, msk = 0;
        for (long r = 0; r < rounds; r++) {
            for (int i = 0; i < 31; i++) { cr_a[i] = next(); cr_b[i] = next(); }
            for (int i = 0; i < 32; i++) {
                cr_av[i] = ((__uint128_t)next() << 64) | next();
                cr_bv[i] = ((__uint128_t)next() << 64) | next();
            }
            cr_cx = next() & 0x7fffffff;
            cr_cv = next() & 0xffffffff;
            if (mode == 2) { cr_a[15] = (uintptr_t)cfz_cr_callee; cr_a[30] = (uintptr_t)cfz_cr_ibl_ret; }
            cr_ctl[0] = (next() & 15) << 28;
            cr_nb = next() & 15;
            cr_ctl[1] = (cr_fpcr0 & ~CR_FPCR_MASK) | CR_FPCR_A;
            cr_ctl[2] = CR_FPSR_A;
            cr_ctl[3] = (uint64_t)mode;
            cr_round = cr_round + 1;
            cr_ctl[4] = (uint64_t)cr_round;
            cr_mode = mode;
            cr_capbad = -1;
            if (!sigsetjmp(cr_jb, 1)) cfz_cr_enter(cr_a, cr_av, cr_ctl, &cr_ready);
            cap += cr_capbad == 0;
            long bad = 0;
            for (int i = 0; i < 31; i++) bad += cr_rec[i] != ((cr_cx >> i & 1) ? cr_b[i] : cr_a[i]);
            bad += cr_rec[31] != cr_top;
            bad += ((cr_rec[32] >> 28) & 15) != cr_nb;
            bad += (cr_rec[33] & CR_FPCR_MASK) != CR_FPCR_B;
            bad += (cr_rec[34] & CR_FPSR_MASK) != CR_FPSR_B;
            __uint128_t v[32];
            memcpy(v, cr_rec + 36, sizeof v);
            for (int i = 0; i < 32; i++) bad += v[i] != ((cr_cv >> i & 1) ? cr_bv[i] : cr_av[i]);
            res += bad == 0;
            msk += (cr_masknow & SIGBIT(SIGUSR2)) != 0;
        }
        OUT(names[mode], "captured %ld/%ld restored %ld/%ld maskApplied %ld/%ld", cap, rounds, res, rounds, msk, rounds);
    }
    __atomic_store_n(&cr_quit, 1, __ATOMIC_RELEASE);
    __atomic_store_n(&cr_ready, -1, __ATOMIC_RELEASE);
    cfz_wake(&cr_ready);
    pthread_join(t, 0);
    signal(SIGTRAP, SIG_DFL);
    signal(SIGUSR1, SIG_DFL);
    return 1;
}

/* det: blocking syscalls interrupted by paced tgkill from another thread, with and
 * without SA_RESTART and with a nested handler. The handler classifies each signal
 * by its frame: pc at the svc (restart pending) or after it with x0 = -EINTR. */
enum { RM_READ, RM_WRITE, RM_NANOSLEEP, RM_CLOCKABS, RM_FUTEX, RM_PPOLL, RM_EPOLL, RM_ACCEPT, RM_CONNECT, RM_RECVMSG,
       RM_WAIT4, RM_FLOCK, RM_SIGWAIT, RM_SEMOP, RM_URING, RM_WAITV, RM_PAUSE, RM_N };
static const char *const rm_names[RM_N] = {"read", "write", "nanosleep", "clockNanosleepAbs", "futexWait", "ppoll",
                                           "epollPwait2", "accept4", "connect", "recvmsg", "wait4", "flock",
                                           "sigtimedwait", "semop", "ioUringEnter", "futexWaitv", "sigsuspendPause"};
#define RM_WTOTAL (256 * 1024)
#define RM_SLEEP_NS 200000000L
static volatile int rm_robs, rm_eobs, rm_usr1, rm_usr2, rm_go, rm_done, rm_quit, rm_case, rm_nested, rm_main_done,
    rm_stream_ok;
static pid_t rm_tid;
static unsigned char *rm_stream;
static struct {
    int p[2], ep, lfd, cfd, filler, sock, sp[2], memfd, fd2, semid, ring, cp[2], reaped;
    pid_t child;
    uint32_t fw;
    size_t prefill, sqlen, cqlen;
    struct sockaddr_un addr;
    socklen_t alen;
    char *sq, *cq;
    struct io_uring_sqe *sqe;
    struct io_uring_params up;
} rmx;
static void rm_handler(int s, siginfo_t *si, void *ucv) {
    (void)s; (void)si;
    int saved = errno;
    ucontext_t *uc = ucv;
    uintptr_t pc = uc->uc_mcontext.pc;
    if (pc == (uintptr_t)cfz_svc_at) __atomic_add_fetch(&rm_robs, 1, __ATOMIC_SEQ_CST);
    else if (pc == (uintptr_t)cfz_svc_after && (long)uc->uc_mcontext.regs[0] == -EINTR) __atomic_add_fetch(&rm_eobs, 1, __ATOMIC_SEQ_CST);
    if (rm_nested) syscall(SYS_tgkill, getpid(), rm_tid, SIGUSR2);
    __atomic_add_fetch(&rm_usr1, 1, __ATOMIC_SEQ_CST);
    cfz_wake(&rm_usr1);
    errno = saved;
}
static void rm_handler2(int s) { (void)s; __atomic_add_fetch(&rm_usr2, 1, __ATOMIC_SEQ_CST); }
static void rm_noop(int s) { (void)s; }
/* One signal to the main thread, waiting for its handler. */
static void rm_kick(void) {
    int a = __atomic_load_n(&rm_usr1, __ATOMIC_ACQUIRE);
    syscall(SYS_tgkill, getpid(), rm_tid, SIGUSR1);
    for (int k = 0; k < 10000 && __atomic_load_n(&rm_usr1, __ATOMIC_ACQUIRE) == a; k++) cfz_wait(&rm_usr1, a, 1);
}
/* Reads the whole numbered stream, interrupting the writer now and then. */
static void rm_drain(void) {
    static unsigned char buf[8192];
    size_t total = rmx.prefill + RM_WTOTAL, got = 0;
    int ok = 1, n = 0;
    while (got < total) {
        ssize_t r = read(rmx.p[0], buf, sizeof buf);
        if (r <= 0) {
            if (r < 0 && errno == EINTR) continue;
            ok = 0;
            break;
        }
        for (ssize_t i = 0; i < r; i++) ok &= buf[i] == (unsigned char)((got + (size_t)i) % 251);
        got += (size_t)r;
        if (++n % 4 == 0) rm_kick();
    }
    rm_stream_ok = ok && got == total;
}
static void rm_complete(int c) {
    char z = 'z';
    switch (c) {
    case RM_READ: case RM_PPOLL: case RM_EPOLL: case RM_URING:
        if (write(rmx.p[1], &z, 1) != 1) {}
        break;
    case RM_WRITE: rm_drain(); break;
    case RM_FUTEX: case RM_WAITV:
        __atomic_store_n(&rmx.fw, 1, __ATOMIC_RELEASE);
        syscall(SYS_futex, &rmx.fw, FUTEX_WAKE_PRIVATE, INT_MAX, NULL, NULL, 0);
        break;
    case RM_ACCEPT:
        rmx.cfd = socket(AF_UNIX, SOCK_STREAM, 0);
        if (connect(rmx.cfd, (struct sockaddr *)&rmx.addr, rmx.alen)) {}
        break;
    case RM_CONNECT: {
        int a = accept(rmx.lfd, NULL, NULL); /* the filler: the queue has room again */
        if (a >= 0) close(a);
        break;
    }
    case RM_RECVMSG:
        if (write(rmx.sp[1], &z, 1) != 1) {}
        break;
    case RM_WAIT4:
        if (write(rmx.cp[1], &z, 1) != 1) {}
        break;
    case RM_FLOCK: flock(rmx.memfd, LOCK_UN); break;
    case RM_SIGWAIT: syscall(SYS_tgkill, getpid(), rm_tid, SIGRTMIN + 5); break;
    case RM_SEMOP: {
        struct sembuf b = {0, 1, 0};
        semop(rmx.semid, &b, 1);
        break;
    }
    default: break;
    }
}
static void *rm_helper(void *p) {
    (void)p;
    int seen = 0;
    for (;;) {
        int g;
        while ((g = __atomic_load_n(&rm_go, __ATOMIC_ACQUIRE)) == seen) cfz_wait(&rm_go, seen, 1000);
        seen = g;
        if (__atomic_load_n(&rm_quit, __ATOMIC_ACQUIRE)) return NULL;
        for (int k = 0; k < 3000 && rm_eobs < 1 && rm_robs < 3 && !rm_main_done; k++) {
            cfz_sleep_us(1000);
            rm_kick();
        }
        rm_complete(rm_case);
        __atomic_store_n(&rm_done, seen, __ATOMIC_RELEASE);
        cfz_wake(&rm_done);
    }
}
static int rm_setup(int c) {
    rmx.fw = 0;
    rmx.cfd = rmx.filler = rmx.sock = -1;
    switch (c) {
    case RM_READ: case RM_PPOLL: return !pipe(rmx.p);
    case RM_WRITE: {
        if (pipe(rmx.p)) return 0;
        fcntl(rmx.p[1], F_SETPIPE_SZ, 65536);
        fcntl(rmx.p[1], F_SETFL, O_NONBLOCK);
        size_t off = 0;
        ssize_t r;
        while (off < (1 << 20) && (r = write(rmx.p[1], rm_stream + off, 4096)) > 0) off += (size_t)r;
        fcntl(rmx.p[1], F_SETFL, 0);
        rmx.prefill = off;
        return 1;
    }
    case RM_EPOLL: {
        if (pipe(rmx.p)) return 0;
        rmx.ep = epoll_create1(0);
        struct epoll_event e = {.events = EPOLLIN};
        epoll_ctl(rmx.ep, EPOLL_CTL_ADD, rmx.p[0], &e);
        struct timespec z = {0, 0};
        if (syscall(441, rmx.ep, &e, 1, &z, NULL, 8) < 0 && errno == ENOSYS) {
            close(rmx.ep); close(rmx.p[0]); close(rmx.p[1]);
            return 0;
        }
        return 1;
    }
    case RM_ACCEPT: case RM_CONNECT: {
        rmx.lfd = socket(AF_UNIX, SOCK_STREAM, 0);
        memset(&rmx.addr, 0, sizeof rmx.addr);
        rmx.addr.sun_family = AF_UNIX;
        int n = snprintf(rmx.addr.sun_path + 1, sizeof rmx.addr.sun_path - 1, "cfz-restart-%d-%d", (int)getpid(), c);
        rmx.alen = (socklen_t)(offsetof(struct sockaddr_un, sun_path) + 1 + (size_t)n);
        if (bind(rmx.lfd, (struct sockaddr *)&rmx.addr, rmx.alen) || listen(rmx.lfd, c == RM_CONNECT ? 0 : 4)) return 0;
        if (c == RM_CONNECT) { /* a backlog of 0 holds the filler; the next connect waits */
            rmx.filler = socket(AF_UNIX, SOCK_STREAM, 0);
            if (connect(rmx.filler, (struct sockaddr *)&rmx.addr, rmx.alen)) return 0;
            rmx.sock = socket(AF_UNIX, SOCK_STREAM, 0);
        }
        return 1;
    }
    case RM_RECVMSG: return !socketpair(AF_UNIX, SOCK_STREAM, 0, rmx.sp);
    case RM_WAIT4:
        if (pipe(rmx.cp)) return 0;
        rmx.reaped = 0;
        rmx.child = fork();
        if (!rmx.child) {
            char b;
            _exit(read(rmx.cp[0], &b, 1) == 1 ? 5 : 6);
        }
        return rmx.child > 0;
    case RM_FLOCK: {
        rmx.memfd = memfd_create("cfzlock", 0);
        if (rmx.memfd < 0 || flock(rmx.memfd, LOCK_EX)) return 0;
        char path[64];
        snprintf(path, sizeof path, "/proc/self/fd/%d", rmx.memfd);
        rmx.fd2 = open(path, O_RDWR); /* another open file description of the same file */
        return rmx.fd2 >= 0;
    }
    case RM_SIGWAIT: {
        sigset_t s;
        sigemptyset(&s);
        sigaddset(&s, SIGRTMIN + 5);
        signal(SIGRTMIN + 5, rm_noop); /* a signal the wait missed must not kill the process */
        return !pthread_sigmask(SIG_BLOCK, &s, NULL);
    }
    case RM_SEMOP:
        rmx.semid = semget(IPC_PRIVATE, 1, IPC_CREAT | 0600);
        return rmx.semid >= 0;
    case RM_URING: {
        static char ubuf[8];
        if (pipe(rmx.p)) return 0;
        memset(&rmx.up, 0, sizeof rmx.up);
        rmx.ring = (int)syscall(425, 4, &rmx.up);
        if (rmx.ring < 0) { close(rmx.p[0]); close(rmx.p[1]); return 0; }
        rmx.sqlen = rmx.up.sq_off.array + 4 * rmx.up.sq_entries;
        rmx.cqlen = rmx.up.cq_off.cqes + sizeof(struct io_uring_cqe) * rmx.up.cq_entries;
        rmx.sq = mmap(0, rmx.sqlen, PROT_READ | PROT_WRITE, MAP_SHARED, rmx.ring, IORING_OFF_SQ_RING);
        rmx.cq = mmap(0, rmx.cqlen, PROT_READ | PROT_WRITE, MAP_SHARED, rmx.ring, IORING_OFF_CQ_RING);
        rmx.sqe = mmap(0, sizeof(struct io_uring_sqe) * rmx.up.sq_entries, PROT_READ | PROT_WRITE, MAP_SHARED, rmx.ring,
                       IORING_OFF_SQES);
        uint32_t tail = *(uint32_t *)(rmx.sq + rmx.up.sq_off.tail), idx = tail & *(uint32_t *)(rmx.sq + rmx.up.sq_off.ring_mask);
        memset(&rmx.sqe[idx], 0, sizeof rmx.sqe[idx]);
        rmx.sqe[idx].opcode = IORING_OP_READ;
        rmx.sqe[idx].fd = rmx.p[0];
        rmx.sqe[idx].addr = (uintptr_t)ubuf;
        rmx.sqe[idx].len = 1;
        rmx.sqe[idx].user_data = 77;
        ((uint32_t *)(rmx.sq + rmx.up.sq_off.array))[idx] = idx;
        __atomic_store_n((uint32_t *)(rmx.sq + rmx.up.sq_off.tail), tail + 1, __ATOMIC_RELEASE);
        return syscall(426, rmx.ring, 1, 0, 0, NULL, 0) == 1;
    }
    case RM_WAITV: {
        struct { uint64_t val, uaddr; uint32_t flags, reserved; } w = {1, (uintptr_t)&rmx.fw, 2 | 128, 0};
        return !(syscall(449, &w, 1, 0, NULL, CLOCK_MONOTONIC) < 0 && errno == ENOSYS);
    }
    default: return 1;
    }
}
static void rm_teardown(int c) {
    switch (c) {
    case RM_READ: case RM_PPOLL: case RM_WRITE: close(rmx.p[0]); close(rmx.p[1]); break;
    case RM_EPOLL: close(rmx.ep); close(rmx.p[0]); close(rmx.p[1]); break;
    case RM_ACCEPT: case RM_CONNECT:
        close(rmx.lfd);
        if (rmx.cfd >= 0) close(rmx.cfd);
        if (rmx.filler >= 0) close(rmx.filler);
        if (rmx.sock >= 0) close(rmx.sock);
        break;
    case RM_RECVMSG: close(rmx.sp[0]); close(rmx.sp[1]); break;
    case RM_WAIT4:
        close(rmx.cp[0]); close(rmx.cp[1]);
        if (!rmx.reaped) waitpid(rmx.child, NULL, 0);
        break;
    case RM_FLOCK: close(rmx.fd2); close(rmx.memfd); break;
    case RM_SIGWAIT: {
        sigset_t s;
        sigemptyset(&s);
        sigaddset(&s, SIGRTMIN + 5);
        pthread_sigmask(SIG_UNBLOCK, &s, NULL);
        signal(SIGRTMIN + 5, SIG_DFL);
        break;
    }
    case RM_SEMOP: semctl(rmx.semid, 0, IPC_RMID); break;
    case RM_URING:
        munmap(rmx.sq, rmx.sqlen);
        munmap(rmx.cq, rmx.cqlen);
        munmap(rmx.sqe, sizeof(struct io_uring_sqe) * rmx.up.sq_entries);
        close(rmx.ring); close(rmx.p[0]); close(rmx.p[1]);
        break;
    default: break;
    }
}
/* Runs the call until it completes; returns the first call's raw result. */
static long rm_run(int c, int *ok) {
    long first = LONG_MIN, r = 0;
    int k = 0;
#define RM_CALL(...) do { r = cfz_rawsys(__VA_ARGS__); if (k++ == 0) first = r; } while (0)
    *ok = 0;
    switch (c) {
    case RM_READ: {
        char b;
        do RM_CALL(rmx.p[0], (long)&b, 1, 0, 0, 0, SYS_read); while (r == -EINTR);
        *ok = r == 1;
        break;
    }
    case RM_WRITE: {
        size_t off = 0;
        while (off < RM_WTOTAL) {
            RM_CALL(rmx.p[1], (long)(rm_stream + rmx.prefill + off), (long)(RM_WTOTAL - off), 0, 0, 0, SYS_write);
            if (r > 0) off += (size_t)r;
            else if (r != -EINTR) break;
        }
        *ok = off == RM_WTOTAL;
        break;
    }
    case RM_NANOSLEEP: {
        struct timespec req = {0, RM_SLEEP_NS}, rem = {0, 0};
        uint64_t t0 = mono_ns(CLOCK_MONOTONIC);
        RM_CALL((long)&req, (long)&rem, 0, 0, 0, 0, SYS_nanosleep);
        int remok = 1;
        if (r == -EINTR) {
            remok = rem.tv_sec == 0 && rem.tv_nsec > 0 && rem.tv_nsec <= RM_SLEEP_NS;
            while (nanosleep(&rem, &rem) && errno == EINTR) {}
        }
        *ok = remok && (r == 0 || r == -EINTR) && mono_ns(CLOCK_MONOTONIC) - t0 >= (uint64_t)RM_SLEEP_NS;
        break;
    }
    case RM_CLOCKABS: {
        uint64_t end = mono_ns(CLOCK_MONOTONIC) + RM_SLEEP_NS;
        struct timespec dl = {(time_t)(end / 1000000000ULL), (long)(end % 1000000000ULL)};
        do RM_CALL(CLOCK_MONOTONIC, TIMER_ABSTIME, (long)&dl, 0, 0, 0, SYS_clock_nanosleep); while (r == -EINTR);
        *ok = r == 0 && mono_ns(CLOCK_MONOTONIC) >= end;
        break;
    }
    case RM_FUTEX:
        while (!__atomic_load_n(&rmx.fw, __ATOMIC_ACQUIRE)) {
            RM_CALL((long)&rmx.fw, FUTEX_WAIT_PRIVATE, 0, 0, 0, 0, SYS_futex);
            if (r && r != -EINTR && r != -EAGAIN) break;
        }
        /* The wake can land between the last interruption and the restart. */
        if (first == -EAGAIN) first = 0;
        *ok = rmx.fw == 1;
        break;
    case RM_WAITV: {
        struct { uint64_t val, uaddr; uint32_t flags, reserved; } w = {0, (uintptr_t)&rmx.fw, 2 | 128, 0};
        while (!__atomic_load_n(&rmx.fw, __ATOMIC_ACQUIRE)) {
            RM_CALL((long)&w, 1, 0, 0, CLOCK_MONOTONIC, 0, 449);
            if (r < 0 && r != -EINTR && r != -EAGAIN) break;
        }
        if (first == -EAGAIN) first = 0;
        *ok = rmx.fw == 1;
        break;
    }
    case RM_PPOLL: {
        struct pollfd pf = {rmx.p[0], POLLIN, 0};
        do RM_CALL((long)&pf, 1, 0, 0, 8, 0, SYS_ppoll); while (r == -EINTR);
        *ok = r == 1;
        break;
    }
    case RM_EPOLL: {
        struct epoll_event e;
        do RM_CALL(rmx.ep, (long)&e, 1, 0, 0, 8, 441); while (r == -EINTR);
        *ok = r == 1;
        break;
    }
    case RM_ACCEPT:
        do RM_CALL(rmx.lfd, 0, 0, SOCK_CLOEXEC, 0, 0, SYS_accept4); while (r == -EINTR);
        *ok = r >= 0;
        if (r >= 0) close((int)r);
        break;
    case RM_CONNECT:
        do RM_CALL(rmx.sock, (long)&rmx.addr, rmx.alen, 0, 0, 0, SYS_connect); while (r == -EINTR);
        *ok = r == 0;
        break;
    case RM_RECVMSG: {
        char b;
        struct iovec io = {&b, 1};
        struct msghdr mh;
        memset(&mh, 0, sizeof mh);
        mh.msg_iov = &io;
        mh.msg_iovlen = 1;
        do RM_CALL(rmx.sp[0], (long)&mh, 0, 0, 0, 0, SYS_recvmsg); while (r == -EINTR);
        *ok = r == 1;
        break;
    }
    case RM_WAIT4: {
        int st = 0;
        do RM_CALL(rmx.child, (long)&st, 0, 0, 0, 0, SYS_wait4); while (r == -EINTR);
        rmx.reaped = r == rmx.child;
        *ok = r == rmx.child && WIFEXITED(st) && WEXITSTATUS(st) == 5;
        break;
    }
    case RM_FLOCK:
        do RM_CALL(rmx.fd2, LOCK_EX, 0, 0, 0, 0, SYS_flock); while (r == -EINTR);
        *ok = r == 0;
        break;
    case RM_SIGWAIT: {
        uint64_t set = SIGBIT(SIGRTMIN + 5);
        siginfo_t info;
        memset(&info, 0, sizeof info);
        struct timespec limit = {5, 0}; /* bounds a signal the wait does not see */
        do RM_CALL((long)&set, (long)&info, (long)&limit, 8, 0, 0, SYS_rt_sigtimedwait); while (r == -EINTR);
        *ok = r == SIGRTMIN + 5 && info.si_code == SI_TKILL;
        break;
    }
    case RM_SEMOP: {
        struct sembuf b = {0, -1, 0};
        do RM_CALL(rmx.semid, (long)&b, 1, 0, 0, 0, SYS_semop); while (r == -EINTR);
        *ok = r == 0;
        break;
    }
    case RM_URING:
        for (;;) {
            uint32_t head = __atomic_load_n((uint32_t *)(rmx.cq + rmx.up.cq_off.head), __ATOMIC_ACQUIRE);
            uint32_t tail = __atomic_load_n((uint32_t *)(rmx.cq + rmx.up.cq_off.tail), __ATOMIC_ACQUIRE);
            if (head != tail) {
                struct io_uring_cqe *e = (struct io_uring_cqe *)(rmx.cq + rmx.up.cq_off.cqes) +
                                         (head & *(uint32_t *)(rmx.cq + rmx.up.cq_off.ring_mask));
                *ok = e->user_data == 77 && e->res == 1;
                __atomic_store_n((uint32_t *)(rmx.cq + rmx.up.cq_off.head), head + 1, __ATOMIC_RELEASE);
                break;
            }
            RM_CALL(rmx.ring, 0, 1, IORING_ENTER_GETEVENTS, 0, 0, 426);
            if (r < 0 && r != -EINTR) break;
        }
        break;
    default: { /* pause(): sigsuspend with the current mask */
        uint64_t cur = kmask_get();
        RM_CALL((long)&cur, 8, 0, 0, 0, 0, SYS_rt_sigsuspend);
        *ok = r == -EINTR;
    }
    }
#undef RM_CALL
    return first;
}
static int t_restart(void) {
    rm_stream = malloc((1 << 20) + RM_WTOTAL);
    for (size_t i = 0; i < (1 << 20) + RM_WTOTAL; i++) rm_stream[i] = (unsigned char)(i % 251);
    rm_tid = gettid();
    struct sigaction a2;
    memset(&a2, 0, sizeof a2);
    a2.sa_handler = rm_handler2;
    a2.sa_flags = SA_RESTART;
    sigaction(SIGUSR2, &a2, NULL);
    pthread_t h;
    pthread_create(&h, 0, rm_helper, 0);
    int go = 0;
    for (int c = 0; c < RM_N; c++) {
        char words[3][32];
        int okall = 1;
        for (int mode = 0; mode < 3; mode++) {
            struct sigaction a;
            memset(&a, 0, sizeof a);
            a.sa_sigaction = rm_handler;
            a.sa_flags = SA_SIGINFO | (mode ? SA_RESTART : 0);
            sigaction(SIGUSR1, &a, NULL);
            rm_nested = mode == 2;
            if (!rm_setup(c)) { snprintf(words[mode], sizeof words[mode], "absent"); continue; }
            rm_robs = rm_eobs = rm_usr1 = rm_usr2 = 0;
            rm_main_done = 0;
            rm_stream_ok = 1;
            rm_case = c;
            __atomic_store_n(&rm_go, ++go, __ATOMIC_RELEASE);
            cfz_wake(&rm_go);
            int ok;
            long first = rm_run(c, &ok);
            __atomic_store_n(&rm_main_done, 1, __ATOMIC_RELEASE);
            int d;
            while ((d = __atomic_load_n(&rm_done, __ATOMIC_ACQUIRE)) != go) cfz_wait(&rm_done, d, 100);
            rm_teardown(c);
            if (first == -EINTR) snprintf(words[mode], sizeof words[mode], "eintr");
            else if (first >= 0) snprintf(words[mode], sizeof words[mode], "%s", rm_robs >= 3 && !rm_eobs ? "restarted" : "done");
            else snprintf(words[mode], sizeof words[mode], "err%ld", -first);
            if (c == RM_WRITE) ok &= rm_stream_ok;
            if (mode == 2) ok &= rm_usr2 == rm_usr1 && rm_usr1 > 0;
            okall &= ok;
        }
        OUT(rm_names[c], "plain %s restart %s nested %s completed %d", words[0], words[1], words[2], okall);
    }
    __atomic_store_n(&rm_quit, 1, __ATOMIC_RELEASE);
    __atomic_store_n(&rm_go, ++go, __ATOMIC_RELEASE);
    cfz_wake(&rm_go);
    pthread_join(h, 0);
    signal(SIGUSR1, SIG_DFL);
    signal(SIGUSR2, SIG_DFL);
    /* restart_syscall: timed waits in a child that is stopped and continued
     * repeatedly complete their whole time without EINTR. */
    struct rs { long r[5]; int el[5]; } *sh = mmap(0, 4096, PROT_READ | PROT_WRITE, MAP_SHARED | MAP_ANONYMOUS, -1, 0);
    memset(sh, 0, sizeof *sh);
    int pp[2];
    if (pipe(pp)) return 0;
    pid_t c = fork();
    if (!c) {
        static uint32_t fw;
        for (int i = 0; i < 5; i++) {
            uint64_t t0 = mono_ns(CLOCK_MONOTONIC), end = t0 + 150000000ULL;
            struct timespec rel = {0, 150000000}, dl = {(time_t)(end / 1000000000ULL), (long)(end % 1000000000ULL)};
            struct pollfd pf = {pp[0], POLLIN, 0};
            fd_set rf;
            FD_ZERO(&rf);
            FD_SET(pp[0], &rf);
            long r;
            switch (i) {
            case 0: r = cfz_rawsys((long)&rel, 0, 0, 0, 0, 0, SYS_nanosleep); break;
            case 1: r = cfz_rawsys(CLOCK_MONOTONIC, TIMER_ABSTIME, (long)&dl, 0, 0, 0, SYS_clock_nanosleep); break;
            case 2: r = cfz_rawsys((long)&fw, FUTEX_WAIT_PRIVATE, 0, (long)&rel, 0, 0, SYS_futex); break;
            case 3: r = cfz_rawsys((long)&pf, 1, (long)&rel, 0, 8, 0, SYS_ppoll); break;
            default: r = cfz_rawsys(pp[0] + 1, (long)&rf, 0, 0, (long)&rel, 0, SYS_pselect6); break;
            }
            sh->r[i] = r;
            sh->el[i] = mono_ns(CLOCK_MONOTONIC) - t0 >= 150000000ULL;
        }
        _exit(0);
    }
    long stops = 0;
    for (;;) {
        siginfo_t si;
        cfz_sleep_us(3000 + (long)(next() % 6000));
        if (kill(c, SIGSTOP)) break;
        memset(&si, 0, sizeof si);
        if (waitid(P_PID, c, &si, WSTOPPED | WEXITED) || si.si_code != CLD_STOPPED) break;
        stops++;
        cfz_sleep_us(1000 + (long)(next() % 3000));
        kill(c, SIGCONT);
        memset(&si, 0, sizeof si);
        if (waitid(P_PID, c, &si, WCONTINUED | WEXITED) || si.si_code != CLD_CONTINUED) break;
    }
    waitpid(c, NULL, WNOHANG);
    close(pp[0]);
    close(pp[1]);
    OUT("stopContinue", "nanosleep %ld %d clockAbs %ld %d futex %ld %d ppoll %ld %d pselect6 %ld %d stopped %d", sh->r[0],
        sh->el[0], sh->r[1], sh->el[1], sh->r[2], sh->el[2], sh->r[3], sh->el[3], sh->r[4], sh->el[4], stops > 0);
    return 1;
}

/* det: thread-directed realtime signals to a thread blocked in read while two
 * threads flush code: exactly once, FIFO per signal number within each sender's
 * batch; a batch queued while blocked is delivered lowest number first. */
#define RF_BATCH 16
#define RF_SIGS 4
#define RF_BLOCKED 24
struct rf_ev { int sig, val, code, pid; };
static struct rf_ev rf_log[4096];
static volatile int rf_n, rf_token, rf_tready, rf_phase, rf_release, rf_readres;
static pid_t rf_tid;
static unsigned char rf_plan[4096];
static long rf_batches, rf_rounds2;
static int rf_pipe[2];
static void rf_handler(int s, siginfo_t *si, void *uc) {
    (void)uc;
    int i = rf_n;
    if (i < 4096) {
        rf_log[i].sig = s;
        rf_log[i].val = si->si_value.sival_int;
        rf_log[i].code = si->si_code;
        rf_log[i].pid = si->si_pid;
    }
    __atomic_store_n(&rf_n, i + 1, __ATOMIC_RELEASE);
    cfz_wake(&rf_n);
}
static void rf_send(int sig, int val) {
    for (int k = 0; k < 100000; k++) {
        if (!cfz_queue(rf_tid, sig, val) || errno != EAGAIN) return;
        cfz_sleep_us(100);
    }
}
static void *rf_sender(void *p) {
    long me = (long)p;
    for (long b = me; b < rf_batches; b += 2) {
        int t;
        while ((t = __atomic_load_n(&rf_token, __ATOMIC_ACQUIRE)) != b) cfz_wait(&rf_token, t, 100);
        for (int j = 0; j < RF_BATCH; j++) rf_send(SIGRTMIN + rf_plan[b * RF_BATCH + j], (int)(b * RF_BATCH + j));
        __atomic_store_n(&rf_token, (int)b + 1, __ATOMIC_RELEASE);
        cfz_wake(&rf_token);
    }
    return NULL;
}
static void *rf_target(void *p) {
    (void)p;
    rf_tid = gettid();
    __atomic_store_n(&rf_tready, 1, __ATOMIC_RELEASE);
    cfz_wake(&rf_tready);
    char b;
    ssize_t r = read(rf_pipe[0], &b, 1);
    rf_readres = r == 1 ? 1 : r < 0 ? -errno : 0;
    sigset_t s;
    sigemptyset(&s);
    for (int k = 0; k < RF_SIGS; k++) sigaddset(&s, SIGRTMIN + k);
    for (long k = 0; k < rf_rounds2; k++) {
        pthread_sigmask(SIG_BLOCK, &s, NULL);
        __atomic_store_n(&rf_phase, (int)(2 * k + 1), __ATOMIC_RELEASE);
        cfz_wake(&rf_phase);
        int v;
        while ((v = __atomic_load_n(&rf_release, __ATOMIC_ACQUIRE)) != k + 1) cfz_wait(&rf_release, v, 100);
        pthread_sigmask(SIG_UNBLOCK, &s, NULL);
        __atomic_store_n(&rf_phase, (int)(2 * k + 2), __ATOMIC_RELEASE);
        cfz_wake(&rf_phase);
    }
    return NULL;
}
static int t_rtfifo(void) {
    rf_batches = 40 * scale;
    if (rf_batches * RF_BATCH > 4096) rf_batches = 4096 / RF_BATCH;
    rf_rounds2 = 20 * scale;
    long total = rf_batches * RF_BATCH;
    for (long i = 0; i < total; i++) rf_plan[i] = (unsigned char)(next() % RF_SIGS);
    if (pipe(rf_pipe)) return 0;
    struct sigaction a;
    memset(&a, 0, sizeof a);
    a.sa_sigaction = rf_handler;
    a.sa_flags = SA_SIGINFO | SA_RESTART;
    sigemptyset(&a.sa_mask);
    for (int k = 0; k < RF_SIGS; k++) sigaddset(&a.sa_mask, SIGRTMIN + k);
    for (int k = 0; k < RF_SIGS; k++) sigaction(SIGRTMIN + k, &a, NULL);
    static struct jitw jw[2];
    pthread_t fl[2], tt, s0, s1;
    fl_stop = 0;
    for (int k = 0; k < 2; k++) { jit_init(&jw[k]); pthread_create(&fl[k], 0, flusher, &jw[k]); }
    pthread_create(&tt, 0, rf_target, 0);
    cfz_wait_for(&rf_tready, 1, 10000);
    pthread_create(&s0, 0, rf_sender, (void *)0L);
    pthread_create(&s1, 0, rf_sender, (void *)1L);
    pthread_join(s0, 0);
    pthread_join(s1, 0);
    cfz_wait_for(&rf_n, (int)total, 20000);
    int n = rf_n;
    if (write(rf_pipe[1], "x", 1) != 1) return 0;
    static unsigned char seen[4096];
    static int lastj[4096 / RF_BATCH][RF_SIGS];
    memset(lastj, 0xff, sizeof lastj);
    long once = 0, dup = 0, foreign = 0, codeok = 1, pidok = 1, fifo = 1;
    for (int i = 0; i < n && i < 4096; i++) {
        struct rf_ev *e = &rf_log[i];
        int v = e->val;
        if (v < 0 || v >= total || e->sig != SIGRTMIN + rf_plan[v]) { foreign++; continue; }
        if (seen[v]++) dup++;
        codeok &= e->code == SI_QUEUE;
        pidok &= e->pid == getpid();
        int b = v / RF_BATCH, j = v % RF_BATCH, s = rf_plan[v];
        if (j <= lastj[b][s]) fifo = 0;
        lastj[b][s] = j;
    }
    for (long v = 0; v < total; v++) once += seen[v] == 1;
    /* Blocked arm: a full batch while the target blocks the signals, then unblock. */
    long ordered = 0;
    for (long k = 0; k < rf_rounds2; k++) {
        if (!cfz_wait_for(&rf_phase, (int)(2 * k + 1), 20000)) break;
        __atomic_store_n(&rf_n, 0, __ATOMIC_RELEASE);
        unsigned char plan[RF_BLOCKED];
        int expect[RF_BLOCKED], m = 0;
        for (int j = 0; j < RF_BLOCKED; j++) plan[j] = (unsigned char)(next() % RF_SIGS);
        for (int j = 0; j < RF_BLOCKED; j++) rf_send(SIGRTMIN + plan[j], j);
        for (int s = 0; s < RF_SIGS; s++)
            for (int j = 0; j < RF_BLOCKED; j++)
                if (plan[j] == s) expect[m++] = j;
        __atomic_store_n(&rf_release, (int)k + 1, __ATOMIC_RELEASE);
        cfz_wake(&rf_release);
        cfz_wait_for(&rf_phase, (int)(2 * k + 2), 20000);
        cfz_wait_for(&rf_n, RF_BLOCKED, 5000);
        int good = rf_n == RF_BLOCKED;
        for (int i = 0; good && i < RF_BLOCKED; i++)
            good = rf_log[i].val == expect[i] && rf_log[i].sig == SIGRTMIN + plan[expect[i]] && rf_log[i].code == SI_QUEUE;
        ordered += good;
    }
    __atomic_store_n(&fl_stop, 1, __ATOMIC_RELEASE);
    for (int k = 0; k < 2; k++) pthread_join(fl[k], 0);
    pthread_join(tt, 0);
    for (int k = 0; k < RF_SIGS; k++) signal(SIGRTMIN + k, SIG_DFL);
    close(rf_pipe[0]);
    close(rf_pipe[1]);
    OUT("delivered", "%ld/%ld", once, total);
    OUT("duplicates", "%ld", dup);
    OUT("foreign", "%ld", foreign);
    OUT("fifoPerSignal", "%ld", fifo);
    OUT("siQueue", "%ld", codeok);
    OUT("siPid", "%ld", pidok);
    OUT("readCompleted", "%d", rf_readres);
    OUT("blockedLowestFirst", "%ld/%ld", ordered, rf_rounds2);
    OUT("flushStale", "%ld", jw[0].stale + jw[1].stale);
    OUT("flushRan", "%d", jw[0].calls > 0 && jw[1].calls > 0);
    return 1;
}

/* det: realtime queue pressure. Every accepted payload arrives exactly once (how
 * many are accepted depends on the host and the translator and is not printed);
 * signals pending at fork stay with the parent; signalfd drains a deep queue. */
#define SP_N 10000
static unsigned char sp_arr[SP_N], *sp_acc;
static volatile int sp_total, sp_bad, sp_slow, sp_logn;
static int sp_log[1024];
static pid_t sp_tid;
static void sp_handler(int s, siginfo_t *si, void *uc) {
    (void)s; (void)uc;
    int v = si->si_value.sival_int;
    if (si->si_code != SI_QUEUE || v < 0 || v >= SP_N) sp_bad++;
    else if (sp_arr[v] < 255) sp_arr[v]++;
    if (sp_slow) fixed_work(sp_slow);
    __atomic_add_fetch(&sp_total, 1, __ATOMIC_SEQ_CST);
}
static void sp_tag_handler(int s, siginfo_t *si, void *uc) {
    (void)s; (void)uc;
    int n = sp_logn;
    if (n < 1024) sp_log[n] = si->si_value.sival_int;
    sp_logn = n + 1;
}
static void *sp_sender(void *p) {
    (void)p;
    for (int i = 0; i < SP_N; i++) sp_acc[i] = cfz_queue(sp_tid, SIGRTMIN + 1, i) == 0;
    return NULL;
}
static void sp_reset(void) {
    memset(sp_arr, 0, sizeof sp_arr);
    memset(sp_acc, 0, SP_N);
    sp_total = sp_bad = 0;
}
static long sp_accepted(void) {
    long n = 0;
    for (int i = 0; i < SP_N; i++) n += sp_acc[i];
    return n;
}
static void sp_settle(long accepted) {
    uint64_t end = mono_ns(CLOCK_MONOTONIC) + 30000000000ULL;
    while (sp_total < accepted && mono_ns(CLOCK_MONOTONIC) < end) cfz_sleep_us(1000);
    cfz_sleep_us(20000);
}
static void sp_report(const char *name) {
    long acc = sp_accepted(), lost = 0, dup = 0, extra = 0;
    for (int i = 0; i < SP_N; i++) {
        lost += sp_acc[i] && !sp_arr[i];
        dup += sp_arr[i] > 1;
        extra += !sp_acc[i] && sp_arr[i];
    }
    OUT(name, "lost %ld duplicated %ld unaccepted %ld foreign %d someAccepted %d", lost, dup, extra, (int)sp_bad, acc > 0);
}
static void sp_drain_log(int want) {
    uint64_t end = mono_ns(CLOCK_MONOTONIC) + 10000000000ULL;
    while (sp_logn < want && mono_ns(CLOCK_MONOTONIC) < end) cfz_sleep_us(1000);
    cfz_sleep_us(10000);
}
static void sp_tags(int tag, int *got, int *order, int *foreign) {
    *got = 0; *order = 1; *foreign = 0;
    for (int i = 0; i < sp_logn && i < 1024; i++) {
        if (sp_log[i] >> 16 != tag) { (*foreign)++; continue; }
        *order &= (sp_log[i] & 0xffff) == *got;
        (*got)++;
    }
}
static int t_sigpressure(void) {
    sp_acc = mmap(0, SP_N, PROT_READ | PROT_WRITE, MAP_SHARED | MAP_ANONYMOUS, -1, 0);
    sp_tid = gettid();
    struct rlimit orig, lim;
    getrlimit(RLIMIT_SIGPENDING, &orig);
    rlim_t high = orig.rlim_max == RLIM_INFINITY || orig.rlim_max >= 30000 ? 30000 : orig.rlim_max;
    struct sigaction a;
    memset(&a, 0, sizeof a);
    a.sa_sigaction = sp_handler;
    a.sa_flags = SA_SIGINFO | SA_RESTART;
    sigemptyset(&a.sa_mask);
    sigaction(SIGRTMIN + 1, &a, NULL);
    sigset_t one;
    sigemptyset(&one);
    sigaddset(&one, SIGRTMIN + 1);
    pthread_t t;
    /* (a) 10,000 from another thread while blocked, with a low limit. */
    lim.rlim_cur = 64;
    lim.rlim_max = orig.rlim_max;
    setrlimit(RLIMIT_SIGPENDING, &lim);
    sp_reset();
    sp_slow = 0;
    pthread_sigmask(SIG_BLOCK, &one, NULL);
    pthread_create(&t, 0, sp_sender, 0);
    pthread_join(t, 0);
    pthread_sigmask(SIG_UNBLOCK, &one, NULL);
    sp_settle(sp_accepted());
    sp_report("threadBlockedLowLimit");
    /* ... faster than a slow handler drains them, with a high limit. */
    lim.rlim_cur = high;
    setrlimit(RLIMIT_SIGPENDING, &lim);
    sp_reset();
    sp_slow = 300;
    pthread_create(&t, 0, sp_sender, 0);
    pthread_join(t, 0);
    sp_settle(sp_accepted());
    sp_report("threadSlowHandler");
    /* ... process-directed from a child. */
    sp_reset();
    pid_t c = fork();
    if (!c) {
        pid_t parent = getppid();
        for (int i = 0; i < SP_N; i++) {
            union sigval v = {.sival_int = i};
            sp_acc[i] = sigqueue(parent, SIGRTMIN + 1, v) == 0;
        }
        _exit(0);
    }
    waitpid(c, NULL, 0);
    sp_settle(sp_accepted());
    sp_report("childSlowHandler");
    sp_slow = 0;
    signal(SIGRTMIN + 1, SIG_DFL);
    /* (b) 254..257 queued while blocked before fork; the child gets its own tags. */
    struct sigaction b;
    memset(&b, 0, sizeof b);
    b.sa_sigaction = sp_tag_handler;
    b.sa_flags = SA_SIGINFO | SA_RESTART;
    sigemptyset(&b.sa_mask);
    sigaction(SIGRTMIN + 2, &b, NULL);
    sigset_t two;
    sigemptyset(&two);
    sigaddset(&two, SIGRTMIN + 2);
    const int counts[4] = {254, 255, 256, 257};
    for (int ci = 0; ci < 4; ci++) {
        int n = counts[ci], pa = 0, ca = 0;
        pthread_sigmask(SIG_BLOCK, &two, NULL);
        for (int i = 0; i < n; i++) {
            union sigval v = {.sival_int = 0x10000 | i};
            pa += sigqueue(getpid(), SIGRTMIN + 2, v) == 0;
        }
        int go[2], res[2];
        if (pipe(go) || pipe(res)) return 0;
        c = fork();
        if (!c) {
            int want, out[3];
            if (read(go[0], &want, sizeof want) != sizeof want) _exit(1);
            sp_logn = 0;
            pthread_sigmask(SIG_UNBLOCK, &two, NULL);
            sp_drain_log(want);
            sp_tags(2, &out[0], &out[1], &out[2]);
            _exit(write(res[1], out, sizeof out) == sizeof out ? 0 : 1);
        }
        for (int i = 0; i < n; i++) {
            union sigval v = {.sival_int = 0x20000 | i};
            ca += sigqueue(c, SIGRTMIN + 2, v) == 0;
        }
        if (write(go[1], &ca, sizeof ca) != sizeof ca) return 0;
        sp_logn = 0;
        pthread_sigmask(SIG_UNBLOCK, &two, NULL);
        sp_drain_log(pa);
        int pg, po, pf, out[3] = {-1, -1, -1};
        sp_tags(1, &pg, &po, &pf);
        if (read(res[0], out, sizeof out) != sizeof out) {}
        waitpid(c, NULL, 0);
        close(go[0]); close(go[1]); close(res[0]); close(res[1]);
        OUT("fork", "%d parent %d/%d inOrder %d foreign %d child %d/%d inOrder %d foreign %d", n, pg, pa, po, pf, out[0], ca,
            out[1], out[2]);
    }
    signal(SIGRTMIN + 2, SIG_DFL);
    /* (c) more than 512 signalfd records, mixed read sizes, then EAGAIN. */
    sigset_t three;
    sigemptyset(&three);
    sigaddset(&three, SIGRTMIN + 3);
    pthread_sigmask(SIG_BLOCK, &three, NULL);
    int acc = 0;
    for (int i = 0; i < 600; i++) {
        union sigval v = {.sival_int = i};
        acc += sigqueue(getpid(), SIGRTMIN + 3, v) == 0;
    }
    int sfd = signalfd(-1, &three, SFD_NONBLOCK);
    const int sizes[4] = {1, 7, 3, 16};
    struct signalfd_siginfo buf[16];
    int got = 0, order = 1, eagain = 0;
    for (int k = 0; k < 100000; k++) {
        ssize_t r = read(sfd, buf, (size_t)sizes[k % 4] * sizeof buf[0]);
        if (r < 0) { eagain = errno == EAGAIN; break; }
        if (r % (ssize_t)sizeof buf[0]) order = 0;
        for (ssize_t j = 0; j < r / (ssize_t)sizeof buf[0]; j++) {
            order &= buf[j].ssi_int == got && buf[j].ssi_code == SI_QUEUE;
            got++;
        }
    }
    close(sfd);
    pthread_sigmask(SIG_UNBLOCK, &three, NULL);
    OUT("signalfd", "%d/%d inOrder %d thenEagain %d", got, acc, order, eagain);
    setrlimit(RLIMIT_SIGPENDING, &orig);
    return 1;
}

/* det: with SIGUSR1 and SIGUSR2 blocked and pending, a temporary mask that unblocks
 * SIGUSR1 delivers exactly it when the call is interrupted, and nothing when the call
 * returns normally or rejects its arguments; the original mask is always restored. */
static volatile int tm_h1, tm_h2;
static void tm_handler(int s) {
    if (s == SIGUSR1) tm_h1++;
    else tm_h2++;
}
enum { TM_SUSPEND, TM_PPOLL, TM_PSELECT, TM_EPOLL, TM_PPOLL_READY, TM_PSELECT_READY, TM_EPOLL_READY, TM_PPOLL_BOTH,
       TM_PSELECT_BOTH, TM_EPOLL_BOTH, TM_PPOLL_BAD, TM_PSELECT_BAD, TM_EPOLL_BAD, TM_SUSPEND_BAD, TM_N };
static const char *const tm_names[TM_N] = {
    "sigsuspend", "ppoll", "pselect6", "epollPwait2", "ppollReady", "pselect6Ready", "epollPwait2Ready",
    "ppollReadyBothPending", "pselect6ReadyBothPending", "epollPwait2ReadyBothPending", "ppollBadTimeout",
    "pselect6BadTimeout", "epollPwait2BadFd", "sigsuspendBadSize"};
static const char *ename(int e) {
    switch (e) {
    case 0: return "-";
    case EINTR: return "EINTR";
    case EINVAL: return "EINVAL";
    case EBADF: return "EBADF";
    case ENOSYS: return "ENOSYS";
    default: return "other";
    }
}
static int t_tmpmask(void) {
    struct sigaction a;
    memset(&a, 0, sizeof a);
    a.sa_handler = tm_handler;
    sigemptyset(&a.sa_mask);
    sigaction(SIGUSR1, &a, NULL);
    sigaction(SIGUSR2, &a, NULL);
    int empty[2];
    if (pipe(empty)) return 0;
    int ready = eventfd(1, 0), epE = epoll_create1(0), epR = epoll_create1(0);
    struct epoll_event e = {.events = EPOLLIN};
    epoll_ctl(epE, EPOLL_CTL_ADD, empty[0], &e);
    epoll_ctl(epR, EPOLL_CTL_ADD, ready, &e);
    uint64_t both = SIGBIT(SIGUSR1) | SIGBIT(SIGUSR2);
    static char first[TM_N][160];
    long same = 0, rounds = 20 * scale;
    for (long round = 0; round < rounds; round++) {
        for (int v = 0; v < TM_N; v++) {
            syscall(SYS_rt_sigprocmask, SIG_BLOCK, &both, NULL, 8);
            uint64_t orig = kmask_get(), temp = orig & ~SIGBIT(SIGUSR1);
            tm_h1 = tm_h2 = 0;
            if (v < TM_PPOLL_READY || v > TM_EPOLL_READY) syscall(SYS_tgkill, getpid(), gettid(), SIGUSR1);
            syscall(SYS_tgkill, getpid(), gettid(), SIGUSR2);
            int rdy = v >= TM_PPOLL_READY && v <= TM_EPOLL_BOTH, fd = rdy ? ready : empty[0], ep = rdy ? epR : epE;
            struct pollfd pf = {fd, POLLIN, 0};
            fd_set rf;
            FD_ZERO(&rf);
            FD_SET(fd, &rf);
            /* The interrupted forms return at once; the timeout only bounds a lost signal. */
            struct timespec bad = {0, -1}, limit = {1, 0};
            struct { const uint64_t *ss; size_t len; } psm = {&temp, 8};
            struct epoll_event out;
            long r;
            switch (v) {
            case TM_SUSPEND: r = syscall(SYS_rt_sigsuspend, &temp, 8); break;
            case TM_PPOLL: case TM_PPOLL_READY: case TM_PPOLL_BOTH: r = syscall(SYS_ppoll, &pf, 1, &limit, &temp, 8); break;
            case TM_PSELECT: case TM_PSELECT_READY: case TM_PSELECT_BOTH:
                r = syscall(SYS_pselect6, fd + 1, &rf, NULL, NULL, &limit, &psm);
                break;
            case TM_EPOLL: case TM_EPOLL_READY: case TM_EPOLL_BOTH: r = syscall(441, ep, &out, 1, &limit, &temp, 8); break;
            case TM_PPOLL_BAD: r = syscall(SYS_ppoll, &pf, 1, &bad, &temp, 8); break;
            case TM_PSELECT_BAD: r = syscall(SYS_pselect6, fd + 1, &rf, NULL, NULL, &bad, &psm); break;
            case TM_EPOLL_BAD: r = syscall(441, -1, &out, 1, NULL, &temp, 8); break;
            default: r = syscall(SYS_rt_sigsuspend, &temp, 4); break;
            }
            int err = r < 0 ? errno : 0, h1 = tm_h1, h2 = tm_h2;
            uint64_t mask = kmask_get(), pend = kpending();
            char line[160];
            snprintf(line, sizeof line, "ret %ld %s h1 %d h2 %d maskRestored %d pending %s%s%s", r, ename(err), h1, h2,
                     mask == orig, pend & SIGBIT(SIGUSR1) ? "1" : "", pend & SIGBIT(SIGUSR2) ? "2" : "",
                     pend & both ? "" : "none");
            syscall(SYS_rt_sigprocmask, SIG_UNBLOCK, &both, NULL, 8);
            if (round == 0) strcpy(first[v], line);
            same += !strcmp(first[v], line);
        }
    }
    signal(SIGUSR1, SIG_DFL);
    signal(SIGUSR2, SIG_DFL);
    for (int v = 0; v < TM_N; v++) OUT(tm_names[v], "%s", first[v]);
    OUT("same", "%ld/%ld", same, rounds * TM_N);
    return 1;
}

/* det: application SIGILL, SIGSTKFLT and SIGUSR2 (the signals DynamoRIO and HotSpot
 * use internally) queued with values and paced by acknowledgment while a thread
 * flushes code; genuine SIGILL and SIGTRAP faults interleave. */
static const int ss_sigs[3] = {SIGILL, SIGSTKFLT, SIGUSR2};
static volatile int ss_ack, ss_gen_req, ss_gen_done, ss_done;
static long ss_got[3], ss_inorder[3], ss_foreign, ss_ill, ss_trap, ss_unacked;
static int ss_next[3], ss_foreign_sig, ss_foreign_code;
static pid_t ss_tid;
static void share_handler(int s, siginfo_t *si, void *ucv) {
    ucontext_t *uc = ucv;
    uintptr_t pc = uc->uc_mcontext.pc;
    if (si->si_code == SI_QUEUE && si->si_pid == getpid()) {
        int k = s == SIGILL ? 0 : s == SIGSTKFLT ? 1 : s == SIGUSR2 ? 2 : -1;
        if (k < 0) ss_foreign++;
        else {
            ss_got[k]++;
            ss_inorder[k] += si->si_value.sival_int == ss_next[k];
            ss_next[k] = si->si_value.sival_int + 1;
        }
        __atomic_add_fetch(&ss_ack, 1, __ATOMIC_SEQ_CST);
        cfz_wake(&ss_ack);
        return;
    }
    if (s == SIGILL && si->si_code == ILL_ILLOPC && pc == (uintptr_t)cfz_sf_udf_at) { ss_ill++; uc->uc_mcontext.pc = pc + 4; return; }
    if (s == SIGTRAP && si->si_code == TRAP_BRKPT && pc == (uintptr_t)cfz_sf_brk_at) { ss_trap++; uc->uc_mcontext.pc = pc + 4; return; }
    if (!ss_foreign++) { ss_foreign_sig = s; ss_foreign_code = si->si_code; }
    if (pc == (uintptr_t)cfz_sf_udf_at || pc == (uintptr_t)cfz_sf_brk_at) uc->uc_mcontext.pc = pc + 4;
}
static void *ss_sender(void *p) {
    long n = (long)p;
    for (long i = 0; i < n && ss_unacked < 4; i++) { /* a few lost signals are enough */
        for (int k = 0; k < 3; k++) {
            int a = __atomic_load_n(&ss_ack, __ATOMIC_ACQUIRE);
            if (cfz_queue(ss_tid, ss_sigs[k], (int)i)) { ss_unacked++; continue; }
            uint64_t end = mono_ns(CLOCK_MONOTONIC) + 5000000000ULL;
            while (__atomic_load_n(&ss_ack, __ATOMIC_ACQUIRE) == a) {
                if (mono_ns(CLOCK_MONOTONIC) > end) { ss_unacked++; break; }
                cfz_wait(&ss_ack, a, 10);
            }
        }
        if (i % 25 == 0) { /* ask the main thread for a genuine SIGILL and SIGTRAP */
            int g = (int)(i / 25) + 1;
            __atomic_store_n(&ss_gen_req, g, __ATOMIC_RELEASE);
            cfz_wait_for(&ss_gen_done, g, 10000);
        }
    }
    __atomic_store_n(&ss_done, 1, __ATOMIC_RELEASE);
    return NULL;
}
static int t_sigshare(void) {
    ss_tid = gettid();
    struct sigaction a;
    memset(&a, 0, sizeof a);
    a.sa_sigaction = share_handler;
    a.sa_flags = SA_SIGINFO | SA_RESTART;
    sigemptyset(&a.sa_mask);
    const int all[4] = {SIGILL, SIGSTKFLT, SIGUSR2, SIGTRAP};
    for (int k = 0; k < 4; k++) sigaction(all[k], &a, NULL);
    static struct jitw jw;
    jit_init(&jw);
    fl_stop = 0;
    pthread_t fl, snd;
    pthread_create(&fl, 0, flusher, &jw);
    long n = 5000 * scale, genuine = 0;
    pthread_create(&snd, 0, ss_sender, (void *)n);
    while (!__atomic_load_n(&ss_done, __ATOMIC_ACQUIRE)) {
        int g = __atomic_load_n(&ss_gen_req, __ATOMIC_ACQUIRE);
        if (g != ss_gen_done) {
            uint64_t d;
            cfz_sf_udf(NULL, 0, &d);
            cfz_sf_brk(NULL, 0, &d);
            genuine++;
            __atomic_store_n(&ss_gen_done, g, __ATOMIC_RELEASE);
            cfz_wake(&ss_gen_done);
        } else fixed_work(2000);
    }
    pthread_join(snd, 0);
    __atomic_store_n(&fl_stop, 1, __ATOMIC_RELEASE);
    pthread_join(fl, 0);
    for (int k = 0; k < 4; k++) signal(all[k], SIG_DFL);
    const char *names[3] = {"SIGILL", "SIGSTKFLT", "SIGUSR2"};
    for (int k = 0; k < 3; k++) OUT(names[k], "%ld/%ld inOrder %ld", ss_got[k], n, ss_inorder[k]);
    OUT("unacked", "%ld", ss_unacked);
    OUT("foreign", "%ld", ss_foreign);
    if (ss_foreign) OUT("firstForeign", "%d %d", ss_foreign_sig, ss_foreign_code);
    OUT("genuineIll", "%ld/%ld", ss_ill, genuine);
    OUT("genuineTrap", "%ld/%ld", ss_trap, genuine);
    OUT("flushStale", "%ld", jw.stale);
    return 1;
}

/* det: a child with JIT writers, compute threads and timed waiters is stopped and
 * continued repeatedly, with paced SIGUSR1 in between; timed waits never return early
 * or with EINTR, counts are conserved. */
struct sst {
    volatile int stop, ready, usr1;
    long jitCalls[2], jitStale[2], sums[2], sumBad[2], timed[4], early[4], eintr[4], wrong[4];
};
static struct sst *st_sh;
static void st_usr1(int s) { (void)s; __atomic_add_fetch(&st_sh->usr1, 1, __ATOMIC_SEQ_CST); }
static void *st_worker(void *p) {
    long k = (long)p;
    sigset_t u;
    sigemptyset(&u);
    sigaddset(&u, SIGUSR1);
    pthread_sigmask(k < 4 ? SIG_UNBLOCK : SIG_BLOCK, &u, NULL);
    struct jitw j;
    int pp[2];
    uint32_t fw = 0;
    uint64_t want = 0;
    if (k < 2) jit_init(&j);
    if (k == 6 && pipe(pp)) return NULL;
    __atomic_add_fetch(&st_sh->ready, 1, __ATOMIC_SEQ_CST);
    for (long i = 0; !__atomic_load_n(&st_sh->stop, __ATOMIC_ACQUIRE); i++) {
        if (k < 2) { jit_step(&j, i); continue; }
        if (k < 4) {
            uint64_t s = 0;
            for (volatile uint64_t x = 0; x < 20000; x++) s += (x * x) ^ (uint64_t)k;
            if (!want) want = s;
            st_sh->sums[k - 2]++;
            st_sh->sumBad[k - 2] += s != want;
            continue;
        }
        uint64_t t0 = mono_ns(CLOCK_MONOTONIC), need = 2000000;
        long r, expect = 0;
        struct timespec t = {0, 2000000};
        switch (k) {
        case 4:
            t.tv_nsec = 3000000;
            need = 3000000;
            expect = -ETIMEDOUT;
            r = cfz_rawsys((long)&fw, FUTEX_WAIT_PRIVATE, 0, (long)&t, 0, 0, SYS_futex);
            break;
        case 5: r = cfz_rawsys((long)&t, 0, 0, 0, 0, 0, SYS_nanosleep); break;
        case 6: {
            struct pollfd pf = {pp[0], POLLIN, 0};
            r = cfz_rawsys((long)&pf, 1, (long)&t, 0, 8, 0, SYS_ppoll);
            break;
        }
        default: {
            uint64_t end = t0 + need;
            struct timespec dl = {(time_t)(end / 1000000000ULL), (long)(end % 1000000000ULL)};
            r = cfz_rawsys(CLOCK_MONOTONIC, TIMER_ABSTIME, (long)&dl, 0, 0, 0, SYS_clock_nanosleep);
        }
        }
        uint64_t el = mono_ns(CLOCK_MONOTONIC) - t0;
        int x = (int)k - 4;
        st_sh->timed[x]++;
        if (r == -EINTR) st_sh->eintr[x]++;
        else if (r != expect) st_sh->wrong[x]++;
        else if (el < need) st_sh->early[x]++;
    }
    if (k < 2) { st_sh->jitCalls[k] = j.calls; st_sh->jitStale[k] = j.stale; }
    return NULL;
}
static int t_sigstop(void) {
    st_sh = mmap(0, 4096, PROT_READ | PROT_WRITE, MAP_SHARED | MAP_ANONYMOUS, -1, 0);
    memset(st_sh, 0, sizeof *st_sh);
    pid_t c = fork();
    if (!c) {
        struct sigaction a;
        memset(&a, 0, sizeof a);
        a.sa_handler = st_usr1;
        a.sa_flags = SA_RESTART;
        sigemptyset(&a.sa_mask);
        sigaction(SIGUSR1, &a, NULL);
        sigset_t u;
        sigemptyset(&u);
        sigaddset(&u, SIGUSR1);
        pthread_sigmask(SIG_BLOCK, &u, NULL);
        pthread_t w[8];
        for (long k = 0; k < 8; k++) pthread_create(&w[k], 0, st_worker, (void *)k);
        for (int k = 0; k < 8; k++) pthread_join(w[k], 0);
        _exit(0);
    }
    uint64_t end = mono_ns(CLOCK_MONOTONIC) + 30000000000ULL;
    while (st_sh->ready < 8 && mono_ns(CLOCK_MONOTONIC) < end) cfz_sleep_us(1000);
    long cycles = 200 * scale, stopped = 0, cont = 0, sent = 0;
    for (long i = 0; i < cycles; i++) {
        siginfo_t si;
        cfz_sleep_us(200 + (long)(next() % 1300));
        kill(c, SIGSTOP);
        memset(&si, 0, sizeof si);
        if (waitid(P_PID, c, &si, WSTOPPED | WEXITED) || si.si_code != CLD_STOPPED) break;
        stopped++;
        cfz_sleep_us(100 + (long)(next() % 400));
        kill(c, SIGCONT);
        memset(&si, 0, sizeof si);
        if (waitid(P_PID, c, &si, WCONTINUED | WEXITED) || si.si_code != CLD_CONTINUED) break;
        cont++;
        if (i % 4 == 0) {
            int before = st_sh->usr1;
            kill(c, SIGUSR1);
            sent++;
            uint64_t until = mono_ns(CLOCK_MONOTONIC) + 5000000000ULL;
            while (st_sh->usr1 == before && mono_ns(CLOCK_MONOTONIC) < until) cfz_sleep_us(100);
        }
    }
    st_sh->stop = 1;
    int st = 0;
    waitpid(c, &st, 0);
    OUT("stopped", "%ld/%ld", stopped, cycles);
    OUT("continued", "%ld/%ld", cont, cycles);
    OUT("usr1", "%d/%ld", st_sh->usr1, sent);
    OUT("jit", "ran %d stale %ld", st_sh->jitCalls[0] > 0 && st_sh->jitCalls[1] > 0, st_sh->jitStale[0] + st_sh->jitStale[1]);
    OUT("compute", "ran %d bad %ld", st_sh->sums[0] > 0 && st_sh->sums[1] > 0, st_sh->sumBad[0] + st_sh->sumBad[1]);
    const char *names[4] = {"futexTimed", "nanosleep", "ppoll", "clockNanosleepAbs"};
    for (int x = 0; x < 4; x++)
        OUT(names[x], "ran %d early %ld eintr %ld wrong %ld", st_sh->timed[x] > 0, st_sh->early[x], st_sh->eintr[x], st_sh->wrong[x]);
    OUT("exit", "%d", WIFEXITED(st) ? WEXITSTATUS(st) : 128 + WTERMSIG(st));
    return 1;
}

static const struct { const char *name; int (*fn)(void); } TESTS[] = {
    {"casp", t_casp}, {"lse2", t_lse2}, {"ldapr", t_ldapr}, {"dczva", t_dczva}, {"carry", t_carry},
    {"fpcr", t_fpcr}, {"tbi", t_tbi}, {"rwrx", t_rwrx}, {"mremapcode", t_mremapcode},
    {"membarrier", t_membarrier}, {"dlopen", t_dlopen}, {"segvbarrier", t_segvbarrier},
    {"siglongjmp", t_siglongjmp}, {"ucontext", t_ucontext}, {"pthreadcancel", t_pthreadcancel},
    {"seccomp", t_seccomp}, {"autodisarm", t_autodisarm}, {"signalfd", t_signalfd}, {"pselect", t_pselect},
    {"sigsuspend", t_sigsuspend},
    {"cputimer", t_cputimer}, {"futexwaitv", t_futexwaitv}, {"epolloneshot", t_epolloneshot},
    {"pidfd", t_pidfd}, {"clone3", t_clone3}, {"memfdexec", t_memfdexec}, {"vmops", t_vmops},
    {"timerfd", t_timerfd}, {"rseq", t_rseq}, {"iouring", t_iouring}, {"ptrace", t_ptrace},
    {"wipeonfork", t_wipeonfork}, {"scmrights", t_scmrights}, {"ifunc", t_ifunc},
    {"robust", t_robust}, {"robustpi", t_robustpi}, {"mapnoreplace", t_mapnoreplace}, {"vmreadv", t_vmreadv}, {"pidns", t_pidns},
    {"getrandom", t_getrandom}, {"getrandomhash", t_getrandomhash}, {"vmsplice", t_vmsplice},
    {"unwind", t_unwind}, {"tlsdtor", t_tlsdtor}, {"staticpie", t_staticpie}, {"staticpieaddr", t_staticpieaddr},
    {"counters", t_counters}, {"vdsoclock", t_vdsoclock}, {"vdsogetrandom", t_vdsogetrandom}, {"rndr", t_rndr},
    {"tpidrro", t_tpidrro}, {"waits", t_waits}, {"prtsc", t_prtsc}, {"affinity", t_affinity},
    {"exclusive", t_exclusive},
    {"sigframe", t_sigframe}, {"ctxrewrite", t_ctxrewrite}, {"restart", t_restart}, {"rtfifo", t_rtfifo},
    {"sigpressure", t_sigpressure}, {"tmpmask", t_tmpmask}, {"sigshare", t_sigshare}, {"sigstop", t_sigstop},
    {"capabilities", t_capabilities},
};

int main(int argc, char **argv) {
    if (argc > 1 && !strcmp(argv[1], "child")) return 42;
    argv0 = argv[0];
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
