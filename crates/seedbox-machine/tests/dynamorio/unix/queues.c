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
static void cfz_wait(volatile int *w, int v, long ms) { (void)ms; syscall(SYS_futex, w, FUTEX_WAIT_PRIVATE, v, NULL, NULL, 0); }
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
    unix_emit(slot, v);
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
        sched_yield();
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

static void fixed_work(long n) { volatile unsigned long x=1; for(long i=0;i<n;i++)x=x*1664525+1013904223; }
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
    while (sp_total < accepted && mono_ns(CLOCK_MONOTONIC) < end) sched_yield();

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
    while (sp_logn < want && mono_ns(CLOCK_MONOTONIC) < end) sched_yield();

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
static uint64_t kpending(void) { uint64_t m=0; syscall(SYS_rt_sigpending,&m,8); return m; }
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


int main(int argc,char **argv) { setvbuf(stdout,NULL,_IOLBF,0); if(argc!=2)return 2;
if(!strcmp(argv[1],"rtfifo"))return !t_rtfifo();
if(!strcmp(argv[1],"sigpressure"))return !t_sigpressure();
if(!strcmp(argv[1],"tmpmask"))return !t_tmpmask();
return 2; }
