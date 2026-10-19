/* Extracted source scenarios: Seedbox PR317 f32472b7fe470367b7eddecde1ac53ad299f3a3e,
 * tools/jvm-fuzz/probes/c/cfz.c. Original populations and scenario state retained.
 * No upstream harness, DR internal headers, or DR private layouts. */
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
#include "support.h"
#include <sys/time.h>
static long scale = 1;
static uint64_t rng = 0x9E3779B97F4A7C15ULL | 1;
static uint64_t next(void) { rng ^= rng << 13; rng ^= rng >> 7; rng ^= rng << 17; return rng; }
#define OUT(k, fmt, ...) printf("%s " fmt "\n", k, __VA_ARGS__)
static uint64_t mono_ns(clockid_t c) { struct timespec t; clock_gettime(c, &t); return (uint64_t)t.tv_sec * 1000000000ULL + t.tv_nsec; }
static void cfz_wake(volatile int *w) { syscall(SYS_futex, w, FUTEX_WAKE_PRIVATE, INT_MAX, NULL, NULL, 0); }
/* Source readiness helper used timeout polling without a wake on every store.
 * Poll only the positive shared-state predicate; syscall subjects live in waits.c. */
static void cfz_wait(volatile int *w, int v, long ms) { (void)w; (void)v; (void)ms; sched_yield(); }
static int cfz_wait_for(volatile int *w, int want, long ms) { (void)ms; int v; while ((v=__atomic_load_n(w,__ATOMIC_ACQUIRE))<want) cfz_wait(w,v,0); return 1; }
static uint64_t kmask_get(void) { uint64_t m=0; syscall(SYS_rt_sigprocmask,SIG_BLOCK,NULL,&m,8); return m; }
#define SIGBIT(s) (1ULL << ((s)-1))
#define CFZ_FN(n) ".globl " n "\n.hidden " n "\n.type " n ", %function\n" n ":\n"
#define CFZ_AT(n) ".globl " n "\n.hidden " n "\n" n ":\n"
#define CFZ_HIDDEN __attribute__((visibility("hidden")))
CFZ_HIDDEN long cfz_rawsys(long,long,long,long,long,long,long);
CFZ_HIDDEN extern const char cfz_svc_after[];
#if defined(__aarch64__)
asm(".text\n.p2align 2\n" CFZ_FN("cfz_rawsys") "mov x8,x6\nsvc #0\n" CFZ_AT("cfz_svc_after") "ret\n");
#else
asm(".text\n.p2align 2\n.globl cfz_rawsys\n.hidden cfz_rawsys\n.type cfz_rawsys,@function\ncfz_rawsys:\nmov 8(%rsp),%rax\nmov %rcx,%r10\nsyscall\n.globl cfz_svc_after\n.hidden cfz_svc_after\ncfz_svc_after:\nret\n");
#endif
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
        register unsigned long x
#if defined(__aarch64__)
        asm("x28")
#else
        asm("r12")
#endif
        = 0x12345678 + i;
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
#if defined(__aarch64__)
    uc->uc_mcontext.regs[0] = 1234;
#else
    uc->uc_mcontext.gregs[REG_RAX] = 1234;
#endif /* emulated getppid result */
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

static int t_pidfd(void) {
    long ok = 0;
    for (long i = 0; i < 20 * scale; i++) {
        int ready[2];
        if (pipe(ready)) { perror("pidfd readiness pipe"); return 2; }
        pid_t p = fork();
        if (!p) {
            close(ready[0]);
            char byte = 'R';
            if (write(ready[1], &byte, 1) != 1) _exit(2);
            close(ready[1]);
            pause();
            _exit(0);
        }
        close(ready[1]);
        if (p < 0) { close(ready[0]); perror("pidfd fork"); return 2; }
        struct pollfd child_ready = {ready[0], POLLIN, 0};
        int waited;
        do { waited = poll(&child_ready, 1, 30000); } while (waited < 0 && errno == EINTR);
        char byte = 0;
        ssize_t received = -1;
        if (waited > 0) {
            do { received = read(ready[0], &byte, 1); } while (received < 0 && errno == EINTR);
        }
        close(ready[0]);
        if (waited <= 0 || received != 1 || byte != 'R') {
            if (waited < 0) perror("pidfd child readiness poll");
            else if (waited == 0) fputs("pidfd child readiness timed out\n", stderr);
            kill(p, SIGKILL);
            waitpid(p, 0, 0);
            return 2;
        }
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
           (unsigned)uc->uc_stack.ss_flags == AC_AUTODISARM &&
#if defined(__aarch64__)
           uc->uc_mcontext.pc == (uintptr_t)cfz_svc_after
#else
           uc->uc_mcontext.gregs[REG_RIP] == (uintptr_t)cfz_svc_after
#endif
;
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
#if defined(__x86_64__)
        /* x86 CALL stores its return PC on the user stack. Keep later calls
         * below the suspended raw-syscall return slot (A64 retains LR in frame). */
        volatile char *below_suspended_call = __builtin_alloca(512);
        below_suspended_call[0] = 1;
        asm volatile("" : : "r"(below_suspended_call) : "memory");
#endif
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


static uint64_t kpending(void) { uint64_t m=0; syscall(SYS_rt_sigpending,&m,8); return m; }
static int cfz_queue(pid_t tid,int sig,int value) { siginfo_t si={0};si.si_signo=sig;si.si_code=SI_QUEUE;si.si_pid=getpid();si.si_uid=getuid();si.si_value.sival_int=value;return syscall(SYS_rt_tgsigqueueinfo,getpid(),tid,sig,&si); }
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
        unix_wait_syscall(c, c, SYS_rt_sigtimedwait);
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
    while (!ts_blocked) sched_yield();
    sigset_t only;
    sigemptyset(&only);
    sigaddset(&only, TS_SIG);
    pthread_sigmask(SIG_UNBLOCK, &only, NULL);
    unix_wait_syscall(getpid(),ts_blocked,SYS_read);
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
            while (!access(task,F_OK)) sched_yield();
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
    /* Positive second CLOCK_MONOTONIC timer expiration allows the blocked
     * periodic timer to accumulate notifications. This checks the overrun API,
     * never elapsed host duration or successful sleep/deadline accuracy. */
    int tickfd=timerfd_create(CLOCK_MONOTONIC,TFD_CLOEXEC);
    struct itimerspec accumulate={{0,0},{0,50000000}};
    uint64_t notifications=0;
    if(tickfd<0 || timerfd_settime(tickfd,0,&accumulate,NULL) || read(tickfd,&notifications,sizeof notifications)!=sizeof notifications || notifications!=1)return 0;
    close(tickfd);
    pthread_sigmask(SIG_UNBLOCK, &per, NULL);
    while(!ts_per_n)sched_yield();
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
        if (mask >> k & 1) unix_wait_syscall(getpid(),sio_w[k].tid,SYS_rt_sigtimedwait);
    if (write(wfd, "x", 1) != 1) { snprintf(b, n, "writeFailed"); return; }
    /* Positive receipt for asynchronous ownership. In the disabled case,
     * send the higher-numbered STOP after the acknowledged write: a queued
     * SIO_SIG is lower-numbered and must win; any later stray remains pending
     * and is exposed when the final mask is restored. No timed absence. */
    if(want>=0) for(;;) {
        int any=0;
        for(int k=0;k<3;k++)if((mask>>k&1) && __atomic_load_n(&sio_w[k].done,__ATOMIC_ACQUIRE)!=before[k])any=1;
        if(any)break;
        sched_yield();
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


static const char *argv0;
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

/* A static-PIE helper sets ADDR_NO_RANDOMIZE and re-executes itself through
 * /proc/self/exe; each pass checks its own relocation and personality. */
static int staticpie(int show_address) {
    char path[4096];
    const char *slash = strrchr(argv0, '/');
    if (slash) snprintf(path, sizeof path, "%.*s/unix-staticpie", (int)(slash - argv0), argv0);
    else snprintf(path, sizeof path, "unix-staticpie");
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

int main(int argc,char **argv) {
argv0=argv[0]; setvbuf(stdout,NULL,_IOLBF,0); if(argc>1 && !strcmp(argv[1],"child"))return 42; if(argc>1 && !strcmp(argv[1],"execsighelper"))return es_helper(argc,argv); if(argc!=2)return 2;
if(!strcmp(argv[1],"execsig"))return !t_execsig();
if(!strcmp(argv[1],"execsigrace"))return !t_execsigrace();
if(!strcmp(argv[1],"timerstorm"))return !t_timerstorm();
if(!strcmp(argv[1],"timerstormcounts"))return !t_timerstormcounts();
if(!strcmp(argv[1],"sigio"))return !t_sigio();
if(!strcmp(argv[1],"pidns"))return !t_pidns();
if(!strcmp(argv[1],"staticpie"))return !t_staticpie();
if(!strcmp(argv[1],"staticpieaddr"))return !t_staticpieaddr();
if(!strcmp(argv[1],"ucontext"))return !t_ucontext();
if(!strcmp(argv[1],"siglongjmp"))return !t_siglongjmp();
if(!strcmp(argv[1],"pthreadcancel"))return !t_pthreadcancel();
if(!strcmp(argv[1],"seccomp"))return !t_seccomp();
if(!strcmp(argv[1],"autodisarm"))return !t_autodisarm();
if(!strcmp(argv[1],"signalfd"))return !t_signalfd();
if(!strcmp(argv[1],"pselect"))return !t_pselect();
if(!strcmp(argv[1],"sigsuspend"))return !t_sigsuspend();
if(!strcmp(argv[1],"cputimer"))return !t_cputimer();
if(!strcmp(argv[1],"pidfd"))return !t_pidfd();
if(!strcmp(argv[1],"clone3"))return !t_clone3();
if(!strcmp(argv[1],"memfdexec"))return !t_memfdexec();
if(!strcmp(argv[1],"timerfd"))return !t_timerfd();
if(!strcmp(argv[1],"wipeonfork"))return !t_wipeonfork();
if(!strcmp(argv[1],"unwind"))return !t_unwind();
if(!strcmp(argv[1],"tlsdtor"))return !t_tlsdtor();
if(!strcmp(argv[1],"autodisarmctx"))return !t_autodisarmctx();
return 2; }
