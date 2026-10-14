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
#include <linux/audit.h>
#include <linux/bpf.h>
#include <linux/perf_event.h>
#include <sys/personality.h>
#include <sys/random.h>
#include <limits.h>
#include <sys/file.h>
#include <sys/ioctl.h>
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

/* ---- signals: nesting, exec, alternate-stack switches, SIGIO and timers ---- */

/* 1 when thread tid of process pid sleeps in syscall nr (with first argument a0
 * unless a0 is 0), as /proc reports it; 0 if not seen within ms milliseconds, -1
 * without the file. Callers use it only to place an event after a wait has begun
 * and sleep instead when it fails; no output depends on it. */
static int cfz_blocked_in(pid_t pid, pid_t tid, long nr, uintptr_t a0, long ms) {
    char path[64];
    snprintf(path, sizeof path, "/proc/%d/task/%d/syscall", (int)pid, (int)tid);
    uint64_t end = mono_ns(CLOCK_MONOTONIC) + (uint64_t)ms * 1000000ULL;
    for (;;) {
        int fd = open(path, O_RDONLY | O_CLOEXEC);
        if (fd < 0) return -1;
        char b[256];
        ssize_t n = read(fd, b, sizeof b - 1);
        close(fd);
        if (n > 0) {
            b[n] = 0;
            long got = -1;
            unsigned long arg = 0;
            if (sscanf(b, "%ld %lx", &got, &arg) >= 1 && got == nr && (!a0 || arg == a0)) return 1;
        }
        if (mono_ns(CLOCK_MONOTONIC) > end) return 0;
        cfz_sleep_us(100);
    }
}
static void cfz_settle(pid_t pid, pid_t tid, long nr, uintptr_t a0) {
    if (cfz_blocked_in(pid, tid, nr, a0, 2000) != 1) cfz_sleep_us(20000);
}

/* det: same-signal nesting with SA_NODEFER to depth 50; on odd rounds the deepest
 * level raises another signal whose SA_ONSTACK handler nests ten levels on the
 * alternate stack and raises a legacy (non-SA_SIGINFO) handler there; sigaltstack
 * there must report SS_ONSTACK and refuse a change; SA_RESETHAND proven in a child
 * by the default action. Every level raises the next through
 * cfz_canary_kill, which checks x9-x15, x19-x28, v16-v23 and NZCV across the
 * delivery, and each frame must resume right after its svc. */
CFZ_HIDDEN long cfz_canary_kill(const uint64_t *can, long tgid, long tid, long sig);
CFZ_HIDDEN extern const char cfz_ck_after[];
#define CK_XREGS "9, 10, 11, 12, 13, 14, 15, 19, 20, 21, 22, 23, 24, 25, 26, 27, 28"
asm(".text\n.p2align 2\n"
    CFZ_FN("cfz_canary_kill")
    "stp x29, x30, [sp, #-176]!\nmov x29, sp\n"
    "stp x19, x20, [sp, #16]\nstp x21, x22, [sp, #32]\nstp x23, x24, [sp, #48]\nstp x25, x26, [sp, #64]\n"
    "stp x27, x28, [sp, #80]\nstp d8, d9, [sp, #96]\nstp d10, d11, [sp, #112]\nstp d12, d13, [sp, #128]\n"
    "stp d14, d15, [sp, #144]\nstr x0, [sp, #160]\n"
    ".irp r, " CK_XREGS "\nldr x\\r, [x0, #\\r * 8]\n.endr\n"
    ".irp v, 16, 17, 18, 19, 20, 21, 22, 23\nldr q\\v, [x0, #(256 + (\\v - 16) * 16)]\n.endr\n"
    "ldr x5, [x0, #504]\nmov x0, x1\nmov x1, x2\nmov x2, x3\nmov x8, #131\nmsr nzcv, x5\nsvc #0\n"
    CFZ_AT("cfz_ck_after")
    "mrs x6, nzcv\nldr x0, [sp, #160]\nmov x1, #0\n"
    ".irp r, " CK_XREGS "\nldr x2, [x0, #\\r * 8]\ncmp x\\r, x2\ncinc x1, x1, ne\n.endr\n"
    ".irp v, 16, 17, 18, 19, 20, 21, 22, 23\nldr x2, [x0, #(256 + (\\v - 16) * 16)]\nfmov x3, d\\v\ncmp x2, x3\n"
    "cinc x1, x1, ne\nldr x2, [x0, #(264 + (\\v - 16) * 16)]\nmov x3, v\\v\\().d[1]\ncmp x2, x3\ncinc x1, x1, ne\n.endr\n"
    "ldr x2, [x0, #504]\ncmp x2, x6\ncinc x1, x1, ne\nmov x0, x1\n"
    "ldp x19, x20, [sp, #16]\nldp x21, x22, [sp, #32]\nldp x23, x24, [sp, #48]\nldp x25, x26, [sp, #64]\n"
    "ldp x27, x28, [sp, #80]\nldp d8, d9, [sp, #96]\nldp d10, d11, [sp, #112]\nldp d12, d13, [sp, #128]\n"
    "ldp d14, d15, [sp, #144]\nldp x29, x30, [sp], #176\nret\n");
#define NS_DEPTH 50
#define NS_ALT 10
static pid_t ns_tid;
static uint64_t ns_mask0;
static stack_t ns_alt;
static volatile int ns_depth, ns_adepth, ns_phase;
static long ns_canbad, ns_pcbad, ns_spbad, ns_maskbad, ns_infobad, ns_altbad, ns_altflagbad, ns_altrefused, ns_legacy, ns_legacybad,
    ns_levels, ns_alevels;
static uintptr_t ns_sp[NS_DEPTH + 2], ns_asp[NS_ALT + 2];
static void ns_raise(int level, int sig) {
    uint64_t can[64];
    for (int i = 0; i < 64; i++) can[i] = ((uint64_t)(level + 1) * 0x9E3779B97F4A7C15ULL) ^ ((uint64_t)i << 48) ^ (uint64_t)sig;
    can[63] = (uint64_t)((level * 5 + sig) & 15) << 28;
    long bad = cfz_canary_kill(can, getpid(), ns_tid, sig);
    ns_canbad += bad;
}
static int ns_onalt(uintptr_t sp) { return sp >= (uintptr_t)ns_alt.ss_sp && sp < (uintptr_t)ns_alt.ss_sp + ns_alt.ss_size; }
static void ns_frame(int s, const siginfo_t *si, const ucontext_t *uc) {
    if (uc->uc_mcontext.pc != (uintptr_t)cfz_ck_after) ns_pcbad++;
    if (si->si_signo != s || si->si_code != SI_TKILL || si->si_pid != getpid()) ns_infobad++;
}
static void ns_usr1(int s, siginfo_t *si, void *ucv) {
    ucontext_t *uc = ucv;
    volatile char here = 0;
    int d = ++ns_depth;
    ns_levels++;
    ns_sp[d] = (uintptr_t)&here;
    if ((d > 1 && ns_sp[d] >= ns_sp[d - 1]) || ns_onalt(ns_sp[d])) ns_spbad++;
    ns_frame(s, si, uc);
    uint64_t saved, hm = ns_mask0 | SIGBIT(SIGRTMIN + 11);
    memcpy(&saved, &uc->uc_sigmask, 8);
    if (kmask_get() != hm || saved != (d == 1 ? ns_mask0 : hm)) ns_maskbad++;
    if (d < NS_DEPTH) ns_raise(d, SIGUSR1);
    else if (ns_phase) ns_raise(d, SIGRTMIN + 2);
    ns_depth--;
}
static void ns_alth(int s, siginfo_t *si, void *ucv) {
    ucontext_t *uc = ucv;
    volatile char here = 0;
    int d = ++ns_adepth;
    ns_alevels++;
    ns_asp[d] = (uintptr_t)&here;
    stack_t cur;
    sigaltstack(NULL, &cur);
    if (!ns_onalt(ns_asp[d])) ns_altbad++;
    if (cur.ss_flags != SS_ONSTACK) ns_altflagbad++;
    if (d == 1) { /* the kernel refuses to change the stack it is running on */
        int saved = errno;
        stack_t again = ns_alt;
        ns_altrefused += sigaltstack(&again, NULL) == -1 && errno == EPERM;
        errno = saved;
    }
    if (d > 1 && ns_asp[d] >= ns_asp[d - 1]) ns_altbad++;
    /* The saved uc_stack is the registration (its flags, not SS_ONSTACK), at every level. */
    if (uc->uc_stack.ss_sp != ns_alt.ss_sp || uc->uc_stack.ss_size != ns_alt.ss_size || uc->uc_stack.ss_flags != 0) ns_altbad++;
    ns_frame(s, si, uc);
    if (kmask_get() != (ns_mask0 | SIGBIT(SIGRTMIN + 11))) ns_maskbad++;
    if (d < NS_ALT) ns_raise(100 + d, SIGRTMIN + 2);
    else ns_raise(200, SIGUSR2);
    ns_adepth--;
}
static void ns_leg(int s) {
    volatile char here = 0;
    uintptr_t sp = (uintptr_t)&here;
    ns_legacy++;
    if (s != SIGUSR2 || !ns_onalt(sp) || sp >= ns_asp[NS_ALT] ||
        kmask_get() != (ns_mask0 | SIGBIT(SIGRTMIN + 11) | SIGBIT(SIGUSR2)))
        ns_legacybad++;
}
static volatile int nsr_ran, nsr_dfl, nsr_blocked;
static void nsr_h(int s, siginfo_t *si, void *uc) {
    (void)si; (void)uc;
    struct sigaction o;
    sigaction(s, NULL, &o);
    nsr_ran++;
    nsr_dfl = o.sa_handler == SIG_DFL;
    nsr_blocked = (kmask_get() & SIGBIT(s)) != 0;
}
static int t_nestsig(void) {
    static char altmem[262144] __attribute__((aligned(16)));
    ns_alt.ss_sp = altmem;
    ns_alt.ss_size = sizeof altmem;
    ns_alt.ss_flags = 0;
    if (sigaltstack(&ns_alt, NULL)) return 0;
    ns_tid = gettid();
    struct sigaction a;
    memset(&a, 0, sizeof a);
    a.sa_sigaction = ns_usr1;
    a.sa_flags = SA_SIGINFO | SA_NODEFER;
    sigemptyset(&a.sa_mask);
    sigaddset(&a.sa_mask, SIGRTMIN + 11);
    sigaction(SIGUSR1, &a, NULL);
    a.sa_sigaction = ns_alth;
    a.sa_flags = SA_SIGINFO | SA_NODEFER | SA_ONSTACK;
    sigemptyset(&a.sa_mask);
    sigaction(SIGRTMIN + 2, &a, NULL);
    memset(&a, 0, sizeof a);
    a.sa_handler = ns_leg;
    sigemptyset(&a.sa_mask);
    sigaction(SIGUSR2, &a, NULL);
    ns_mask0 = kmask_get();
    long rounds = 500 * scale, endbad = 0;
    for (long r = 0; r < rounds; r++) {
        ns_phase = (int)(r & 1);
        ns_raise(0, SIGUSR1);
        endbad += kmask_get() != ns_mask0 || ns_depth || ns_adepth;
    }
    signal(SIGUSR1, SIG_DFL);
    signal(SIGUSR2, SIG_DFL);
    signal(SIGRTMIN + 2, SIG_DFL);
    stack_t off = {.ss_flags = SS_DISABLE};
    sigaltstack(&off, NULL);
    long alts = rounds / 2;
    OUT("levels", "%ld/%ld", ns_levels, rounds * NS_DEPTH);
    OUT("altLevels", "%ld/%ld", ns_alevels, alts * NS_ALT);
    OUT("legacy", "%ld/%ld bad %ld", ns_legacy, alts, ns_legacybad);
    OUT("canaryBad", "%ld", ns_canbad);
    OUT("frame", "pcBad %ld infoBad %ld spBad %ld maskBad %ld altBad %ld endBad %ld", ns_pcbad, ns_infobad, ns_spbad,
        ns_maskbad, ns_altbad, endbad);
    OUT("altQuery", "onstackFlagBad %ld changeRefused %ld/%ld", ns_altflagbad, ns_altrefused, alts);
    int p[2];
    if (pipe(p)) return 0;
    pid_t c = fork();
    if (!c) {
        memset(&a, 0, sizeof a);
        a.sa_sigaction = nsr_h;
        a.sa_flags = SA_SIGINFO | SA_RESETHAND;
        sigemptyset(&a.sa_mask);
        sigaction(SIGUSR1, &a, NULL);
        syscall(SYS_tgkill, getpid(), gettid(), SIGUSR1);
        struct sigaction o;
        sigaction(SIGUSR1, NULL, &o);
        char b = (char)('0' + (nsr_ran == 1) + 2 * nsr_dfl + 4 * nsr_blocked + 8 * (o.sa_handler == SIG_DFL));
        if (write(p[1], &b, 1) != 1) _exit(4);
        syscall(SYS_tgkill, getpid(), gettid(), SIGUSR1);
        _exit(7);
    }
    close(p[1]);
    char b = '?';
    if (read(p[0], &b, 1) != 1) b = '?';
    close(p[0]);
    int st = 0;
    waitpid(c, &st, 0);
    int v = b == '?' ? -1 : b - '0';
    OUT("resethand", "ran %d dflInHandler %d blockedInHandler %d dflAfter %d exit %d", v & 1, (v >> 1) & 1, (v >> 2) & 1,
        (v >> 3) & 1, WIFEXITED(st) ? WEXITSTATUS(st) : 128 + WTERMSIG(st));
    return 1;
}

/* det (execsig): signal state across execve. Before exec: SIGUSR1 and three realtime
 * signals blocked, SIGUSR2, SIGHUP and SIGWINCH handled, SIGPIPE and SIGINT ignored,
 * an alternate stack, a sibling thread; then a process-directed SIGUSR1, two values
 * queued to the exec'ing thread and one to the sibling, which blocks it. A failed
 * execve must change nothing. After the real one the helper reports its state,
 * drains what is pending, takes anything left through handlers and acknowledges;
 * only then does the parent queue one more value.
 * replay (execsigrace): the parent queues 64 values and sends SIGWINCH while the
 * child is inside execve; the helper reports what arrived. */
#define ES_A (SIGRTMIN + 4)
#define ES_B (SIGRTMIN + 5)
#define ES_C (SIGRTMIN + 6)
static char es_self[4096];
static volatile int es_sib_go, es_late;
static pid_t es_sib_tid;
static void es_nop(int s) { (void)s; }
static void es_late_h(int s) { (void)s; es_late++; }
static void es_state(const char *tag) {
    uint64_t m = kmask_get(), p = kpending();
    const int sigs[6] = {SIGUSR2, SIGHUP, SIGWINCH, SIGPIPE, SIGINT, SIGTERM};
    char d[7];
    for (int i = 0; i < 6; i++) {
        struct sigaction a;
        sigaction(sigs[i], NULL, &a);
        d[i] = a.sa_handler == SIG_DFL ? 'D' : a.sa_handler == SIG_IGN ? 'I' : 'H';
    }
    d[6] = 0;
    stack_t st;
    sigaltstack(NULL, &st);
    OUT(tag, "mask %#llx pending %#llx disp %s alt %s", (unsigned long long)m, (unsigned long long)p, d,
        (st.ss_flags & SS_DISABLE) ? "disabled" : (st.ss_flags & SS_ONSTACK) ? "onstack" : "armed");
}
static const char *es_code(int c) {
    return c == SI_QUEUE ? "queue" : c == SI_USER ? "user" : c == SI_TKILL ? "tkill" : "other";
}
static int es_helper(int argc, char **argv) {
    int fd = argc > 2 ? atoi(argv[2]) : -1, race = argc > 3 && atoi(argv[3]), gate = argc > 4 ? atoi(argv[4]) : -1;
    es_state("helper");
    char b;
    if (race && gate >= 0 && read(gate, &b, 1) != 1) return 3;
    sigset_t w;
    sigemptyset(&w);
    sigaddset(&w, SIGUSR1);
    sigaddset(&w, ES_A);
    sigaddset(&w, ES_C);
    char line[2048] = "";
    int len = 0, n = 0, queued = 0, inorder = 1, last = -1, usr1 = 0;
    struct timespec zero = {0, 0};
    siginfo_t si;
    int got;
    while (n < 256 && (got = sigtimedwait(&w, &si, &zero)) > 0) {
        n++;
        if (got == SIGUSR1) usr1++;
        if (race) {
            if (got == ES_A) { queued++; inorder &= si.si_value.sival_int > last; last = si.si_value.sival_int; }
        } else if (len < (int)sizeof line - 64) {
            len += snprintf(line + len, sizeof line - (size_t)len, " %s/%d/%s",
                            got == SIGUSR1 ? "USR1" : got == ES_A ? "A" : got == ES_C ? "C" : "?",
                            got == SIGUSR1 ? 0 : si.si_value.sival_int, es_code(si.si_code));
        }
    }
    struct sigaction a;
    memset(&a, 0, sizeof a);
    a.sa_handler = es_late_h;
    sigemptyset(&a.sa_mask);
    sigaction(SIGUSR1, &a, NULL);
    sigaction(ES_A, &a, NULL);
    sigaction(ES_C, &a, NULL);
    sigprocmask(SIG_UNBLOCK, &w, NULL);
    if (race) OUT("raced", "queued %d/64 inOrder %d usr1 %d", queued, inorder, usr1);
    else OUT("drained", "%d%s", n, line);
    OUT("lateDeliveries", "%d", (int)es_late);
    if (write(fd, "A", 1) != 1) return 4;
    sigset_t bw;
    sigemptyset(&bw);
    sigaddset(&bw, ES_B);
    struct timespec t = {10, 0};
    memset(&si, 0, sizeof si);
    got = sigtimedwait(&bw, &si, &t);
    OUT("afterAck", "%s value %d fromParent %d %s", got == ES_B ? "B" : "none", si.si_value.sival_int,
        si.si_pid == getppid(), es_code(si.si_code));
    return 0;
}
static void *es_sibling(void *p) {
    (void)p;
    es_sib_tid = gettid();
    __atomic_store_n(&es_sib_go, 1, __ATOMIC_RELEASE);
    cfz_wake(&es_sib_go);
    for (;;) pause();
    return NULL;
}
static void es_child(int race, int ackfd, int gofd, int gatefd) {
    sigset_t b;
    sigemptyset(&b);
    sigaddset(&b, SIGUSR1);
    sigaddset(&b, ES_A);
    sigaddset(&b, ES_B);
    sigaddset(&b, ES_C);
    pthread_sigmask(SIG_BLOCK, &b, NULL);
    struct sigaction a;
    memset(&a, 0, sizeof a);
    a.sa_handler = es_nop;
    sigemptyset(&a.sa_mask);
    sigaction(SIGUSR2, &a, NULL);
    sigaction(SIGWINCH, &a, NULL);
    a.sa_flags = SA_ONSTACK;
    sigaction(SIGHUP, &a, NULL);
    signal(SIGPIPE, SIG_IGN);
    signal(SIGINT, SIG_IGN);
    static char alt[65536] __attribute__((aligned(16)));
    stack_t st = {.ss_sp = alt, .ss_flags = 0, .ss_size = sizeof alt};
    sigaltstack(&st, NULL);
    pthread_t t;
    pthread_create(&t, NULL, es_sibling, NULL);
    cfz_wait_for(&es_sib_go, 1, 5000);
    if (!race) {
        kill(getpid(), SIGUSR1);
        cfz_queue(gettid(), ES_A, 11);
        cfz_queue(gettid(), ES_A, 12);
        cfz_queue(es_sib_tid, ES_C, 22);
    }
    es_state("before");
    char *bad[] = {"cfz", NULL};
    int r = execve("/nonexistent/cfz-helper", bad, environ);
    OUT("failedExec", "%d %s", r, errno == ENOENT ? "ENOENT" : "other");
    es_state("afterFailedExec");
    char fds[16], gates[16];
    snprintf(fds, sizeof fds, "%d", ackfd);
    snprintf(gates, sizeof gates, "%d", gatefd);
    char *args[] = {"cfz", "execsighelper", fds, race ? "1" : "0", gates, NULL};
    if (race && write(gofd, "E", 1) != 1) _exit(5);
    execve(es_self, args, environ);
    _exit(127);
}
static int execsig(int race) {
    ssize_t n = readlink("/proc/self/exe", es_self, sizeof es_self - 1);
    if (n < 0) return 0;
    es_self[n] = 0;
    int ack[2], go[2], gate[2];
    if (pipe(ack) || pipe(go) || pipe(gate)) return 0;
    pid_t c = fork();
    if (!c) {
        close(ack[0]);
        close(go[0]);
        close(gate[1]);
        es_child(race, ack[1], go[1], gate[0]);
    }
    close(ack[1]);
    close(go[1]);
    close(gate[0]);
    char b;
    if (race && read(go[0], &b, 1) == 1) {
        for (int i = 0; i < 64; i++) {
            union sigval v = {.sival_int = 100 + i};
            sigqueue(c, ES_A, v);
            if (i % 8 == 0) kill(c, SIGWINCH);
        }
        if (write(gate[1], "D", 1) != 1) return 0;
    }
    int acked = read(ack[0], &b, 1) == 1;
    if (acked) {
        cfz_settle(c, c, SYS_rt_sigtimedwait, 0);
        union sigval v = {.sival_int = 77};
        sigqueue(c, ES_B, v);
    }
    int st = 0;
    waitpid(c, &st, 0);
    close(ack[0]);
    close(go[0]);
    close(gate[1]);
    OUT("acked", "%d", acked);
    OUT("exit", "%d", WIFEXITED(st) ? WEXITSTATUS(st) : 128 + WTERMSIG(st));
    return 1;
}
static int t_execsig(void) { return execsig(0); }
static int t_execsigrace(void) { return execsig(1); }

/* det: an SS_AUTODISARM SIGUSR1 handler swaps to the main context and keeps its
 * frame on alternate stack A. SIGUSR2 (SA_ONSTACK) then arrives on the thread
 * stack, or on alternate stack B when odd rounds register B meanwhile; the first
 * handler resumes, checks its canaries and saved context, and returns, which must
 * restore registration A. */
#define AC_AUTODISARM (1U << 31)
static ucontext_t ac_main, ac_h1, ac_tmp;
static char ac_mem_a[65536] __attribute__((aligned(16))), ac_mem_b[65536] __attribute__((aligned(16)));
static long ac_round;
static int ac_useb;
static long ac_h1n, ac_h1alt, ac_h1disarmed, ac_ucok, ac_resumed, ac_canbad, ac_ucok2, ac_u2n, ac_u2where[3], ac_u2flags,
    ac_finalok;
static int ac_in(uintptr_t p, const char *m) { return p >= (uintptr_t)m && p < (uintptr_t)m + 65536; }
static int ac_ucgood(const ucontext_t *uc) {
    return uc->uc_stack.ss_sp == ac_mem_a && uc->uc_stack.ss_size == sizeof ac_mem_a &&
           (unsigned)uc->uc_stack.ss_flags == AC_AUTODISARM && uc->uc_mcontext.pc == (uintptr_t)cfz_svc_after;
}
static void ac_usr1(int s, siginfo_t *si, void *ucv) {
    (void)s; (void)si;
    ucontext_t *uc = ucv;
    volatile uint64_t can[16];
    for (int i = 0; i < 16; i++) can[i] = 0xA5A5000000000000ULL ^ ((uint64_t)ac_round << 8) ^ (uint64_t)i;
    stack_t cur;
    sigaltstack(NULL, &cur);
    ac_h1n++;
    ac_h1alt += ac_in((uintptr_t)can, ac_mem_a);
    ac_h1disarmed += (cur.ss_flags & SS_DISABLE) != 0;
    ac_ucok += ac_ucgood(uc);
    swapcontext(&ac_h1, &ac_main);
    ac_resumed++;
    long bad = 0;
    for (int i = 0; i < 16; i++) bad += can[i] != (0xA5A5000000000000ULL ^ ((uint64_t)ac_round << 8) ^ (uint64_t)i);
    ac_canbad += bad;
    ac_ucok2 += ac_ucgood(uc);
}
static void ac_usr2(int s) {
    (void)s;
    volatile char here = 0;
    uintptr_t p = (uintptr_t)&here;
    stack_t cur;
    sigaltstack(NULL, &cur);
    ac_u2n++;
    ac_u2where[ac_in(p, ac_mem_a) ? 1 : ac_in(p, ac_mem_b) ? 2 : 0]++;
    ac_u2flags += cur.ss_flags == (ac_useb ? SS_ONSTACK : SS_DISABLE);
}
static void __attribute__((noinline)) ac_one(void) {
    volatile int step = 0;
    getcontext(&ac_main);
    if (step == 0) {
        step = 1;
        cfz_rawsys(getpid(), gettid(), SIGUSR1, 0, 0, 0, SYS_tgkill);
        /* The first handler returned; its sigreturn resumed here. */
        step = 3;
        return;
    }
    if (step == 1) {
        step = 2;
        if (ac_useb) {
            stack_t b = {.ss_sp = ac_mem_b, .ss_flags = 0, .ss_size = sizeof ac_mem_b};
            sigaltstack(&b, NULL);
        }
        cfz_rawsys(getpid(), gettid(), SIGUSR2, 0, 0, 0, SYS_tgkill);
        swapcontext(&ac_tmp, &ac_h1);
    }
    abort();
}
static int t_autodisarmctx(void) {
    stack_t a = {.ss_sp = ac_mem_a, .ss_flags = (int)AC_AUTODISARM, .ss_size = sizeof ac_mem_a};
    if (sigaltstack(&a, NULL)) { puts("UNSUPPORTED autodisarm"); return 0; }
    struct sigaction s;
    memset(&s, 0, sizeof s);
    s.sa_sigaction = ac_usr1;
    s.sa_flags = SA_SIGINFO | SA_ONSTACK;
    sigemptyset(&s.sa_mask);
    sigaction(SIGUSR1, &s, NULL);
    memset(&s, 0, sizeof s);
    s.sa_handler = ac_usr2;
    s.sa_flags = SA_ONSTACK;
    sigemptyset(&s.sa_mask);
    sigaction(SIGUSR2, &s, NULL);
    long rounds = 2000 * scale;
    for (ac_round = 0; ac_round < rounds; ac_round++) {
        ac_useb = (int)(ac_round & 1);
        ac_one();
        stack_t cur;
        sigaltstack(NULL, &cur);
        ac_finalok += cur.ss_sp == ac_mem_a && cur.ss_size == sizeof ac_mem_a && (unsigned)cur.ss_flags == AC_AUTODISARM;
    }
    signal(SIGUSR1, SIG_DFL);
    signal(SIGUSR2, SIG_DFL);
    stack_t off = {.ss_flags = SS_DISABLE};
    sigaltstack(&off, NULL);
    OUT("first", "%ld/%ld onA %ld disarmed %ld savedOk %ld resumed %ld savedOkAfter %ld canaryBad %ld", ac_h1n, rounds,
        ac_h1alt, ac_h1disarmed, ac_ucok, ac_resumed, ac_ucok2, ac_canbad);
    OUT("second", "%ld/%ld threadStack %ld onA %ld onB %ld flagsOk %ld", ac_u2n, rounds, ac_u2where[0], ac_u2where[1],
        ac_u2where[2], ac_u2flags);
    OUT("restoredA", "%ld/%ld", ac_finalok, rounds);
    return 1;
}

/* det: F_SETOWN_EX(F_OWNER_TID) with F_SETSIG to a realtime signal and O_ASYNC on a
 * pipe and a socket. Each phase makes one readiness transition while the workers
 * wait in sigtimedwait; ownership moves between threads and to the process, and
 * descriptor numbers are reused. Reports who received it, si_fd, si_band, si_code. */
#define SIO_SIG (SIGRTMIN + 7)
#define SIO_STOP (SIGRTMIN + 8)
struct sio_w { pthread_t th; pid_t tid; volatile int gen, done; int got, err, fd, code; long band; };
static struct sio_w sio_w[3];
static volatile int sio_quit;
static void *sio_worker(void *p) {
    struct sio_w *w = p;
    w->tid = gettid();
    int seen = 0;
    sigset_t s;
    sigemptyset(&s);
    sigaddset(&s, SIO_SIG);
    sigaddset(&s, SIO_STOP);
    __atomic_store_n(&w->done, 1, __ATOMIC_RELEASE);
    cfz_wake(&w->done);
    for (;;) {
        int g;
        while ((g = __atomic_load_n(&w->gen, __ATOMIC_ACQUIRE)) == seen) cfz_wait(&w->gen, seen, 100);
        seen = g;
        if (sio_quit) break;
        siginfo_t si;
        memset(&si, 0, sizeof si);
        struct timespec t = {10, 0};
        int r = sigtimedwait(&s, &si, &t);
        w->got = r;
        w->err = r < 0 ? errno : 0;
        w->fd = si.si_fd;
        w->band = si.si_band;
        w->code = si.si_code;
        __atomic_add_fetch(&w->done, 1, __ATOMIC_RELEASE);
        cfz_wake(&w->done);
    }
    return NULL;
}
static int sio_own(int fd, int type, pid_t who, int async) {
    struct f_owner_ex o = {type, who}, g = {0, 0};
    int ok = !fcntl(fd, F_SETOWN_EX, &o) && !fcntl(fd, F_SETSIG, SIO_SIG);
    int fl = fcntl(fd, F_GETFL);
    ok &= !fcntl(fd, F_SETFL, (async ? fl | O_ASYNC : fl & ~O_ASYNC) | O_NONBLOCK);
    ok &= !fcntl(fd, F_GETOWN_EX, &g) && g.type == type && g.pid == who;
    return ok;
}
/* Releases the workers in mask into sigtimedwait, writes one byte to wfd once they
 * are blocked and describes who received SIO_SIG. */
static void sio_phase(char *b, size_t n, int mask, int wfd, int rfd, int want) {
    int before[3];
    for (int k = 0; k < 3; k++) {
        if (!(mask >> k & 1)) continue;
        before[k] = __atomic_load_n(&sio_w[k].done, __ATOMIC_ACQUIRE);
        sio_w[k].got = 0;
        __atomic_add_fetch(&sio_w[k].gen, 1, __ATOMIC_RELEASE);
        cfz_wake(&sio_w[k].gen);
    }
    for (int k = 0; k < 3; k++)
        if (mask >> k & 1) cfz_settle(getpid(), sio_w[k].tid, SYS_rt_sigtimedwait, 0);
    if (write(wfd, "x", 1) != 1) { snprintf(b, n, "writeFailed"); return; }
    uint64_t end = mono_ns(CLOCK_MONOTONIC) + (want < 0 ? 20000000ULL : 5000000000ULL);
    for (;;) {
        int any = 0;
        for (int k = 0; k < 3; k++)
            if ((mask >> k & 1) && __atomic_load_n(&sio_w[k].done, __ATOMIC_ACQUIRE) != before[k]) any = 1;
        if (any || mono_ns(CLOCK_MONOTONIC) > end) break;
        cfz_sleep_us(200);
    }
    for (int k = 0; k < 3; k++)
        if ((mask >> k & 1) && __atomic_load_n(&sio_w[k].done, __ATOMIC_ACQUIRE) == before[k])
            syscall(SYS_tgkill, getpid(), sio_w[k].tid, SIO_STOP);
    for (int k = 0; k < 3; k++)
        if (mask >> k & 1) cfz_wait_for(&sio_w[k].done, before[k] + 1, 10000);
    char c;
    while (read(rfd, &c, 1) == 1) {}
    int len = 0, who = 0;
    for (int k = 0; k < 3; k++) {
        if (!(mask >> k & 1) || sio_w[k].got != SIO_SIG) continue;
        who |= 1 << k;
        len += snprintf(b + len, n - (size_t)len, " w%d fdOk %d band %#lx code %d", k, sio_w[k].fd == rfd,
                        (unsigned long)sio_w[k].band, sio_w[k].code);
    }
    if (!who) snprintf(b, n, " none");
}
static int t_sigio(void) {
    sigset_t s;
    sigemptyset(&s);
    sigaddset(&s, SIO_SIG);
    sigaddset(&s, SIO_STOP);
    pthread_sigmask(SIG_BLOCK, &s, NULL);
    sio_quit = 0;
    for (int k = 0; k < 3; k++) {
        memset(&sio_w[k], 0, sizeof sio_w[k]);
        pthread_create(&sio_w[k].th, NULL, sio_worker, &sio_w[k]);
        cfz_wait_for(&sio_w[k].done, 1, 5000);
    }
    long rounds = 25 * scale;
    const char *names[6] = {"pipeW0", "sockW1", "pipeMovedW2", "pipeProcess", "pipeReusedW1", "sockReusedNoAsync"};
    char first[6][256], cur[256];
    long same[6] = {0}, setupok = 0, setups = 0, reused = 0;
    int pp[2], sv[2];
    if (pipe(pp) || socketpair(AF_UNIX, SOCK_STREAM, 0, sv)) return 0;
    fcntl(sv[0], F_SETFL, O_NONBLOCK);
    for (long r = 0; r < rounds; r++) {
        for (int ph = 0; ph < 6; ph++) {
            int ok = 1;
            switch (ph) {
            case 0: ok = sio_own(pp[0], F_OWNER_TID, sio_w[0].tid, 1); sio_phase(cur, sizeof cur, 7, pp[1], pp[0], 0); break;
            case 1: ok = sio_own(sv[0], F_OWNER_TID, sio_w[1].tid, 1); sio_phase(cur, sizeof cur, 7, sv[1], sv[0], 1); break;
            case 2: ok = sio_own(pp[0], F_OWNER_TID, sio_w[2].tid, 1); sio_phase(cur, sizeof cur, 7, pp[1], pp[0], 2); break;
            case 3: ok = sio_own(pp[0], F_OWNER_PID, getpid(), 1); sio_phase(cur, sizeof cur, 4, pp[1], pp[0], 2); break;
            case 4: {
                int op0 = pp[0], op1 = pp[1], os0 = sv[0], os1 = sv[1];
                close(pp[0]); close(pp[1]); close(sv[0]); close(sv[1]);
                if (pipe(pp) || socketpair(AF_UNIX, SOCK_STREAM, 0, sv)) return 0;
                reused += pp[0] == op0 && pp[1] == op1 && sv[0] == os0 && sv[1] == os1;
                fcntl(sv[0], F_SETFL, O_NONBLOCK);
                ok = sio_own(pp[0], F_OWNER_TID, sio_w[1].tid, 1);
                sio_phase(cur, sizeof cur, 7, pp[1], pp[0], 1);
                break;
            }
            default: ok = sio_own(sv[0], F_OWNER_TID, sio_w[0].tid, 0); sio_phase(cur, sizeof cur, 1, sv[1], sv[0], -1); break;
            }
            setups++;
            setupok += ok;
            if (!r) snprintf(first[ph], sizeof first[ph], "%s", cur);
            same[ph] += !strcmp(first[ph], cur);
        }
    }
    sio_quit = 1;
    for (int k = 0; k < 3; k++) {
        __atomic_add_fetch(&sio_w[k].gen, 1, __ATOMIC_RELEASE);
        cfz_wake(&sio_w[k].gen);
        pthread_join(sio_w[k].th, NULL);
    }
    close(pp[0]); close(pp[1]); close(sv[0]); close(sv[1]);
    pthread_sigmask(SIG_UNBLOCK, &s, NULL);
    for (int ph = 0; ph < 6; ph++) OUT(names[ph], "%ld/%ld%s", same[ph], rounds, first[ph]);
    OUT("owners", "%ld/%ld", setupok, setups);
    OUT("numbersReused", "%ld/%ld", reused, rounds);
    return 1;
}

/* det (timerstorm): 200 one-shot POSIX timers per round: 100 SIGEV_SIGNAL with values
 * (never handled by the two threads that block it), 50 SIGEV_THREAD,
 * 25 SIGEV_THREAD_ID aimed at a computing thread and 25 at one blocked in read();
 * the computing thread is replaced on alternate rounds, after which its old TID is
 * refused. Then a periodic timer expires while blocked: one delivery with overruns.
 * replay (timerstormcounts): adds the overrun and delivery counts. */
#define TS_SIG (SIGRTMIN + 9)
#define TS_TID (SIGRTMIN + 10)
#define TS_PER (SIGRTMIN + 12)
#define TS_N 200
static volatile int ts_seen[TS_N], ts_total, ts_cstop, ts_cready, ts_per_n, ts_per_over;
static volatile long ts_wrong, ts_codebad, ts_eintr;
static volatile pid_t ts_main, ts_compute, ts_blocked;
static timer_t ts_pert;
static int ts_pipe[2];
static void ts_note(int v, int sigev) {
    pid_t t = gettid();
    if (v >= 0 && v < TS_N) {
        __atomic_add_fetch(&ts_seen[v], 1, __ATOMIC_RELAXED);
        int bad = sigev ? (t == ts_main || t == ts_compute || t == ts_blocked)
                  : v < 100 ? (t == ts_compute || t == ts_blocked) : t != (v < 175 ? ts_compute : ts_blocked);
        if (bad) __atomic_add_fetch(&ts_wrong, 1, __ATOMIC_RELAXED);
    }
    __atomic_add_fetch(&ts_total, 1, __ATOMIC_RELEASE);
    cfz_wake(&ts_total);
}
static void ts_sig_h(int s, siginfo_t *si, void *uc) {
    (void)s; (void)uc;
    if (si->si_code != SI_TIMER) __atomic_add_fetch(&ts_codebad, 1, __ATOMIC_RELAXED);
    ts_note(si->si_value.sival_int, 0);
}
static void ts_thread_fn(union sigval v) { ts_note(v.sival_int, 1); }
static void ts_per_h(int s, siginfo_t *si, void *uc) {
    (void)s; (void)si; (void)uc;
    if (!ts_per_n++) {
        ts_per_over = timer_getoverrun(ts_pert);
        timer_delete(ts_pert);
    }
}
static void *ts_compute_fn(void *p) {
    (void)p;
    sigset_t s;
    sigemptyset(&s);
    sigaddset(&s, TS_SIG);
    sigaddset(&s, TS_PER);
    pthread_sigmask(SIG_BLOCK, &s, NULL);
    ts_compute = gettid();
    __atomic_store_n(&ts_cready, 1, __ATOMIC_RELEASE);
    volatile uint64_t h = 1;
    while (!__atomic_load_n(&ts_cstop, __ATOMIC_ACQUIRE)) h = h * 6364136223846793005ULL + 1;
    return NULL;
}
static void *ts_blocked_fn(void *p) {
    (void)p;
    ts_blocked = gettid();
    char b;
    for (;;) {
        ssize_t n = read(ts_pipe[0], &b, 1);
        if (n == 1 && b == 'q') break;
        if (n < 0 && errno == EINTR) ts_eintr++;
        else if (n <= 0) break;
    }
    return NULL;
}
static int timerstorm(int counts) {
    ts_main = gettid();
    sigset_t s;
    sigemptyset(&s);
    sigaddset(&s, TS_SIG);
    sigaddset(&s, TS_PER);
    pthread_sigmask(SIG_BLOCK, &s, NULL);
    struct sigaction a;
    memset(&a, 0, sizeof a);
    a.sa_sigaction = ts_sig_h;
    a.sa_flags = SA_SIGINFO | SA_RESTART;
    sigemptyset(&a.sa_mask);
    sigaction(TS_SIG, &a, NULL);
    sigaction(TS_TID, &a, NULL);
    a.sa_sigaction = ts_per_h;
    sigaction(TS_PER, &a, NULL);
    if (pipe(ts_pipe)) return 0;
    pthread_t comp, blk;
    ts_cstop = 0;
    ts_cready = 0;
    pthread_create(&comp, NULL, ts_compute_fn, NULL);
    pthread_create(&blk, NULL, ts_blocked_fn, NULL);
    cfz_wait_for(&ts_cready, 1, 5000);
    while (!ts_blocked) cfz_sleep_us(100);
    sigset_t only;
    sigemptyset(&only);
    sigaddset(&only, TS_SIG);
    pthread_sigmask(SIG_UNBLOCK, &only, NULL);
    cfz_settle(getpid(), ts_blocked, SYS_read, 0);
    long rounds = 10 * scale, created = 0, armed = 0, once = 0, missing = 0, dups = 0, complete = 0, deadEinval = 0, replaced = 0;
    for (long r = 0; r < rounds; r++) {
        for (int i = 0; i < TS_N; i++) ts_seen[i] = 0;
        ts_total = 0;
        timer_t tm[TS_N];
        int ok[TS_N];
        for (int i = 0; i < TS_N; i++) {
            struct sigevent ev;
            memset(&ev, 0, sizeof ev);
            ev.sigev_value.sival_int = i;
            if (i < 100) { ev.sigev_notify = SIGEV_SIGNAL; ev.sigev_signo = TS_SIG; }
            else if (i < 150) { ev.sigev_notify = SIGEV_THREAD; ev.sigev_notify_function = ts_thread_fn; }
            else {
                ev.sigev_notify = SIGEV_THREAD_ID;
                ev.sigev_signo = TS_TID;
                ev._sigev_un._tid = i < 175 ? ts_compute : ts_blocked;
            }
            ok[i] = timer_create(CLOCK_MONOTONIC, &ev, &tm[i]) == 0;
            created += ok[i];
        }
        for (int i = 0; i < TS_N; i++) {
            struct itimerspec its = {{0, 0}, {0, 1000000L + (i % 20) * 100000L}};
            if (ok[i]) armed += timer_settime(tm[i], 0, &its, NULL) == 0;
        }
        complete += cfz_wait_for(&ts_total, TS_N, 10000);
        cfz_sleep_us(2000);
        for (int i = 0; i < TS_N; i++) {
            once += ts_seen[i] == 1;
            missing += ts_seen[i] == 0;
            dups += ts_seen[i] > 1;
            if (ok[i]) timer_delete(tm[i]);
        }
        if (r & 1) {
            pid_t old = ts_compute;
            __atomic_store_n(&ts_cstop, 1, __ATOMIC_RELEASE);
            pthread_join(comp, NULL);
            ts_cstop = 0;
            ts_cready = 0;
            pthread_create(&comp, NULL, ts_compute_fn, NULL);
            cfz_wait_for(&ts_cready, 1, 5000);
            replaced += ts_compute != old;
            /* pthread_join returns before the kernel has released the thread. */
            char task[64];
            snprintf(task, sizeof task, "/proc/self/task/%d", (int)old);
            for (int w = 0; w < 10000 && !access(task, F_OK); w++) cfz_sleep_us(100);
            struct sigevent ev;
            memset(&ev, 0, sizeof ev);
            ev.sigev_notify = SIGEV_THREAD_ID;
            ev.sigev_signo = TS_TID;
            ev._sigev_un._tid = old;
            timer_t t;
            int e = timer_create(CLOCK_MONOTONIC, &ev, &t);
            deadEinval += e == -1 && errno == EINVAL;
            if (!e) timer_delete(t);
        }
    }
    __atomic_store_n(&ts_cstop, 1, __ATOMIC_RELEASE);
    pthread_join(comp, NULL);
    if (write(ts_pipe[1], "q", 1) != 1) return 0;
    pthread_join(blk, NULL);
    /* A periodic timer expiring while its signal is blocked: one pending signal, overruns. */
    sigset_t per;
    sigemptyset(&per);
    sigaddset(&per, TS_PER);
    struct sigevent ev;
    memset(&ev, 0, sizeof ev);
    ev.sigev_notify = SIGEV_SIGNAL;
    ev.sigev_signo = TS_PER;
    ev.sigev_value.sival_int = 999;
    ts_per_n = 0;
    ts_per_over = -1;
    int pok = timer_create(CLOCK_MONOTONIC, &ev, &ts_pert) == 0;
    struct itimerspec its = {{0, 5000000L}, {0, 5000000L}};
    if (pok) timer_settime(ts_pert, 0, &its, NULL);
    cfz_sleep_us(50000);
    pthread_sigmask(SIG_UNBLOCK, &per, NULL);
    cfz_sleep_us(1000);
    if (!ts_per_n && pok) timer_delete(ts_pert);
    signal(TS_SIG, SIG_DFL);
    signal(TS_TID, SIG_DFL);
    signal(TS_PER, SIG_DFL);
    close(ts_pipe[0]);
    close(ts_pipe[1]);
    long total = rounds * TS_N;
    OUT("created", "%ld/%ld armed %ld/%ld", created, total, armed, total);
    OUT("delivered", "complete %ld/%ld once %ld/%ld missing %ld duplicates %ld", complete, rounds, once, total, missing, dups);
    OUT("targets", "wrongThread %ld codeBad %ld readEintr %ld", (long)ts_wrong, (long)ts_codebad, (long)ts_eintr);
    OUT("replaced", "%ld/%ld deadTidEinval %ld/%ld", replaced, rounds / 2, deadEinval, rounds / 2);
    OUT("periodic", "created %d delivered %d overrunPositive %d", pok, ts_per_n > 0, ts_per_over > 0);
    if (counts) OUT("periodicCounts", "deliveries %d overrun %d", (int)ts_per_n, (int)ts_per_over);
    return 1;
}
static int t_timerstorm(void) { return timerstorm(0); }
static int t_timerstormcounts(void) { return timerstorm(1); }

/* ---- progress charges, atomics, exclusives and restored FP state ---- */

/* Counted loops for chargelin; each returns a checksum and executes an exact number
 * of instructions: straight 64 per iteration, tight 4, blr/ret 5, poll 5; ivau runs
 * blocks of 250,000 tight iterations, each followed by IC IVAU of its own loop, DSB
 * and ISB; signal runs blocks of 25,000 tight iterations, each followed by a tgkill. */
CFZ_HIDDEN uint64_t cfz_cl_straight(long iters);
CFZ_HIDDEN uint64_t cfz_cl_tight(long iters);
CFZ_HIDDEN uint64_t cfz_cl_blr(long iters);
CFZ_HIDDEN uint64_t cfz_cl_poll(long iters, volatile int *flag);
CFZ_HIDDEN uint64_t cfz_cl_ivau(long blocks);
CFZ_HIDDEN uint64_t cfz_cl_signal(long blocks, long tgid, long tid);
asm(".text\n.p2align 6\n"
    CFZ_FN("cfz_cl_straight") "mov x1, #1\nmov x2, #3\n1:\n.rept 31\nadd x1, x1, x2\neor x2, x2, x1, ror #7\n.endr\n"
    "subs x0, x0, #1\nb.ne 1b\nmov x0, x1\nret\n"
    ".p2align 6\n" CFZ_FN("cfz_cl_tight") "mov x1, #1\nmov x2, #3\n1: add x1, x1, x2\neor x2, x2, x1, ror #7\n"
    "subs x0, x0, #1\nb.ne 1b\nmov x0, x1\nret\n"
    ".p2align 6\n" CFZ_FN("cfz_cl_callee") "add x1, x1, #3\nret\n"
    ".p2align 6\n" CFZ_FN("cfz_cl_blr") "stp x29, x30, [sp, #-16]!\nmov x1, #1\nadr x9, cfz_cl_callee\n"
    "1: blr x9\nsubs x0, x0, #1\nb.ne 1b\nmov x0, x1\nldp x29, x30, [sp], #16\nret\n"
    ".p2align 6\n" CFZ_FN("cfz_cl_poll") "mov x2, #1\n1: ldr w3, [x1]\ncbnz w3, 2f\nadd x2, x2, #5\nsubs x0, x0, #1\n"
    "b.ne 1b\n2: mov x0, x2\nret\n"
    ".p2align 6\n" CFZ_FN("cfz_cl_ivau") "mov x1, #1\nmov x2, #3\nadr x5, 1f\n3: mov x4, #0xd090\nmovk x4, #0x3, lsl #16\n"
    "1: add x1, x1, x2\neor x2, x2, x1, ror #7\nsubs x4, x4, #1\nb.ne 1b\nic ivau, x5\ndsb ish\nisb\n"
    "subs x0, x0, #1\nb.ne 3b\nmov x0, x1\nret\n"
    ".p2align 6\n" CFZ_FN("cfz_cl_signal") "mov x9, x0\nmov x10, x1\nmov x11, x2\nmov x12, #1\nmov x13, #3\n"
    "3: mov x4, #25000\n1: add x12, x12, x13\neor x13, x13, x12, ror #7\nsubs x4, x4, #1\nb.ne 1b\n"
    "mov x0, x10\nmov x1, x11\nmov x2, #10\nmov x8, #131\nsvc #0\nsubs x9, x9, #1\nb.ne 3b\nmov x0, x12\nret\n");
static volatile int cl_flag, cl_sigs;
static void cl_usr1(int s) { (void)s; cl_sigs++; }
#define CL_SHAPES 6
#define CL_REPS 5
static uint64_t cl_run(int shape, long m) {
    switch (shape) {
    case 0: return cfz_cl_straight(156250 * m);
    case 1: return cfz_cl_tight(2500000 * m);
    case 2: return cfz_cl_blr(2000000 * m);
    case 3: return cfz_cl_poll(2000000 * m, &cl_flag);
    case 4: return cfz_cl_ivau(10 * m);
    default: return cfz_cl_signal(100 * m, getpid(), gettid());
    }
}
/* chargelin (det): each shape at 10M and 20M application instructions, warmed, timed
 * CL_REPS times with the same clock_gettime wrapper. Prints checksums, the delivered
 * signal count and loose bounds that also hold natively: the 20M minimum is 1.3 to
 * 3 times the 10M minimum.
 * chargelinvalues (replay): adds every elapsed time and, as "sb*" predicates, the
 * relations Seedbox's progress charges must satisfy with cpu.jitter zero (one
 * progress call per 50,000 instructions, so 10M instructions are 200 calls):
 *   sbLinear  |t20 - 2 t10| <= 3 calls; sbSpread  every repetition within 2 calls of
 *   its minimum; sbAsStraight (tight, blrRet and poll only) |t10 - straight t10| <= 3
 *   calls. ivau and signal add translator or kernel work and are compared only with
 *   themselves and with the replay. Natively the sb predicates usually fail. */
static int chargelin(int values) {
    const char *names[CL_SHAPES] = {"straight", "tight", "blrRet", "poll", "ivau", "signal"};
    signal(SIGUSR1, cl_usr1);
    cl_flag = 0;
    cl_sigs = 0;
    uint64_t t[CL_SHAPES][2][CL_REPS], sum[CL_SHAPES][2];
    int stable[CL_SHAPES];
    for (int s = 0; s < CL_SHAPES; s++) {
        sum[s][0] = cl_run(s, 1);
        sum[s][1] = cl_run(s, 2);
        stable[s] = 1;
    }
    long reps = CL_REPS;
    for (long r = 0; r < reps; r++)
        for (int s = 0; s < CL_SHAPES; s++)
            for (int z = 0; z < 2; z++) {
                uint64_t t0 = mono_ns(CLOCK_MONOTONIC);
                uint64_t h = cl_run(s, z + 1);
                uint64_t t1 = mono_ns(CLOCK_MONOTONIC);
                t[s][z][r] = t1 - t0;
                stable[s] &= h == sum[s][z];
            }
    signal(SIGUSR1, SIG_DFL);
    uint64_t mn[CL_SHAPES][2], mx[CL_SHAPES][2];
    for (int s = 0; s < CL_SHAPES; s++)
        for (int z = 0; z < 2; z++) {
            mn[s][z] = mx[s][z] = t[s][z][0];
            for (long r = 1; r < reps; r++) {
                if (t[s][z][r] < mn[s][z]) mn[s][z] = t[s][z][r];
                if (t[s][z][r] > mx[s][z]) mx[s][z] = t[s][z][r];
            }
        }
    for (int s = 0; s < CL_SHAPES; s++) {
        uint64_t a = mn[s][0], b = mn[s][1];
        OUT(names[s], "sum %lx %lx stable %d scales %d", (unsigned long)sum[s][0], (unsigned long)sum[s][1], stable[s],
            13 * a <= 10 * b && b <= 3 * a);
    }
    OUT("signals", "%d/%ld", (int)cl_sigs, (1 + reps) * 300);
    if (!values) return 1;
    for (int s = 0; s < CL_SHAPES; s++) {
        char line[512];
        int len = 0;
        for (int z = 0; z < 2; z++) {
            len += snprintf(line + len, sizeof line - (size_t)len, "%s", z ? " ns20" : "ns10");
            for (long r = 0; r < reps; r++) len += snprintf(line + len, sizeof line - (size_t)len, " %lu", (unsigned long)t[s][z][r]);
        }
        uint64_t a = mn[s][0], b = mn[s][1], call = a / 200;
        uint64_t lin = b > 2 * a ? b - 2 * a : 2 * a - b;
        int spread = mx[s][0] - mn[s][0] <= 2 * call && mx[s][1] - mn[s][1] <= 2 * call;
        len += snprintf(line + len, sizeof line - (size_t)len, " sbLinear %d sbSpread %d", lin <= 3 * call, spread);
        if (s >= 1 && s <= 3) {
            uint64_t base = mn[0][0], d = a > base ? a - base : base - a;
            snprintf(line + len, sizeof line - (size_t)len, " sbAsStraight %d", d <= 3 * (base / 200));
        }
        OUT(names[s], "%s", line);
    }
    return 1;
}
static int t_chargelin(void) { return chargelin(0); }
static int t_chargelinvalues(void) { return chargelin(1); }

/* det: casal, ldaddal, swpal, ldxr/stxr and ldaxp/stlxp at every offset of a 16-byte
 * granule, across a cache line, across a page, into a PROT_NONE page and inside it.
 * One character per offset: '.' success with the right memory and result, 'x' wrong;
 * a fault is B (SIGBUS BUS_ADRALN), S (SEGV_ACCERR), M (SEGV_MAPERR), O (other), in
 * lower case when si_addr is not the access address, W when memory changed. sites:
 * 1 the load (or only) instruction faulted, 2 the store-exclusive, 3 elsewhere. */
CFZ_HIDDEN uint64_t cfz_ua_casal(void *p, uint64_t expect, uint64_t val);
CFZ_HIDDEN uint64_t cfz_ua_ldadd(void *p, uint64_t add);
CFZ_HIDDEN uint64_t cfz_ua_swp(void *p, uint64_t val);
CFZ_HIDDEN uint64_t cfz_ua_xpair(void *p, uint64_t add);
CFZ_HIDDEN uint64_t cfz_ua_ppair(void *p, uint64_t add);
CFZ_HIDDEN extern const char cfz_ua_start[], cfz_ua_end[], cfz_ua_casal_at[], cfz_ua_ldadd_at[], cfz_ua_swp_at[],
    cfz_ua_xpair_ld[], cfz_ua_xpair_st[], cfz_ua_ppair_ld[], cfz_ua_ppair_st[];
asm(".text\n.p2align 2\n.arch_extension lse\n" CFZ_AT("cfz_ua_start")
    CFZ_FN("cfz_ua_casal") "mov x3, x1\n" CFZ_AT("cfz_ua_casal_at") "casal x3, x2, [x0]\nmov x0, x3\nret\n"
    CFZ_FN("cfz_ua_ldadd") CFZ_AT("cfz_ua_ldadd_at") "ldaddal x1, x2, [x0]\nmov x0, x2\nret\n"
    CFZ_FN("cfz_ua_swp") CFZ_AT("cfz_ua_swp_at") "swpal x1, x2, [x0]\nmov x0, x2\nret\n"
    CFZ_FN("cfz_ua_xpair") "mov x4, #1000\n1:\n" CFZ_AT("cfz_ua_xpair_ld") "ldxr x2, [x0]\nadd x3, x2, x1\n"
    CFZ_AT("cfz_ua_xpair_st") "stxr w5, x3, [x0]\ncbz w5, 2f\nsubs x4, x4, #1\nb.ne 1b\nmov x0, #-1\nret\n2: mov x0, x2\nret\n"
    CFZ_FN("cfz_ua_ppair") "mov x4, #1000\n1:\n" CFZ_AT("cfz_ua_ppair_ld") "ldaxp x2, x3, [x0]\nadd x6, x2, x1\nadd x7, x3, x1\n"
    CFZ_AT("cfz_ua_ppair_st") "stlxp w5, x6, x7, [x0]\ncbz w5, 2f\nsubs x4, x4, #1\nb.ne 1b\nmov x0, #-1\nret\n"
    "2: mov x0, x2\nret\n" CFZ_AT("cfz_ua_end"));
static volatile struct { int sig, code, site; uintptr_t addr; } ua_r;
static void ua_handler(int s, siginfo_t *si, void *ucv) {
    ucontext_t *uc = ucv;
    uintptr_t pc = uc->uc_mcontext.pc;
    if (pc < (uintptr_t)cfz_ua_start || pc >= (uintptr_t)cfz_ua_end) { signal(s, SIG_DFL); return; }
    ua_r.sig = s;
    ua_r.code = si->si_code;
    ua_r.addr = (uintptr_t)si->si_addr;
    ua_r.site = (pc == (uintptr_t)cfz_ua_casal_at || pc == (uintptr_t)cfz_ua_ldadd_at || pc == (uintptr_t)cfz_ua_swp_at ||
                 pc == (uintptr_t)cfz_ua_xpair_ld || pc == (uintptr_t)cfz_ua_ppair_ld) ? 1
                : (pc == (uintptr_t)cfz_ua_xpair_st || pc == (uintptr_t)cfz_ua_ppair_st) ? 2 : 3;
    uc->uc_mcontext.pc = uc->uc_mcontext.regs[30];
    uc->uc_mcontext.regs[0] = 0xfa17fa17fa17fa17ULL;
}
/* One access at p of which the first rw bytes are readable and writable. */
static char ua_case(int op, unsigned char *p, size_t rw, char *site) {
    size_t n = op == 4 ? 16 : 8, k = rw < n ? rw : n;
    const uint64_t K = 0x0102030405060708ULL;
    unsigned char before[16], after[16], want[16];
    memcpy(before, p, k);
    uint64_t o0 = 0, o1 = 0;
    if (k == n) { memcpy(&o0, before, 8); if (n == 16) memcpy(&o1, before + 8, 8); }
    ua_r.sig = 0;
    uint64_t ret;
    switch (op) {
    case 0: ret = cfz_ua_casal(p, o0, o0 ^ 0x1111111111111111ULL); o0 ^= 0x1111111111111111ULL; break;
    case 1: ret = cfz_ua_ldadd(p, K); o0 += K; break;
    case 2: ret = cfz_ua_swp(p, K); o0 = K; break;
    case 3: ret = cfz_ua_xpair(p, K); o0 += K; break;
    default: ret = cfz_ua_ppair(p, K); o0 += K; o1 += K; break;
    }
    memcpy(after, p, k);
    if (!ua_r.sig) {
        *site = '-';
        if (k != n) return 'x';
        uint64_t old;
        memcpy(&old, before, 8);
        memcpy(want, &o0, 8);
        memcpy(want + 8, &o1, 8);
        return !memcmp(after, want, n) && ret == old ? '.' : 'x';
    }
    *site = (char)('0' + ua_r.site);
    if (memcmp(after, before, k)) return 'W';
    char c = ua_r.sig == SIGBUS && ua_r.code == BUS_ADRALN ? 'B'
             : ua_r.sig == SIGSEGV && ua_r.code == SEGV_ACCERR ? 'S'
             : ua_r.sig == SIGSEGV && ua_r.code == SEGV_MAPERR ? 'M' : 'O';
    return ua_r.addr == (uintptr_t)p ? c : (char)(c - 'A' + 'a');
}
static int t_unaligned(void) {
    if (!(getauxval(AT_HWCAP) & (1 << 8))) { puts("UNSUPPORTED lse"); return 0; }
    unsigned char *m = mmap(0, 4 * 4096, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (m == MAP_FAILED) return 0;
    mprotect(m + 2 * 4096, 4096, PROT_NONE);
    struct sigaction a, ob, os;
    memset(&a, 0, sizeof a);
    a.sa_sigaction = ua_handler;
    a.sa_flags = SA_SIGINFO | SA_NODEFER;
    sigemptyset(&a.sa_mask);
    sigaction(SIGBUS, &a, &ob);
    sigaction(SIGSEGV, &a, &os);
    const char *ops[5] = {"casal", "ldadd", "swp", "ldxrStxr", "ldaxpStlxp"};
    const char *regions[5] = {"granule", "line", "page", "intoProtNone", "inProtNone"};
    static const int inside[4] = {0, 1, 4, 8};
    char first[5][5][2][17];
    long rounds = 200 * scale, stable = 0;
    for (long r = 0; r < rounds; r++) {
        int same = 1;
        for (int i = 0; i < 2 * 4096; i++) m[i] = (unsigned char)(i * 7 + 3 + r);
        for (int op = 0; op < 5; op++)
            for (int g = 0; g < 5; g++) {
                char kinds[17], sites[17];
                int cnt = g == 4 ? 4 : 16;
                for (int i = 0; i < cnt; i++) {
                    unsigned char *p;
                    size_t rw;
                    switch (g) {
                    case 0: p = m + 0x100 + i; rw = 16; break;
                    case 1: p = m + 0x230 + i; rw = 16; break;
                    case 2: p = m + 4096 - 16 + i; rw = 16; break;
                    case 3: p = m + 2 * 4096 - 16 + i; rw = (size_t)(16 - i); break;
                    default: p = m + 2 * 4096 + inside[i]; rw = 0; break;
                    }
                    kinds[i] = ua_case(op, p, rw, &sites[i]);
                }
                kinds[cnt] = sites[cnt] = 0;
                if (!r) { memcpy(first[op][g][0], kinds, 17); memcpy(first[op][g][1], sites, 17); }
                else same &= !strcmp(first[op][g][0], kinds) && !strcmp(first[op][g][1], sites);
            }
        stable += r > 0 && same;
    }
    sigaction(SIGBUS, &ob, NULL);
    sigaction(SIGSEGV, &os, NULL);
    munmap(m, 4 * 4096);
    OUT("uscat", "%d", !!(getauxval(AT_HWCAP) & (1 << 25)));
    for (int op = 0; op < 5; op++)
        for (int g = 0; g < 5; g++) {
            char key[64];
            snprintf(key, sizeof key, "%s.%s", ops[op], regions[g]);
            OUT(key, "kinds %s sites %s", first[op][g][0], first[op][g][1]);
        }
    OUT("stable", "%ld/%ld", stable, rounds - 1 > 0 ? rounds - 1 : 0);
    return 1;
}

/* exclmaint (det): two threads increment a shared word and private words with
 * ldxr/stxr retry loops while a maintenance thread flushes JIT code, toggles page
 * protection, runs expedited membarriers, forks (copy-on-write of the words' page)
 * and sends signals; afterwards single-thread probes put a getpid, a signal whose
 * handler leaves the word alone, or one that changes and restores it (ABA) between
 * ldxr and stxr. Prints final data and whether each probe's outcome agrees with it.
 * exclmaintcounts (replay): adds STXR failure counts and probe success counts, which
 * depend on host interruptions natively and on reservation policy under Seedbox. */
CFZ_HIDDEN uint64_t cfz_ex_inc(volatile uint64_t *w, long n, uint64_t *fails);
CFZ_HIDDEN uint64_t cfz_ex_probe(volatile uint64_t *w, long a0, long a1, long a2, long nr);
asm(".text\n.p2align 2\n"
    CFZ_FN("cfz_ex_inc") "mov x9, #0\n1: ldxr x3, [x0]\nadd x3, x3, #1\nstxr w4, x3, [x0]\ncbz w4, 2f\nadd x9, x9, #1\nb 1b\n"
    "2: subs x1, x1, #1\nb.ne 1b\nldr x10, [x2]\nadd x10, x10, x9\nstr x10, [x2]\nmov x0, x3\nret\n"
    CFZ_FN("cfz_ex_probe") "mov x9, x0\nmov x8, x4\nldxr x10, [x9]\nadd x10, x10, #1\nmov x0, x1\nmov x1, x2\nmov x2, x3\n"
    "svc #0\nstxr w11, x10, [x9]\nmov x0, x11\nret\n");
struct ex_line { volatile uint64_t v; char pad[56]; };
static struct ex_line ex_shared __attribute__((aligned(64))), ex_priv[2] __attribute__((aligned(64)));
static volatile uint64_t ex_probe_word __attribute__((aligned(64)));
static uint64_t ex_fails[2];
static volatile int ex_done, ex_sigs, ex_aba;
static pid_t ex_tid[2];
static void ex_usr1(int s) { (void)s; ex_sigs++; }
static void ex_usr2(int s) {
    (void)s;
    if (ex_aba) {
        uint64_t v = ex_probe_word;
        ex_probe_word = v + 1000;
        ex_probe_word = v;
    }
}
static void *ex_worker(void *p) {
    long k = (long)p, chunks = 20000 * scale;
    ex_tid[k] = gettid();
    for (long c = 0; c < chunks; c++) {
        cfz_ex_inc(&ex_shared.v, 500, &ex_fails[k]);
        cfz_ex_inc(&ex_priv[k].v, 500, &ex_fails[k]);
    }
    __atomic_add_fetch(&ex_done, 1, __ATOMIC_RELEASE);
    return NULL;
}
static int exclmaint(int counts) {
    signal(SIGUSR1, ex_usr1);
    signal(SIGUSR2, ex_usr2);
    ex_shared.v = ex_priv[0].v = ex_priv[1].v = 0;
    ex_fails[0] = ex_fails[1] = 0;
    ex_done = 0;
    int mb = syscall(SYS_membarrier, MEMBARRIER_CMD_REGISTER_PRIVATE_EXPEDITED, 0, 0) == 0;
    char *scratch = mmap(0, 4096, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    struct jitw j;
    jit_init(&j);
    pthread_t t[2];
    ex_tid[0] = ex_tid[1] = 0;
    for (long k = 0; k < 2; k++) pthread_create(&t[k], NULL, ex_worker, (void *)k);
    long kinds[5] = {0}, forkok = 0;
    /* At least 400 rounds of maintenance, overlapping the workers as far as they run. */
    for (long i = 0; i < 400 || __atomic_load_n(&ex_done, __ATOMIC_ACQUIRE) < 2; i++) {
        switch (i % 5) {
        case 0: jit_step(&j, i); kinds[0]++; break;
        case 1: mprotect(scratch, 4096, PROT_READ); mprotect(scratch, 4096, PROT_READ | PROT_WRITE); kinds[1]++; break;
        case 2: if (mb) syscall(SYS_membarrier, MEMBARRIER_CMD_PRIVATE_EXPEDITED, 0, 0); kinds[2]++; break;
        case 3:
            if (i % 40 == 3) {
                pid_t c = fork();
                if (!c) _exit(0);
                int st = 0;
                if (c > 0 && waitpid(c, &st, 0) == c && WIFEXITED(st)) forkok++;
                kinds[3]++;
            }
            break;
        default: {
            pid_t tid = ex_tid[i / 5 % 2];
            if (tid) syscall(SYS_tgkill, getpid(), tid, SIGUSR1);
            kinds[4]++;
        }
        }
        sched_yield();
    }
    for (int k = 0; k < 2; k++) pthread_join(t[k], NULL);
    uint64_t want = 20000ULL * (uint64_t)scale * 500;
    long probes = 200, succ[3] = {0}, consistent = 0;
    for (int v = 0; v < 3; v++) {
        ex_aba = v == 2;
        for (long i = 0; i < probes; i++) {
            uint64_t before = ex_probe_word;
            uint64_t st = v == 0 ? cfz_ex_probe(&ex_probe_word, 0, 0, 0, SYS_getpid)
                                 : cfz_ex_probe(&ex_probe_word, getpid(), gettid(), SIGUSR2, SYS_tgkill);
            succ[v] += st == 0;
            consistent += ex_probe_word == before + (st == 0);
        }
    }
    signal(SIGUSR1, SIG_DFL);
    signal(SIGUSR2, SIG_DFL);
    munmap(scratch, 4096);
    OUT("shared", "%lu/%lu", (unsigned long)ex_shared.v, (unsigned long)(2 * want));
    OUT("private", "%lu/%lu %lu/%lu", (unsigned long)ex_priv[0].v, (unsigned long)want, (unsigned long)ex_priv[1].v,
        (unsigned long)want);
    OUT("maintenance", "flush %d mprotect %d membarrier %d fork %d signal %d forksOk %d", kinds[0] > 0, kinds[1] > 0,
        kinds[2] > 0 && mb, kinds[3] > 0, kinds[4] > 0, forkok == kinds[3]);
    OUT("probes", "consistent %ld/%ld", consistent, 3 * probes);
    if (counts) {
        OUT("stxrFailures", "%lu %lu", (unsigned long)ex_fails[0], (unsigned long)ex_fails[1]);
        OUT("probeSuccesses", "getpid %ld/%ld signal %ld/%ld aba %ld/%ld", succ[0], probes, succ[1], probes, succ[2], probes);
        OUT("maintenanceCounts", "%ld %ld %ld %ld %ld signals %d", kinds[0], kinds[1], kinds[2], kinds[3], kinds[4], (int)ex_sigs);
    }
    return 1;
}
static int t_exclmaint(void) { return exclmaint(0); }
static int t_exclmaintcounts(void) { return exclmaint(1); }

/* det: two threads keep distinct values in v0-v27, v29 and v30 and their own FPCR
 * (rounding mode, FZ or DN) and FPSR sticky bits while repeating a rounding-dependent
 * division, with a getpid every 1024 iterations; another thread signals them (the
 * handler checks the saved FP record, then changes FPCR, FPSR and vector registers)
 * and a third rewrites and flushes JIT code. Each thread reports its quotient and
 * whether registers, FPCR and FPSR survived. */
struct sfc { __uint128_t in[32]; uint64_t fpcr, fpsr, iters, pad; __uint128_t out[32]; uint64_t ofpcr, ofpsr, bad, quot; };
_Static_assert(offsetof(struct sfc, out) == 544 && offsetof(struct sfc, ofpcr) == 1056, "sfc layout");
CFZ_HIDDEN void cfz_sfc_soak(struct sfc *s);
CFZ_HIDDEN extern const char cfz_sfc_loop[], cfz_sfc_loop_end[];
asm(".text\n.p2align 2\n"
    CFZ_FN("cfz_sfc_soak")
    "stp x29, x30, [sp, #-80]!\nmov x29, sp\nstp d8, d9, [sp, #16]\nstp d10, d11, [sp, #32]\nstp d12, d13, [sp, #48]\n"
    "stp d14, d15, [sp, #64]\nmov x9, x0\nldr x10, [x9, #512]\nmsr fpcr, x10\nldr x10, [x9, #520]\nmsr fpsr, x10\n"
    "ldp q0, q1, [x9, #0]\nldp q2, q3, [x9, #32]\nldp q4, q5, [x9, #64]\nldp q6, q7, [x9, #96]\n"
    "ldp q8, q9, [x9, #128]\nldp q10, q11, [x9, #160]\nldp q12, q13, [x9, #192]\nldp q14, q15, [x9, #224]\n"
    "ldp q16, q17, [x9, #256]\nldp q18, q19, [x9, #288]\nldp q20, q21, [x9, #320]\nldp q22, q23, [x9, #352]\n"
    "ldp q24, q25, [x9, #384]\nldp q26, q27, [x9, #416]\nldp q28, q29, [x9, #448]\nldp q30, q31, [x9, #480]\n"
    "ldr x11, [x9, #528]\nmov x12, #0\nfdiv d28, d29, d30\nfmov x13, d28\nstr x13, [x9, #1080]\n"
    CFZ_AT("cfz_sfc_loop")
    "fdiv d28, d29, d30\nfmov x14, d28\ncmp x14, x13\ncinc x12, x12, ne\nfmul d31, d28, d30\n"
    "tst x11, #1023\nb.ne 2f\nmov x8, #172\nsvc #0\n2: subs x11, x11, #1\nb.ne " "cfz_sfc_loop\n"
    CFZ_AT("cfz_sfc_loop_end")
    "mrs x10, fpcr\nstr x10, [x9, #1056]\nmrs x10, fpsr\nstr x10, [x9, #1064]\nstr x12, [x9, #1072]\nadd x10, x9, #544\n"
    "stp q0, q1, [x10, #0]\nstp q2, q3, [x10, #32]\nstp q4, q5, [x10, #64]\nstp q6, q7, [x10, #96]\n"
    "stp q8, q9, [x10, #128]\nstp q10, q11, [x10, #160]\nstp q12, q13, [x10, #192]\nstp q14, q15, [x10, #224]\n"
    "stp q16, q17, [x10, #256]\nstp q18, q19, [x10, #288]\nstp q20, q21, [x10, #320]\nstp q22, q23, [x10, #352]\n"
    "stp q24, q25, [x10, #384]\nstp q26, q27, [x10, #416]\nstp q28, q29, [x10, #448]\nstp q30, q31, [x10, #480]\n"
    "ldp d8, d9, [sp, #16]\nldp d10, d11, [sp, #32]\nldp d12, d13, [sp, #48]\nldp d14, d15, [sp, #64]\n"
    "ldp x29, x30, [sp], #80\nret\n");
struct sfc_thread { struct sfc s; pid_t tid; volatile int ready; long sigs, framebad, framechecked; };
static struct sfc_thread sfc_t[2];
static __thread struct sfc_thread *sfc_self;
static volatile int sfc_stop, sfc_go;
/* v28 holds the quotient and v31 scratch; every other vector register is a canary. */
static int sfc_live(int k) { return k != 28 && k != 31; }
static void sfc_handler(int s, siginfo_t *si, void *ucv) {
    (void)s; (void)si;
    ucontext_t *uc = ucv;
    struct sfc_thread *t = sfc_self;
    if (!t) return;
    t->sigs++;
    uintptr_t pc = uc->uc_mcontext.pc;
    if (pc >= (uintptr_t)cfz_sfc_loop && pc < (uintptr_t)cfz_sfc_loop_end) {
        struct cfz_walk w;
        ctx_walk(uc, &w);
        t->framechecked++;
        int bad = !w.fp || w.fp->fpcr != (uint32_t)t->s.fpcr || (w.fp->fpsr & (uint32_t)t->s.fpsr) != (uint32_t)t->s.fpsr;
        if (w.fp)
            for (int i = 0; i < 32; i++)
                if (sfc_live(i) && w.fp->vregs[i] != t->s.in[i]) bad = 1;
        t->framebad += bad;
    }
    asm volatile("msr fpcr, xzr\nmsr fpsr, xzr\nmovi v0.16b, #0x5a\nmovi v16.16b, #0xa5\nmovi v29.16b, #0x33\n"
                 "fmov d30, #1.0\nmovi d1, #0\nfdiv d2, d30, d1\n" ::: "v0", "v1", "v2", "v16", "v29", "v30", "memory");
}
static void *sfc_worker(void *p) {
    struct sfc_thread *t = p;
    sfc_self = t;
    t->tid = gettid();
    __atomic_store_n(&t->ready, 1, __ATOMIC_RELEASE);
    while (!__atomic_load_n(&sfc_go, __ATOMIC_ACQUIRE)) cfz_wait(&sfc_go, 0, 100);
    cfz_sfc_soak(&t->s);
    return NULL;
}
static void *sfc_signaler(void *p) {
    (void)p;
    for (long i = 0; !__atomic_load_n(&sfc_stop, __ATOMIC_ACQUIRE); i++) {
        syscall(SYS_tgkill, getpid(), sfc_t[i & 1].tid, SIGUSR1);
        cfz_sleep_us(300);
    }
    return NULL;
}
static int t_simdfpctl(void) {
    uint64_t fpcr0;
    asm volatile("mrs %0, fpcr" : "=r"(fpcr0));
    const uint64_t modes = (3ULL << 22) | (1ULL << 24) | (1ULL << 25);
    const double num[2] = {2.0, -1.0}, den[2] = {3.0, 3.0};
    const uint64_t ctl[2] = {(1ULL << 22) | (1ULL << 24), (2ULL << 22) | (1ULL << 25)};
    const uint64_t sticky[2] = {1ULL | (1ULL << 27), (1ULL << 1) | (1ULL << 3)};
    for (int k = 0; k < 2; k++) {
        struct sfc_thread *t = &sfc_t[k];
        memset(t, 0, sizeof *t);
        for (int i = 0; i < 32; i++) t->s.in[i] = ((__uint128_t)next() << 64) | next();
        uint64_t nb, db;
        memcpy(&nb, &num[k], 8);
        memcpy(&db, &den[k], 8);
        t->s.in[29] = (t->s.in[29] & ~(__uint128_t)UINT64_MAX) | nb;
        t->s.in[30] = (t->s.in[30] & ~(__uint128_t)UINT64_MAX) | db;
        t->s.fpcr = (fpcr0 & ~modes) | ctl[k];
        t->s.fpsr = sticky[k];
        t->s.iters = 128000000UL * (uint64_t)scale;
    }
    struct sigaction a;
    memset(&a, 0, sizeof a);
    a.sa_sigaction = sfc_handler;
    a.sa_flags = SA_SIGINFO | SA_RESTART;
    sigemptyset(&a.sa_mask);
    sigaction(SIGUSR1, &a, NULL);
    sfc_stop = 0;
    sfc_go = 0;
    fl_stop = 0;
    struct jitw j;
    jit_init(&j);
    pthread_t w[2], sg, fl;
    for (int k = 0; k < 2; k++) {
        pthread_create(&w[k], NULL, sfc_worker, &sfc_t[k]);
        cfz_wait_for(&sfc_t[k].ready, 1, 5000);
    }
    pthread_create(&sg, NULL, sfc_signaler, NULL);
    pthread_create(&fl, NULL, flusher, &j);
    __atomic_store_n(&sfc_go, 1, __ATOMIC_RELEASE);
    cfz_wake(&sfc_go);
    for (int k = 0; k < 2; k++) pthread_join(w[k], NULL);
    __atomic_store_n(&sfc_stop, 1, __ATOMIC_RELEASE);
    __atomic_store_n(&fl_stop, 1, __ATOMIC_RELEASE);
    pthread_join(sg, NULL);
    pthread_join(fl, NULL);
    signal(SIGUSR1, SIG_DFL);
    for (int k = 0; k < 2; k++) {
        struct sfc_thread *t = &sfc_t[k];
        int regs = 1;
        for (int i = 0; i < 32; i++)
            if (sfc_live(i) && t->s.out[i] != t->s.in[i]) regs = 0;
        char key[8];
        snprintf(key, sizeof key, "t%d", k);
        OUT(key, "quotient %016lx bad %lu regs %d fpcr %d fpsr %d frameBad %ld signaled %d jit %d", (unsigned long)t->s.quot,
            (unsigned long)t->s.bad, regs, t->s.ofpcr == t->s.fpcr, t->s.ofpsr == (t->s.fpsr | (1ULL << 4)), t->framebad,
            t->sigs > 0, j.calls > 0 && j.stale == 0);
    }
    return 1;
}

/* det: each of two threads publishes ledger slots through an rseq critical section
 * that prepares a 32-byte payload, spins, and commits with one store of the next
 * index; SIGALRM every 100 us and the other thread interrupt attempts. Every
 * published slot must be complete and unique; reports whether aborts happened. */
struct rc_slot { uint64_t seq, x, nseq, triple; };
struct rc_thread { struct rc_slot *ledger; uint32_t commit; long n, attempts, aborts; };
static struct rc_thread rc_t[2];
static volatile sig_atomic_t rc_alarms;
static void rc_alarm(int s) { (void)s; rc_alarms++; }
static void *rc_worker(void *p) {
    struct rc_thread *t = p;
    struct rseq *r = (struct rseq *)((char *)__builtin_thread_pointer() + __rseq_offset);
    while (__atomic_load_n(&t->commit, __ATOMIC_RELAXED) < (uint32_t)t->n) {
        unsigned v;
        asm volatile(
            ".pushsection __rseq_cs, \"aw\"\n.balign 32\n1: .long 0, 0\n.quad 2f, 3f-2f, 4f\n.popsection\n"
            "adrp x9, 1b\nadd x9, x9, :lo12:1b\nstr x9, [%[r], #8]\n"
            "2: ldr w10, [%[c]]\nadd x11, %[l], x10, lsl #5\nstr x10, [x11]\neor x12, x10, #0x5555555555555555\n"
            "str x12, [x11, #8]\nmov x13, #20000\n5: subs x13, x13, #1\nb.ne 5b\nmvn x12, x10\nstr x12, [x11, #16]\n"
            "add x12, x10, x10, lsl #1\nstr x12, [x11, #24]\nadd w10, w10, #1\nstr w10, [%[c]]\n"
            "3: mov %w[v], #1\nb 6f\n.inst 0xd428bc00\n4: mov %w[v], #2\n6: str xzr, [%[r], #8]\n"
            : [v] "=&r"(v) : [r] "r"(r), [c] "r"(&t->commit), [l] "r"(t->ledger) : "x9", "x10", "x11", "x12", "x13", "cc", "memory");
        t->attempts++;
        t->aborts += v != 1;
    }
    return NULL;
}
static int t_rseqcommit(void) {
    if (__rseq_size < 20) { puts("UNSUPPORTED rseq"); return 0; }
    long n = 2000 * scale;
    for (int k = 0; k < 2; k++) {
        memset(&rc_t[k], 0, sizeof rc_t[k]);
        rc_t[k].n = n;
        rc_t[k].ledger = calloc((size_t)n + 1, sizeof(struct rc_slot));
    }
    signal(SIGALRM, rc_alarm);
    struct itimerval it = {{0, 100}, {0, 100}};
    setitimer(ITIMER_REAL, &it, NULL);
    pthread_t t[2];
    for (int k = 0; k < 2; k++) pthread_create(&t[k], NULL, rc_worker, &rc_t[k]);
    for (int k = 0; k < 2; k++) pthread_join(t[k], NULL);
    struct itimerval off = {{0, 0}, {0, 0}};
    setitimer(ITIMER_REAL, &off, NULL);
    signal(SIGALRM, SIG_DFL);
    long aborts = 0;
    for (int k = 0; k < 2; k++) {
        struct rc_thread *r = &rc_t[k];
        long ok = 0;
        for (long i = 0; i < n; i++) {
            struct rc_slot *s = &r->ledger[i];
            ok += s->seq == (uint64_t)i && s->x == ((uint64_t)i ^ 0x5555555555555555ULL) && s->nseq == ~(uint64_t)i &&
                  s->triple == 3 * (uint64_t)i;
        }
        aborts += r->aborts;
        char key[8];
        snprintf(key, sizeof key, "t%d", k);
        OUT(key, "commits %u/%ld ledgerOk %ld/%ld attemptsCover %d", r->commit, n, ok, n, r->attempts == n + r->aborts);
        free(r->ledger);
    }
    /* Not printed: whether an abort happened. Under the translator the region runs from an
     * uninstrumented native copy, and Seedbox preempts and delivers signals only at progress
     * charges, so aborts there are rare; natively they are routine. */
    (void)aborts;
    return 1;
}

/* ---- futex variants, epoll edges and seccomp user notification ---- */

/* det: FUTEX_WAKE_OP, FUTEX_CMP_REQUEUE, FUTEX_WAIT_BITSET (absolute monotonic and
 * realtime deadlines, selective bitset wakes), PI lock/unlock and requeue-PI, and
 * futex_waitv indexed wakeups reusing the array; plus errno cases. Waiters are
 * started one at a time and woken only once /proc shows them blocked, so wake order
 * and counts are fixed. Prints the first round and how many rounds matched it. */
static long fx(volatile uint32_t *u, int op, uint32_t val, const void *t, volatile uint32_t *u2, uint32_t v3) {
    long r = syscall(SYS_futex, u, op, val, t, u2, v3);
    return r < 0 ? -errno : r;
}
struct fx_waiter {
    pthread_t th;
    volatile pid_t tid;
    volatile uint32_t *word, *word2;
    int op;
    uint32_t val, bits;
    long ret, ret2, ret3;
    int owner;
    volatile int stage, done;
};
struct fx_waitv { uint64_t val, uaddr; uint32_t flags, reserved; };
static void *fx_waiter_fn(void *p) {
    struct fx_waiter *w = p;
    w->tid = gettid();
    __atomic_store_n(&w->stage, 1, __ATOMIC_RELEASE);
    switch (w->op) {
    case 0: w->ret = fx(w->word, FUTEX_WAIT_PRIVATE, w->val, NULL, NULL, 0); break;
    case 1: w->ret = fx(w->word, FUTEX_WAIT_BITSET_PRIVATE, w->val, NULL, NULL, w->bits); break;
    case 2:
        w->ret = fx(w->word, FUTEX_LOCK_PI_PRIVATE, 0, NULL, NULL, 0);
        w->owner = (*w->word & FUTEX_TID_MASK) == (uint32_t)w->tid;
        w->ret2 = fx(w->word, FUTEX_UNLOCK_PI_PRIVATE, 0, NULL, NULL, 0);
        break;
    case 3:
        w->ret = fx(w->word, FUTEX_WAIT_REQUEUE_PI_PRIVATE, w->val, NULL, w->word2, 0);
        w->owner = (*w->word2 & FUTEX_TID_MASK) == (uint32_t)w->tid;
        w->ret2 = fx(w->word2, FUTEX_UNLOCK_PI_PRIVATE, 0, NULL, NULL, 0);
        break;
    default: {
        struct fx_waitv v[3];
        for (int i = 0; i < 3; i++) v[i] = (struct fx_waitv){0, (uintptr_t)&w->word[i], 2 | 128, 0};
        long r = syscall(449, v, 3, 0, NULL, CLOCK_MONOTONIC);
        w->ret = r < 0 ? -errno : r;
        v[1].val = 1;
        __atomic_store_n(&w->stage, 2, __ATOMIC_RELEASE);
        r = syscall(449, v, 3, 0, NULL, CLOCK_MONOTONIC);
        w->ret2 = r < 0 ? -errno : r;
        v[0].val = 5;
        r = syscall(449, v, 3, 0, NULL, CLOCK_MONOTONIC);
        w->ret3 = r < 0 ? -errno : r;
    }
    }
    __atomic_store_n(&w->done, 1, __ATOMIC_RELEASE);
    return NULL;
}
static void fx_until_stage(struct fx_waiter *w, int stage) {
    uint64_t end = mono_ns(CLOCK_MONOTONIC) + 5000000000ULL;
    while (__atomic_load_n(&w->stage, __ATOMIC_ACQUIRE) < stage && mono_ns(CLOCK_MONOTONIC) < end) cfz_sleep_us(100);
}
static void fx_start(struct fx_waiter *w, long nr, uintptr_t a0) {
    w->stage = w->done = 0;
    pthread_create(&w->th, NULL, fx_waiter_fn, w);
    fx_until_stage(w, 1);
    cfz_settle(getpid(), w->tid, nr, a0);
}
/* Waits until `want` of the n waiters are done; returns the mask of done ones. */
static int fx_done(struct fx_waiter *w, int n, int want) {
    uint64_t end = mono_ns(CLOCK_MONOTONIC) + 5000000000ULL;
    for (;;) {
        int m = 0, c = 0;
        for (int i = 0; i < n; i++)
            if (__atomic_load_n(&w[i].done, __ATOMIC_ACQUIRE)) { m |= 1 << i; c++; }
        if (c >= want || mono_ns(CLOCK_MONOTONIC) > end) return m;
        cfz_sleep_us(100);
    }
}
static struct timespec fx_abs(clockid_t c, long ns, uint64_t *at) {
    *at = mono_ns(c) + (uint64_t)ns;
    struct timespec t = {(time_t)(*at / 1000000000ULL), (long)(*at % 1000000000ULL)};
    return t;
}
static void fx_round(char *b, size_t n) {
    int len = 0;
#define FXP(...) len += snprintf(b + len, n - (size_t)len, __VA_ARGS__)
    static volatile uint32_t a, c, p, m, x[3], y;
    struct fx_waiter w[4];
    /* WAKE_OP: wake one on a, set b to 1 and, since it was 0, wake one on b. */
    a = c = 0;
    memset(w, 0, sizeof w);
    w[0].word = &a;
    w[1].word = &c;
    fx_start(&w[0], SYS_futex, (uintptr_t)&a);
    fx_start(&w[1], SYS_futex, (uintptr_t)&c);
    long r1 = fx(&a, FUTEX_WAKE_OP_PRIVATE, 1, (void *)1, &c, FUTEX_OP(FUTEX_OP_SET, 1, FUTEX_OP_CMP_EQ, 0));
    uint32_t c1 = c;
    for (int i = 0; i < 2; i++) pthread_join(w[i].th, NULL);
    long r2 = fx(&a, FUTEX_WAKE_OP_PRIVATE, 1, (void *)1, &c, FUTEX_OP(FUTEX_OP_ADD, 2, FUTEX_OP_CMP_EQ, 0));
    FXP("wakeOp %ld word %u waiters %ld %ld again %ld word %u\n", r1, c1, w[0].ret, w[1].ret, r2, c);
    /* CMP_REQUEUE: four FIFO waiters on a; wake one, move two to c. */
    a = c = 0;
    memset(w, 0, sizeof w);
    for (int i = 0; i < 4; i++) { w[i].word = &a; fx_start(&w[i], SYS_futex, (uintptr_t)&a); }
    long q = fx(&a, FUTEX_CMP_REQUEUE_PRIVATE, 1, (void *)2, &c, 0);
    int m1 = fx_done(w, 4, 1);
    long qe = fx(&a, FUTEX_CMP_REQUEUE_PRIVATE, 1, (void *)2, &c, 9);
    long qb = fx(&c, FUTEX_WAKE_PRIVATE, 2, NULL, NULL, 0);
    int m2 = fx_done(w, 4, 3);
    long qa = fx(&a, FUTEX_WAKE_PRIVATE, 1, NULL, NULL, 0);
    for (int i = 0; i < 4; i++) pthread_join(w[i].th, NULL);
    FXP("cmpRequeue %ld woke %#x mismatch %ld wakeMoved %ld woke %#x wakeRest %ld rets %ld %ld %ld %ld\n", q, m1, qe, qb, m2, qa,
        w[0].ret, w[1].ret, w[2].ret, w[3].ret);
    /* WAIT_BITSET deadlines and selective wakes. */
    a = 0;
    uint64_t at;
    struct timespec dl = fx_abs(CLOCK_MONOTONIC, 2000000, &at);
    long bm = fx(&a, FUTEX_WAIT_BITSET_PRIVATE, 0, &dl, NULL, FUTEX_BITSET_MATCH_ANY);
    int bmok = mono_ns(CLOCK_MONOTONIC) >= at;
    dl = fx_abs(CLOCK_REALTIME, 2000000, &at);
    long br = fx(&a, FUTEX_WAIT_BITSET_PRIVATE | FUTEX_CLOCK_REALTIME, 0, &dl, NULL, FUTEX_BITSET_MATCH_ANY);
    int brok = mono_ns(CLOCK_REALTIME) >= at;
    memset(w, 0, sizeof w);
    for (int i = 0; i < 3; i++) {
        w[i].word = &a;
        w[i].op = 1;
        w[i].bits = 1u << i;
        fx_start(&w[i], SYS_futex, (uintptr_t)&a);
    }
    long b2 = fx(&a, FUTEX_WAKE_BITSET_PRIVATE, INT_MAX, NULL, NULL, 2);
    int bm1 = fx_done(w, 3, 1);
    long b5 = fx(&a, FUTEX_WAKE_BITSET_PRIVATE, INT_MAX, NULL, NULL, 5);
    for (int i = 0; i < 3; i++) pthread_join(w[i].th, NULL);
    long b0 = fx(&a, FUTEX_WAIT_BITSET_PRIVATE, 0, NULL, NULL, 0);
    FXP("bitset monoAbs %ld notEarly %d realtimeAbs %ld notEarly %d wake2 %ld woke %#x wake5 %ld rets %ld %ld %ld zero %ld\n", bm,
        bmok, br, brok, b2, bm1, b5, w[0].ret, w[1].ret, w[2].ret, b0);
    /* PI: lock, contend, hand over, unlock. */
    p = 0;
    long pl = fx(&p, FUTEX_LOCK_PI_PRIVATE, 0, NULL, NULL, 0);
    int powner = (p & FUTEX_TID_MASK) == (uint32_t)gettid();
    long pt = fx(&p, FUTEX_TRYLOCK_PI_PRIVATE, 0, NULL, NULL, 0);
    long pd = fx(&p, FUTEX_LOCK_PI_PRIVATE, 0, NULL, NULL, 0);
    memset(w, 0, sizeof w);
    w[0].word = &p;
    w[0].op = 2;
    fx_start(&w[0], SYS_futex, (uintptr_t)&p);
    int pwait = (p & FUTEX_WAITERS) != 0 && (p & FUTEX_TID_MASK) == (uint32_t)gettid();
    long pu = fx(&p, FUTEX_UNLOCK_PI_PRIVATE, 0, NULL, NULL, 0);
    pthread_join(w[0].th, NULL);
    long pe = fx(&p, FUTEX_UNLOCK_PI_PRIVATE, 0, NULL, NULL, 0);
    FXP("pi lock %ld owner %d trylock %ld relock %ld waitersBit %d unlock %ld handedOver %ld owner %d unlock %ld word %u notOwner %ld\n",
        pl, powner, pt, pd, pwait, pu, w[0].ret, w[0].owner, w[0].ret2, p, pe);
    /* Requeue-PI: a condition waiter moved to a held PI mutex, then handed the mutex. */
    c = m = 0;
    long ml = fx(&m, FUTEX_LOCK_PI_PRIVATE, 0, NULL, NULL, 0);
    memset(w, 0, sizeof w);
    w[0].word = &c;
    w[0].word2 = &m;
    w[0].op = 3;
    fx_start(&w[0], SYS_futex, (uintptr_t)&c);
    long rq = fx(&c, FUTEX_CMP_REQUEUE_PI_PRIVATE, 1, (void *)1, &m, 0);
    int mwait = (m & FUTEX_WAITERS) != 0;
    long mu = fx(&m, FUTEX_UNLOCK_PI_PRIVATE, 0, NULL, NULL, 0);
    pthread_join(w[0].th, NULL);
    FXP("requeuePi lock %ld requeued %ld waitersBit %d unlock %ld waiter %ld owner %d unlock %ld word %u\n", ml, rq, mwait, mu,
        w[0].ret, w[0].owner, w[0].ret2, m);
    /* futex_waitv: index of the woken word, reuse with an updated value, stale value. */
    x[0] = x[1] = x[2] = y = 0;
    memset(w, 0, sizeof w);
    w[0].word = x;
    w[0].op = 4;
    fx_start(&w[0], 449, 0);
    long wy = fx(&y, FUTEX_WAKE_PRIVATE, 1, NULL, NULL, 0);
    x[1] = 1;
    long w1 = fx(&x[1], FUTEX_WAKE_PRIVATE, 1, NULL, NULL, 0);
    fx_until_stage(&w[0], 2);
    cfz_settle(getpid(), w[0].tid, 449, 0);
    x[2] = 1;
    long w2 = fx(&x[2], FUTEX_WAKE_PRIVATE, 1, NULL, NULL, 0);
    pthread_join(w[0].th, NULL);
    FXP("waitv unselected %ld wake1 %ld index %ld wake2 %ld index %ld stale %ld\n", wy, w1, w[0].ret, w2, w[0].ret2, w[0].ret3);
    /* errno */
    a = 1;
    FXP("errno mismatch %ld unaligned %ld null %ld badOp %ld badWakeOp %ld\n", fx(&a, FUTEX_WAIT_PRIVATE, 0, NULL, NULL, 0),
        fx((volatile uint32_t *)((volatile char *)&a + 1), FUTEX_WAIT_PRIVATE, 1, NULL, NULL, 0),
        fx(NULL, FUTEX_WAIT_PRIVATE, 0, NULL, NULL, 0), fx(&a, 99, 0, NULL, NULL, 0),
        fx(&a, FUTEX_WAKE_OP_PRIVATE, 1, (void *)1, &c, FUTEX_OP(7, 0, 0, 0)));
#undef FXP
}
static int t_futexops(void) {
    long rounds = 10 * scale, same = 0;
    char first[4096], cur[4096];
    for (long r = 0; r < rounds; r++) {
        fx_round(r ? cur : first, sizeof first);
        if (r) same += !strcmp(first, cur);
    }
    fputs(first, stdout);
    OUT("rounds", "%ld/%ld", same, rounds - 1);
    return 1;
}

/* det: 100 pipes and 100 socket pairs registered EPOLLET with generation tags. Per
 * round, in seeded orders: (1) one event per written descriptor, none on a second
 * wait, events after refilling a partially drained one and after refilling a fully
 * drained one; (2) EPOLLONESHOT: one event, none after another write, one after the
 * EPOLL_CTL_MOD rearm; (3) closed and recreated descriptors reuse their numbers and
 * report only the new tag, and none before they are registered again. */
#define EE_N 200
static int ee_r[EE_N], ee_w[EE_N];
static uint32_t ee_gen[EE_N];
static int ee_new(int i) {
    int f[2];
    if (i < 100 ? pipe(f) : socketpair(AF_UNIX, SOCK_STREAM, 0, f)) return 0;
    ee_r[i] = f[0];
    ee_w[i] = f[1];
    fcntl(f[0], F_SETFL, O_NONBLOCK);
    return 1;
}
static int ee_add(int ep, int i, int op, uint32_t extra) {
    struct epoll_event e = {.events = EPOLLIN | EPOLLET | extra, .data.u64 = ((uint64_t)ee_gen[i] << 32) | (uint32_t)i};
    return epoll_ctl(ep, op, ee_r[i], &e) == 0;
}
static void ee_drain(int i) {
    char b[64];
    while (read(ee_r[i], b, sizeof b) > 0) {}
}
/* Collects ready events: count, events for the listed descriptors, stale tags and an
 * order hash. */
struct ee_tally { long events, stale, other; uint64_t hash; };
static void ee_wait(int ep, struct ee_tally *t, int *cnt) {
    struct epoll_event ev[256];
    int n = epoll_wait(ep, ev, 256, 0);
    for (int k = 0; k < n; k++) {
        uint32_t i = (uint32_t)ev[k].data.u64, g = (uint32_t)(ev[k].data.u64 >> 32);
        t->hash = t->hash * 1000003ULL + ev[k].data.u64 * 31 + ev[k].events;
        t->events++;
        if (i >= EE_N) { t->other++; continue; }
        if (g != ee_gen[i]) t->stale++;
        if (cnt) cnt[i]++;
    }
}
static int t_epolledge(void) {
    int ep = epoll_create1(EPOLL_CLOEXEC);
    for (int i = 0; i < EE_N; i++) {
        if (!ee_new(i)) return 0;
        ee_gen[i] = 1;
        if (!ee_add(ep, i, EPOLL_CTL_ADD, 0)) return 0;
    }
    long rounds = 10 * scale;
    struct ee_tally T[8];
    memset(T, 0, sizeof T);
    long once[8] = {0}, expect[8] = {0}, reused = 0, recreated = 0;
    int perm[EE_N], cnt[EE_N];
    for (long r = 0; r < rounds; r++) {
        for (int i = 0; i < EE_N; i++) perm[i] = i;
        for (int i = EE_N - 1; i > 0; i--) { int k = (int)(next() % (uint64_t)(i + 1)), t = perm[i]; perm[i] = perm[k]; perm[k] = t; }
        /* (1) edges */
        memset(cnt, 0, sizeof cnt);
        for (int k = 0; k < 100; k++) if (write(ee_w[perm[k]], "ab", 2) != 2) return 0;
        ee_wait(ep, &T[0], cnt);
        for (int k = 0; k < 100; k++) once[0] += cnt[perm[k]] == 1;
        expect[0] += 100;
        ee_wait(ep, &T[1], NULL);
        memset(cnt, 0, sizeof cnt);
        for (int k = 0; k < 100; k++) {
            char c;
            if (read(ee_r[perm[k]], &c, 1) != 1 || write(ee_w[perm[k]], "c", 1) != 1) return 0;
        }
        ee_wait(ep, &T[2], cnt);
        for (int k = 0; k < 100; k++) once[2] += cnt[perm[k]] == 1;
        memset(cnt, 0, sizeof cnt);
        for (int k = 0; k < 100; k++) { ee_drain(perm[k]); if (write(ee_w[perm[k]], "d", 1) != 1) return 0; }
        ee_wait(ep, &T[3], cnt);
        for (int k = 0; k < 100; k++) once[3] += cnt[perm[k]] == 1;
        for (int k = 0; k < 100; k++) ee_drain(perm[k]);
        ee_wait(ep, &T[7], NULL);
        /* (2) one-shot */
        for (int k = 100; k < 150; k++) ee_add(ep, perm[k], EPOLL_CTL_MOD, EPOLLONESHOT);
        memset(cnt, 0, sizeof cnt);
        for (int k = 100; k < 150; k++) if (write(ee_w[perm[k]], "e", 1) != 1) return 0;
        ee_wait(ep, &T[4], cnt);
        for (int k = 100; k < 150; k++) { once[4] += cnt[perm[k]] == 1; if (write(ee_w[perm[k]], "f", 1) != 1) return 0; }
        ee_wait(ep, &T[5], NULL);
        memset(cnt, 0, sizeof cnt);
        for (int k = 100; k < 150; k++) ee_add(ep, perm[k], EPOLL_CTL_MOD, EPOLLONESHOT);
        ee_wait(ep, &T[6], cnt);
        for (int k = 100; k < 150; k++) once[6] += cnt[perm[k]] == 1;
        ee_wait(ep, &T[5], NULL);
        for (int k = 100; k < 150; k++) { ee_drain(perm[k]); ee_add(ep, perm[k], EPOLL_CTL_MOD, 0); }
        ee_wait(ep, &T[7], NULL);
        /* (3) close, reuse the numbers, register with a new tag */
        for (int k = 150; k < 170; k++) {
            int i = perm[k], orr = ee_r[i], ow = ee_w[i];
            close(ee_r[i]);
            close(ee_w[i]);
            if (!ee_new(i)) return 0;
            reused += ee_r[i] == orr && ee_w[i] == ow;
            if (write(ee_w[i], "g", 1) != 1) return 0;
        }
        ee_wait(ep, &T[7], NULL);
        memset(cnt, 0, sizeof cnt);
        for (int k = 150; k < 170; k++) {
            int i = perm[k];
            ee_gen[i]++;
            recreated += ee_add(ep, i, EPOLL_CTL_ADD, 0);
            if (write(ee_w[i], "h", 1) != 1) return 0;
        }
        ee_wait(ep, &T[3], cnt);
        for (int k = 150; k < 170; k++) { once[5] += cnt[perm[k]] == 1; ee_drain(perm[k]); }
        expect[5] += 20;
        ee_wait(ep, &T[7], NULL);
    }
    for (int i = 0; i < EE_N; i++) { close(ee_r[i]); close(ee_w[i]); }
    close(ep);
    long st = 0;
    for (int k = 0; k < 8; k++) st += T[k].stale + T[k].other;
    OUT("write", "once %ld/%ld events %ld secondWait %ld", once[0], expect[0], T[0].events, T[1].events);
    OUT("refillPartial", "once %ld events %ld", once[2], T[2].events);
    OUT("refillDrained", "once %ld", once[3]);
    OUT("oneshot", "once %ld/%ld suppressed %ld rearmed %ld/%ld", once[4], rounds * 50, T[5].events, once[6], rounds * 50);
    OUT("reuse", "numbers %ld/%ld strayEvents %ld registered %ld newTagOnce %ld/%ld", reused, rounds * 20, T[7].events,
        recreated, once[5], expect[5]);
    OUT("staleOrForeign", "%ld", st);
    uint64_t h = 0;
    for (int k = 0; k < 8; k++) h = h * 31 + T[k].hash;
    OUT("orderHash", "%016lx", (unsigned long)h);
    return 1;
}

/* det: a SECCOMP_RET_USER_NOTIF filter for openat with a sentinel mode argument; a
 * supervisor thread checks each notification (pid, nr, arch, args, path under the
 * test directory) and answers in three phases: CONTINUE (the real open runs), an
 * injected EACCES, and a descriptor installed with SECCOMP_IOCTL_NOTIF_ADDFD.
 * 100 opens from each of 4 threads per phase. */
#define SN_MODE 0x5eed
#define SN_THREADS 4
#define SN_ITERS 100
static char sn_dir[64];
static int sn_listener;
static volatile int sn_phase, sn_go, sn_quit;
static volatile pid_t sn_tids[SN_THREADS];
static struct { long fdok, content, eacces, other; } sn_res[3][SN_THREADS];
static volatile long sn_notes;
static long sn_pidok, sn_nrok, sn_archok, sn_argsok, sn_pathok, sn_replyok;
static void sn_path(char *b, size_t n, int k) { snprintf(b, n, "%s/f%d", sn_dir, k); }
static void *sn_supervisor(void *p) {
    (void)p;
    long total = 3L * SN_THREADS * SN_ITERS * scale;
    size_t dl = strlen(sn_dir);
    uint64_t end = mono_ns(CLOCK_MONOTONIC) + 60000000000ULL;
    while (sn_notes < total && !sn_quit && mono_ns(CLOCK_MONOTONIC) < end) {
        struct pollfd pf = {sn_listener, POLLIN, 0};
        if (poll(&pf, 1, 100) <= 0) continue;
        struct seccomp_notif n;
        memset(&n, 0, sizeof n);
        if (ioctl(sn_listener, SECCOMP_IOCTL_NOTIF_RECV, &n)) continue;
        __atomic_add_fetch(&sn_notes, 1, __ATOMIC_RELEASE);
        int k = -1;
        for (int i = 0; i < SN_THREADS; i++)
            if (sn_tids[i] == (pid_t)n.pid) k = i;
        sn_pidok += k >= 0;
        sn_nrok += n.data.nr == SYS_openat;
        sn_archok += n.data.arch == AUDIT_ARCH_AARCH64;
        sn_argsok += (int)n.data.args[0] == AT_FDCWD && (n.data.args[3] & 0xffff) == SN_MODE &&
                     (n.data.args[2] & O_ACCMODE) == O_RDONLY;
        const char *path = (const char *)(uintptr_t)n.data.args[1];
        sn_pathok += !strncmp(path, sn_dir, dl) && path[dl] == '/';
        struct seccomp_notif_resp r;
        memset(&r, 0, sizeof r);
        r.id = n.id;
        int ok;
        if (sn_phase == 0) {
            r.flags = SECCOMP_USER_NOTIF_FLAG_CONTINUE;
            ok = !ioctl(sn_listener, SECCOMP_IOCTL_NOTIF_SEND, &r);
        } else if (sn_phase == 1) {
            r.error = -EACCES;
            ok = !ioctl(sn_listener, SECCOMP_IOCTL_NOTIF_SEND, &r);
        } else {
            int mfd = memfd_create("cfzsn", MFD_CLOEXEC);
            char text[32];
            int tl = snprintf(text, sizeof text, "addfd-%d", k);
            ok = mfd >= 0 && write(mfd, text, (size_t)tl) == tl;
            struct seccomp_notif_addfd af;
            memset(&af, 0, sizeof af);
            af.id = n.id;
            af.flags = SECCOMP_ADDFD_FLAG_SEND;
            af.srcfd = (uint32_t)mfd;
            af.newfd_flags = O_CLOEXEC;
            int nfd = ok ? ioctl(sn_listener, SECCOMP_IOCTL_NOTIF_ADDFD, &af) : -1;
            if (nfd < 0 && ok) { /* kernels before 5.14: install, then reply with the number */
                af.flags = 0;
                nfd = ioctl(sn_listener, SECCOMP_IOCTL_NOTIF_ADDFD, &af);
                r.val = nfd;
                ok = nfd >= 0 && !ioctl(sn_listener, SECCOMP_IOCTL_NOTIF_SEND, &r);
            }
            ok &= nfd >= 0;
            if (mfd >= 0) close(mfd);
        }
        sn_replyok += ok;
    }
    return NULL;
}
static void *sn_caller(void *p) {
    long k = (long)p;
    sn_tids[k] = gettid();
    char path[96], want[32], got[32];
    sn_path(path, sizeof path, (int)k);
    for (int ph = 0; ph < 3; ph++) {
        while (__atomic_load_n(&sn_go, __ATOMIC_ACQUIRE) <= ph) cfz_wait(&sn_go, ph, 100);
        int wl = ph == 2 ? snprintf(want, sizeof want, "addfd-%ld", k) : snprintf(want, sizeof want, "real-%ld", k);
        for (long i = 0; i < SN_ITERS * scale; i++) {
            long fd = syscall(SYS_openat, AT_FDCWD, path, O_RDONLY | O_CLOEXEC, SN_MODE);
            int e = errno;
            if (fd >= 0) {
                sn_res[ph][k].fdok++;
                ssize_t n = pread((int)fd, got, sizeof got, 0);
                sn_res[ph][k].content += n == wl && !memcmp(got, want, (size_t)wl);
                close((int)fd);
            } else if (e == EACCES) sn_res[ph][k].eacces++;
            else sn_res[ph][k].other++;
        }
    }
    return NULL;
}
static int t_seccompnotify(void) {
    snprintf(sn_dir, sizeof sn_dir, "/tmp/cfzsn.XXXXXX");
    if (!mkdtemp(sn_dir)) { puts("UNSUPPORTED tmpdir"); return 0; }
    for (int k = 0; k < SN_THREADS; k++) {
        char path[96], text[32];
        sn_path(path, sizeof path, k);
        int fd = open(path, O_WRONLY | O_CREAT | O_TRUNC | O_CLOEXEC, 0600);
        int tl = snprintf(text, sizeof text, "real-%d", k);
        if (fd < 0 || write(fd, text, (size_t)tl) != tl) return 0;
        close(fd);
    }
    pid_t c = fork();
    if (!c) {
        struct sock_filter f[] = {
            BPF_STMT(BPF_LD | BPF_W | BPF_ABS, offsetof(struct seccomp_data, arch)),
            BPF_JUMP(BPF_JMP | BPF_JEQ | BPF_K, AUDIT_ARCH_AARCH64, 0, 5),
            BPF_STMT(BPF_LD | BPF_W | BPF_ABS, offsetof(struct seccomp_data, nr)),
            BPF_JUMP(BPF_JMP | BPF_JEQ | BPF_K, SYS_openat, 0, 3),
            BPF_STMT(BPF_LD | BPF_W | BPF_ABS, offsetof(struct seccomp_data, args[3])),
            BPF_JUMP(BPF_JMP | BPF_JEQ | BPF_K, SN_MODE, 0, 1),
            BPF_STMT(BPF_RET | BPF_K, SECCOMP_RET_USER_NOTIF),
            BPF_STMT(BPF_RET | BPF_K, SECCOMP_RET_ALLOW),
        };
        struct sock_fprog prog = {sizeof f / sizeof f[0], f};
        if (prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0)) _exit(99);
        sn_listener = (int)syscall(SYS_seccomp, SECCOMP_SET_MODE_FILTER, SECCOMP_FILTER_FLAG_NEW_LISTENER, &prog);
        if (sn_listener < 0) _exit(99);
        pthread_t sup, t[SN_THREADS];
        pthread_create(&sup, NULL, sn_supervisor, NULL);
        for (long k = 0; k < SN_THREADS; k++) pthread_create(&t[k], NULL, sn_caller, (void *)k);
        for (int k = 0; k < SN_THREADS; k++)
            while (!sn_tids[k]) cfz_sleep_us(100);
        /* Opens without the sentinel mode are not intercepted. */
        char path[96], got[16];
        sn_path(path, sizeof path, 0);
        int plain = open(path, O_RDONLY | O_CLOEXEC);
        int plainok = plain >= 0 && pread(plain, got, sizeof got, 0) == 6 && !memcmp(got, "real-0", 6);
        if (plain >= 0) close(plain);
        uint64_t end = mono_ns(CLOCK_MONOTONIC) + 60000000000ULL;
        for (int ph = 0; ph < 3; ph++) {
            sn_phase = ph;
            __atomic_store_n(&sn_go, ph + 1, __ATOMIC_RELEASE);
            cfz_wake(&sn_go);
            for (;;) {
                long done = 0;
                for (int k = 0; k < SN_THREADS; k++)
                    done += sn_res[ph][k].fdok + sn_res[ph][k].eacces + sn_res[ph][k].other;
                if (done >= SN_THREADS * SN_ITERS * scale || mono_ns(CLOCK_MONOTONIC) > end) break;
                cfz_sleep_us(500);
            }
        }
        for (int k = 0; k < SN_THREADS; k++) pthread_join(t[k], NULL);
        sn_quit = 1;
        pthread_join(sup, NULL);
        const char *names[3] = {"continue", "eacces", "addfd"};
        long per = SN_ITERS * scale;
        for (int ph = 0; ph < 3; ph++) {
            long fdok = 0, content = 0, eacces = 0, other = 0;
            for (int k = 0; k < SN_THREADS; k++) {
                fdok += sn_res[ph][k].fdok;
                content += sn_res[ph][k].content;
                eacces += sn_res[ph][k].eacces;
                other += sn_res[ph][k].other;
            }
            OUT(names[ph], "opened %ld content %ld eacces %ld other %ld of %ld", fdok, content, eacces, other, per * SN_THREADS);
        }
        OUT("notifications", "%ld pid %ld nr %ld arch %ld args %ld path %ld replied %ld", sn_notes, sn_pidok, sn_nrok, sn_archok,
            sn_argsok, sn_pathok, sn_replyok);
        OUT("plainOpen", "%d", plainok);
        _exit(0);
    }
    int st = 0;
    waitpid(c, &st, 0);
    for (int k = 0; k < SN_THREADS; k++) {
        char path[96];
        sn_path(path, sizeof path, k);
        unlink(path);
    }
    rmdir(sn_dir);
    if (WIFEXITED(st) && WEXITSTATUS(st) == 99) { puts("UNSUPPORTED seccomp_user_notif"); return 0; }
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
    {"nestsig", t_nestsig}, {"execsig", t_execsig}, {"execsigrace", t_execsigrace},
    {"autodisarmctx", t_autodisarmctx}, {"sigio", t_sigio}, {"timerstorm", t_timerstorm},
    {"timerstormcounts", t_timerstormcounts}, {"chargelin", t_chargelin}, {"chargelinvalues", t_chargelinvalues},
    {"unaligned", t_unaligned}, {"exclmaint", t_exclmaint}, {"exclmaintcounts", t_exclmaintcounts},
    {"simdfpctl", t_simdfpctl}, {"rseqcommit", t_rseqcommit}, {"futexops", t_futexops}, {"epolledge", t_epolledge},
    {"seccompnotify", t_seccompnotify},
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
    if (!strcmp(argv[1], "execsighelper")) return es_helper(argc, argv);
    for (size_t i = 0; i < sizeof TESTS / sizeof TESTS[0]; i++) {
        if (strcmp(argv[1], TESTS[i].name)) continue;
        if (TESTS[i].fn()) printf("OK %s\n", argv[1]);
        return 0;
    }
    fprintf(stderr, "unknown test %s\n", argv[1]);
    return 2;
}
