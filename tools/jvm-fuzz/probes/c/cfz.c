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

/* What this host offers: syscalls, files and CPU features. Differences from a
 * native run describe the guest; they are reported, not failures. */
static void cap(const char *name, long r) { OUT(name, "%s", r < 0 ? strerror(errno) : "ok"); }
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
    cap("unshare_user", unshare(CLONE_NEWUSER));
    const char *files[] = {"/proc/self/maps", "/proc/self/pagemap", "/proc/self/smaps", "/proc/cpuinfo",
                           "/proc/self/auxv", "/sys/devices/system/cpu/online", "/dev/random", "/dev/urandom",
                           "/dev/shm", "/proc/sys/kernel/random/boot_id", "/sys/kernel/mm/transparent_hugepage/enabled"};
    for (size_t i = 0; i < sizeof files / sizeof files[0]; i++) OUT("file", "%s %d", files[i], access(files[i], F_OK) == 0);
    OUT("cpus", "%ld", sysconf(_SC_NPROCESSORS_ONLN));
    OUT("rseqRegistered", "%d", __rseq_size > 0);
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
    {"capabilities", t_capabilities},
};

int main(int argc, char **argv) {
    if (argc > 1 && !strcmp(argv[1], "child")) return 42;
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
