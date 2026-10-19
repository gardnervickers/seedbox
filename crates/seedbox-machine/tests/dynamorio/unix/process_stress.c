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
/* ==== 33. FailedClone3Rollback ==== */

struct c3args { uint64_t flags, pidfd, child_tid, parent_tid, exit_signal, stack, stack_size, tls, set_tid, set_tid_size, cgroup; };
static volatile sig_atomic_t c3_usr1, c3_hup;
static void c3_h(int s) { if (s == SIGUSR1) c3_usr1++; }
static void c3_hinfo(int s, siginfo_t *si, void *uc) { (void)si; (void)uc; if (s == SIGHUP) c3_hup++; }
static int c3_child(int clear, int die) {
    struct sigaction a;
    int code = 0;
    sigaction(SIGUSR1, NULL, &a);
    code |= (a.sa_handler == (clear ? SIG_DFL : c3_h)) << 0;
    sigaction(SIGUSR2, NULL, &a);
    code |= (a.sa_handler == SIG_IGN) << 1;
    sigaction(SIGHUP, NULL, &a);
    code |= (clear ? a.sa_handler == SIG_DFL : a.sa_sigaction == c3_hinfo) << 2;
    long me = syscall(SYS_getpid);
    syscall(SYS_kill, me, SIGUSR2);
    if (!clear) {
        int u = c3_usr1, h = c3_hup;
        syscall(SYS_kill, me, SIGUSR1);
        syscall(SYS_kill, me, SIGHUP);
        code |= (c3_usr1 == u + 1 && c3_hup == h + 1) << 3;
    } else {
        code |= 1 << 3;
    }
    if (die) syscall(SYS_kill, me, SIGUSR1);
    return code;
}
static int c3_parent_ok(void) {
    struct sigaction a;
    int ok = 1;
    sigaction(SIGUSR1, NULL, &a);
    ok &= a.sa_handler == c3_h;
    sigaction(SIGUSR2, NULL, &a);
    ok &= a.sa_handler == SIG_IGN;
    sigaction(SIGHUP, NULL, &a);
    ok &= a.sa_sigaction == c3_hinfo;
    int u = c3_usr1, h = c3_hup;
    raise(SIGUSR1);
    raise(SIGUSR2);
    raise(SIGHUP);
    ok &= c3_usr1 == u + 1 && c3_hup == h + 1;
    return ok;
}
static void *c3_thread(void *arg) { (void)arg; return (void *)(long)c3_parent_ok(); }
static int t_clone3(void) {
    struct sigaction a = {0};
    a.sa_handler = c3_h;
    a.sa_flags = SA_RESTART;
    sigaction(SIGUSR1, &a, NULL);
    a.sa_handler = SIG_IGN;
    sigaction(SIGUSR2, &a, NULL);
    a.sa_sigaction = c3_hinfo;
    a.sa_flags = SA_SIGINFO;
    sigaction(SIGHUP, &a, NULL);
    struct c3args probe = {.flags = CLONE_CLEAR_SIGHAND, .exit_signal = 200};
    if (syscall(SYS_clone3, &probe, sizeof probe) < 0 && errno == ENOSYS) { puts("UNSUPPORTED clone3"); return 0; }
    char *stack = mmap(NULL, 1 << 16, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    uint64_t stk = (uint64_t)(uintptr_t)stack, stksz = 1 << 16;
    pid_t settid = getpid();
    const uint64_t VMT = CLONE_VM | CLONE_SIGHAND | CLONE_THREAD | CLONE_SETTLS | CLONE_PARENT_SETTID | CLONE_CHILD_CLEARTID;
    int ptid = 0;
    struct { const char *name; struct c3args a; size_t size; int badptr; } inv[] = {
        {"vm_sighand_clear", {.flags = CLONE_VM | CLONE_SIGHAND | CLONE_CLEAR_SIGHAND, .exit_signal = SIGCHLD, .stack = stk, .stack_size = stksz}, 0, 0},
        {"thread_clear", {.flags = VMT | CLONE_CLEAR_SIGHAND, .stack = stk, .stack_size = stksz, .parent_tid = (uintptr_t)&ptid, .child_tid = (uintptr_t)&ptid}, 0, 0},
        {"thread_exitsig", {.flags = VMT, .exit_signal = SIGCHLD, .stack = stk, .stack_size = stksz, .parent_tid = (uintptr_t)&ptid, .child_tid = (uintptr_t)&ptid}, 0, 0},
        {"thread_nosighand", {.flags = CLONE_VM | CLONE_THREAD, .stack = stk, .stack_size = stksz}, 0, 0},
        {"vfork_vm_clear_badsig", {.flags = CLONE_VM | CLONE_VFORK | CLONE_CLEAR_SIGHAND, .exit_signal = 200, .stack = stk, .stack_size = stksz}, 0, 0},
        {"clear_badsig", {.flags = CLONE_CLEAR_SIGHAND, .exit_signal = 200}, 0, 0},
        {"small_size", {.flags = CLONE_CLEAR_SIGHAND, .exit_signal = SIGCHLD}, 32, 0},
        {"stack_size_only", {.flags = CLONE_CLEAR_SIGHAND, .exit_signal = SIGCHLD, .stack_size = 4096}, 0, 0},
        {"unknown_flag", {.flags = CLONE_CLEAR_SIGHAND | 1ULL << 40, .exit_signal = SIGCHLD}, 0, 0},
        {"set_tid_size", {.flags = CLONE_CLEAR_SIGHAND, .exit_signal = SIGCHLD, .set_tid = (uintptr_t)&settid, .set_tid_size = 33}, 0, 0},
        /* Not here: an existing set_tid (EPERM without CAP_CHECKPOINT_RESTORE, EEXIST with it)
         * and CLONE_INTO_CGROUP (ignored by a kernel without cgroups) depend on the environment. */
        {"efault", {0}, 0, 1},
    };
    enum { NINV = sizeof inv / sizeof inv[0] };
    struct { const char *name; uint64_t flags, exit_signal; int clear, die; } val[] = {
        {"plain", 0, SIGCHLD, 0, 0},
        {"clear", CLONE_CLEAR_SIGHAND, SIGCHLD, 1, 0},
        {"vfork", CLONE_VFORK, SIGCHLD, 0, 0},
        {"vfork_clear", CLONE_VFORK | CLONE_CLEAR_SIGHAND, SIGCHLD, 1, 0},
        {"clear_die", CLONE_CLEAR_SIGHAND, SIGCHLD, 1, 1},
        {"pidfd_nosig", CLONE_PIDFD | CLONE_CLEAR_SIGHAND, 0, 1, 0},
    };
    enum { NVAL = sizeof val / sizeof val[0] };
    int first_errno[NINV] = {0}, same[NINV] = {0}, vok[NVAL] = {0}, vtry[NVAL] = {0}, vstat[NVAL];
    for (int v = 0; v < NVAL; v++) vstat[v] = -1;
    long rounds = 30 * scale, parent_ok = 0, parent_checks = 0, threads_ok = 0;
    for (long r = 0; r < rounds; r++) {
        for (int i = 0; i < NINV; i++) {
            if (!(narrow >> i & 1)) continue;
            DIAG("round %ld invalid %s\n", r, inv[i].name);
            ptid = 0;
            struct c3args ca = inv[i].a;
            errno = 0;
            long ret = syscall(SYS_clone3, inv[i].badptr ? (void *)16 : (void *)&ca, inv[i].size ? inv[i].size : sizeof ca);
            if (ret == 0) _exit(99); /* an unexpected child of a non-CLONE_VM call */
            int e = ret < 0 ? errno : 0;
            if (ret > 0) { int st; wait_child((pid_t)ret, &st, __WALL); }
            if (r == 0) first_errno[i] = e;
            same[i] += e == first_errno[i];
            parent_ok += c3_parent_ok();
            parent_checks++;
            /* A valid clone right after the failed one. */
            int v = (int)((r * NINV + i) % NVAL);
            DIAG("round %ld valid %s\n", r, val[v].name);
            vtry[v]++;
            int pidfd = -1;
            struct c3args va = {.flags = val[v].flags, .exit_signal = val[v].exit_signal, .pidfd = (uintptr_t)&pidfd};
            long child = syscall(SYS_clone3, &va, sizeof va);
            if (child == 0) _exit(c3_child(val[v].clear, val[v].die));
            int st = -1;
            if (child > 0 && wait_child((pid_t)child, &st, __WALL) == child) {
                int want = val[v].die ? WIFSIGNALED(st) && WTERMSIG(st) == SIGUSR1 : WIFEXITED(st) && WEXITSTATUS(st) == 15;
                if (!want) fprintf(stderr, "round %ld after %s: valid %s status %#x\n", r, inv[i].name, val[v].name, st);
                vok[v] += want;
                if (vstat[v] == -1) vstat[v] = st;
            } else {
                fprintf(stderr, "round %ld after %s: valid %s clone3 %ld errno %s\n", r, inv[i].name, val[v].name, child, ename(errno));
            }
            if (val[v].flags & CLONE_PIDFD) vok[v] -= pidfd < 0;
            if (pidfd >= 0) close(pidfd);
            parent_ok += c3_parent_ok();
            parent_checks++;
        }
        pthread_t t;
        void *res = NULL;
        if (!pthread_create(&t, NULL, c3_thread, NULL) && !pthread_join(t, &res)) threads_ok += res == (void *)1;
    }
    for (int i = 0; i < NINV; i++)
        if (narrow >> i & 1) printf("invalid %s %s %d/%ld\n", inv[i].name, first_errno[i] ? ename(first_errno[i]) : "success", same[i], rounds);
    for (int v = 0; v < NVAL; v++) printf("valid %s status %#x ok %d/%d\n", val[v].name, vstat[v], vok[v], vtry[v]);
    OUT("parentOk", "%ld/%ld", parent_ok, parent_checks);
    OUT("threadsOk", "%ld/%ld", threads_ok, rounds);
    return 1;
}

/* ==== 34. ExecFromNonLeader ==== */

#define EN_KEEP 20
#define EN_CLO 21
enum { EN_SELF, EN_ENOENT, EN_ENOEXEC, EN_SCRIPT, EN_MEMFD, EN_CHAIN, EN_LEADER, EN_VARIANTS };
static const char *en_names[EN_VARIANTS] = {"self", "enoent", "enoexec", "script", "memfd", "chain", "leader"};
static int en_variant, en_junk = -1, en_script = -1, en_elf = -1, en_never;
static char en_tgid[16], en_ppid[16];
static struct fl en_fl;

/* exechelper [SCRIPT] TAG TGID PPID KEEP CLO: reports what the exec'd image sees. */
static int exechelper_main(int argc, char **argv) {
    int a = 2;
    if (argc > a && argv[a][0] == '/') a++; /* the script path a #! interpreter receives */
    if (argc < a + 5) return 97;
    const char *tag = argv[a];
    pid_t tgid = atoi(argv[a + 1]), ppid = atoi(argv[a + 2]);
    int keep = atoi(argv[a + 3]), clo = atoi(argv[a + 4]);
    char st[4096];
    int threads = -1, tgid_line = -1, pid_line = -2;
    if (read_small("/proc/self/status", st, sizeof st) > 0) {
        char *p;
        if ((p = strstr(st, "\nThreads:"))) threads = atoi(p + 9);
        if ((p = strstr(st, "\nTgid:"))) tgid_line = atoi(p + 6);
        if ((p = strstr(st, "\nPid:"))) pid_line = atoi(p + 5);
    }
    sigset_t m;
    sigprocmask(SIG_BLOCK, NULL, &m);
    struct sigaction u1, pi;
    sigaction(SIGUSR1, NULL, &u1);
    sigaction(SIGPIPE, NULL, &pi);
    char exe[PATH_MAX] = "";
    ssize_t n = readlink("/proc/self/exe", exe, sizeof exe - 1);
    if (n > 0) exe[n] = 0;
    int clo_closed = fcntl(clo, F_GETFD) < 0 && errno == EBADF;
    int keep_open = fcntl(keep, F_GETFD) >= 0;
    dprintf(keep, "%s pid %d tid %d ppid %d threads %d status %d keep %d clo %d usr2 %d hup %d usr1dfl %d pipeign %d exe %s\n", tag,
            getpid() == tgid, (pid_t)syscall(SYS_gettid) == getpid(), getppid() == ppid, threads, tgid_line == pid_line && pid_line == tgid,
            keep_open, clo_closed, sigismember(&m, SIGUSR2), sigismember(&m, SIGHUP), u1.sa_handler == SIG_DFL, pi.sa_handler == SIG_IGN,
            strstr(exe, "memfd:") ? "memfd" : n > 0 ? "file" : "none");
    return 0;
}
/* execchain N TAG TGID PPID KEEP CLO: alternates with /bin/sh -c exec until N is 0. */
static int execchain_main(int argc, char **argv) {
    if (argc < 8) return 96;
    int n = atoi(argv[2]);
    if (n == 0) {
        char *av[] = {argv[0], "exechelper", argv[3], argv[4], argv[5], argv[6], argv[7], NULL};
        return exechelper_main(7, av);
    }
    char nb[16];
    snprintf(nb, sizeof nb, "%d", n - 1);
    char *av[] = {"sh", "-c", "exec \"$0\" execchain \"$@\"", self_abs, nb, argv[3], argv[4], argv[5], argv[6], argv[7], NULL};
    execve(UNIX_SH_PATH, av, environ);
    dprintf(atoi(argv[6]), "%s chain depth %d %s\n", argv[3], n, ename(errno));
    return 95;
}
static int memfd_with(const char *name, const void *data, size_t n) {
    int fd = memfd_create(name, 0);
    if (fd < 0) return -1;
    if (write(fd, data, n) != (ssize_t)n) return -1;
    char p[64];
    snprintf(p, sizeof p, "/proc/self/fd/%d", fd);
    int ro = open(p, O_RDONLY | O_CLOEXEC); /* no writable descriptor, or exec fails ETXTBSY */
    close(fd);
    return ro < 0 ? -1 : high_fd(ro, 1);
}
static void en_exec(void) {
    const char *tag = en_names[en_variant];
    char keep[8], clo[8];
    snprintf(keep, sizeof keep, "%d", EN_KEEP);
    snprintf(clo, sizeof clo, "%d", EN_CLO);
    char *av[] = {"cfzproc", "exechelper", (char *)tag, en_tgid, en_ppid, keep, clo, NULL};
    char p[64];
    if (en_variant == EN_ENOENT) {
        execve("/nonexistent-cfzproc/missing", av, environ);
        dprintf(EN_KEEP, "%s first %s\n", tag, ename(errno));
    } else if (en_variant == EN_ENOEXEC) {
        snprintf(p, sizeof p, "/proc/self/fd/%d", en_junk);
        execve(p, av, environ);
        dprintf(EN_KEEP, "%s first %s\n", tag, ename(errno));
    }
    if (en_variant == EN_SCRIPT) {
        snprintf(p, sizeof p, "/proc/self/fd/%d", en_script);
        char *sv[] = {"script", (char *)tag, en_tgid, en_ppid, keep, clo, NULL};
        execve(p, sv, environ);
    } else if (en_variant == EN_MEMFD) {
        syscall(SYS_execveat, en_elf, "", av, environ, AT_EMPTY_PATH);
    } else if (en_variant == EN_CHAIN) {
        char *cv[] = {"cfzproc", "execchain", "10", (char *)tag, en_tgid, en_ppid, keep, clo, NULL};
        execve(self_abs, cv, environ);
    } else {
        execve(self_abs, av, environ);
    }
    dprintf(EN_KEEP, "%s exec failed %s\n", tag, ename(errno));
    _exit(99);
}
static void en_usr1(int s) { (void)s; }
static void *en_blocker(void *arg) { (void)arg; wait_until(&en_never, 1); return NULL; }
static int en_ready, en_execer_ready;
static void *en_execer(void *arg) {
    (void)arg;
    sigset_t m;
    sigemptyset(&m);
    sigaddset(&m, SIGUSR2);
    pthread_sigmask(SIG_BLOCK, &m, NULL);
    set_wake(&en_execer_ready, 1);
    wait_until(&en_ready, 1);
    if (en_variant != EN_LEADER) en_exec();
    wait_until(&en_never, 1);
    return NULL;
}
static void en_child(int v, int wfd, pid_t parent) {
    en_variant = v;
    snprintf(en_tgid, sizeof en_tgid, "%d", getpid());
    snprintf(en_ppid, sizeof en_ppid, "%d", parent);
    signal(SIGUSR1, en_usr1);
    signal(SIGPIPE, SIG_IGN);
    dup2(wfd, EN_KEEP);
    close(wfd);
    int n = open("/dev/null", O_RDONLY);
    dup3(n, EN_CLO, O_CLOEXEC);
    close(n);
    if (v == EN_ENOEXEC) en_junk = memfd_with("junk", "this is not an executable\n", 26);
    if (v == EN_SCRIPT) {
        char s[PATH_MAX + 32];
        int k = snprintf(s, sizeof s, "#!%s exechelper\n", self_abs);
        en_script = memfd_with("script", s, k);
    }
    if (v == EN_MEMFD) {
        int src = open(self_abs, O_RDONLY);
        int fd = memfd_create("cfzproc", 0);
        char buf[65536];
        ssize_t k;
        while (src >= 0 && fd >= 0 && (k = read(src, buf, sizeof buf)) > 0)
            if (write(fd, buf, k) != k) break;
        close(src);
        char p[64];
        snprintf(p, sizeof p, "/proc/self/fd/%d", fd);
        en_elf = high_fd(open(p, O_RDONLY | O_CLOEXEC), 1);
        close(fd);
    }
    fl_start(&en_fl, 1, 300);
    pthread_t b, e;
    pthread_create(&b, NULL, en_blocker, NULL);
    pthread_create(&e, NULL, en_execer, NULL);
    sigset_t m;
    sigemptyset(&m);
    sigaddset(&m, SIGHUP);
    pthread_sigmask(SIG_BLOCK, &m, NULL);
    set_wake(&en_ready, 1);
    if (v == EN_LEADER) {
        wait_until(&en_execer_ready, 1);
        en_exec();
    }
    pthread_join(e, NULL);
    _exit(98);
}
static int t_execnonleader(void) {
    if (access(UNIX_SH_PATH, X_OK)) { puts("UNSUPPORTED /bin/sh"); return 0; }
    long mismatch = 0;
    for (int v = 0; v < EN_VARIANTS; v++) {
        char first[2200] = "";
        for (long rep = 0; rep < scale; rep++) {
            int p[2];
            if (high_pipe(p)) { perror("pipe"); return 0; }
            fflush(stdout);
            pid_t parent = getpid(), pid = fork();
            if (pid == 0) {
                close(p[0]);
                en_child(v, p[1], parent);
            }
            close(p[1]);
            char buf[2048];
            int k = read_all(p[0], buf, sizeof buf, 60000);
            close(p[0]);
            if (k == -2) {
                printf("STUCK exec %s rep %ld\n", en_names[v], rep);
                fflush(stdout);
                kill(pid, SIGKILL);
                wait_child(pid, NULL, 0);
                _exit(3);
            }
            int st = -1;
            wait_child(pid, &st, 0);
            char line[2200];
            snprintf(line, sizeof line, "%sexit %s %s %d\n", k > 0 ? buf : "", en_names[v], WIFEXITED(st) ? "status" : "signal",
                     WIFEXITED(st) ? WEXITSTATUS(st) : WIFSIGNALED(st) ? WTERMSIG(st) : -1);
            if (rep == 0) {
                snprintf(first, sizeof first, "%s", line);
                fputs(line, stdout);
            } else if (strcmp(first, line)) {
                if (mismatch < 3) fprintf(stderr, "exec %s rep %ld differs:\n%s", en_names[v], rep, line);
                mismatch++;
            }
        }
    }
    OUT("repeatMismatch", "%ld", mismatch);
    return 1;
}

/* ==== 35. FailedVforkAndSpawnAttrs ==== */

static volatile sig_atomic_t sa_usr1, sa_chld;
static void sa_h(int s) {
    if (s == SIGUSR1) sa_usr1++;
    else if (s == SIGCHLD) sa_chld++;
}
/* spawncheck TAG: reports the attributes posix_spawn applied, on fd 5. */
static int spawncheck_main(void) {
    sigset_t m;
    sigprocmask(SIG_BLOCK, NULL, &m);
    struct sigaction te, qu;
    sigaction(SIGTERM, NULL, &te);
    sigaction(SIGQUIT, NULL, &qu);
    char cwd[256] = "?", fd7[256] = "closed";
    if (!getcwd(cwd, sizeof cwd)) strcpy(cwd, "?");
    ssize_t n = readlink("/proc/self/fd/7", fd7, sizeof fd7 - 1);
    if (n > 0) fd7[n] = 0;
    dprintf(5, "usr2 %d winch %d term %s quit %s pgrp %d sid %d cwd %s fd6 %s fd7 %s", sigismember(&m, SIGUSR2), sigismember(&m, SIGWINCH),
            te.sa_handler == SIG_DFL ? "dfl" : te.sa_handler == SIG_IGN ? "ign" : "other",
            qu.sa_handler == SIG_DFL ? "dfl" : qu.sa_handler == SIG_IGN ? "ign" : "other", getpgid(0) == getpid(), getsid(0) == getpid(), cwd,
            fcntl(6, F_GETFD) < 0 ? "closed" : "open", fd7);
    return 0;
}
static pid_t spawn42(void) {
    pid_t pid;
    char *av[] = {"cfzproc", "child42", NULL};
    return posix_spawn(&pid, self_abs, NULL, NULL, av, environ) ? -1 : pid;
}
static int t_spawnattrs(void) {
    struct fl f;
    fl_start(&f, 0, 1000);
    struct sigaction a = {0};
    a.sa_handler = sa_h;
    sigaction(SIGUSR1, &a, NULL);
    if (narrow & 1) {
        long n = 20 * scale, eok = 0, exit_ok = 0, sig_ok = 0, spawn_ok = 0;
        int first = -1;
        for (long i = 0; i < n; i++) {
            int ep[2];
            if (high_pipe(ep)) return 0;
            pid_t p = vfork();
            if (p == 0) {
                char *av[] = {"missing", NULL};
                execve("/nonexistent-cfzproc/missing", av, environ);
                int e = errno;
                syscall(SYS_write, ep[1], &e, sizeof e);
                _exit(127);
            }
            close(ep[1]);
            int e = -1, st = -1;
            if (read(ep[0], &e, sizeof e) != sizeof e) e = -1;
            close(ep[0]);
            if (p > 0) wait_child(p, &st, 0);
            if (first < 0) first = e;
            eok += e == ENOENT;
            exit_ok += WIFEXITED(st) && WEXITSTATUS(st) == 127;
            int u = sa_usr1;
            raise(SIGUSR1);
            sig_ok += sa_usr1 == u + 1;
            pid_t c = spawn42();
            st = -1;
            if (c > 0) wait_child(c, &st, 0);
            spawn_ok += WIFEXITED(st) && WEXITSTATUS(st) == 42;
        }
        printf("vforkfail errno %s ok %ld exit127 %ld signal %ld spawn %ld of %ld\n", first > 0 ? ename(first) : "none", eok, exit_ok, sig_ok, spawn_ok, n);
    }
    if (narrow & 2) {
        a.sa_handler = SIG_IGN;
        sigaction(SIGTERM, &a, NULL);
        sigaction(SIGQUIT, &a, NULL);
        sigset_t w;
        sigemptyset(&w);
        sigaddset(&w, SIGWINCH);
        pthread_sigmask(SIG_BLOCK, &w, NULL);
        int z = open("/dev/zero", O_RDONLY);
        dup2(z, 6);
        close(z);
        long bad = 0;
        for (int c = 0; c < 16; c++) {
            for (long rep = 0; rep < scale; rep++) {
                int rp[2];
                if (high_pipe(rp)) return 0;
                posix_spawnattr_t at;
                posix_spawn_file_actions_t fa;
                posix_spawnattr_init(&at);
                posix_spawn_file_actions_init(&fa);
                short fl = 0;
                if (c & 1) { sigset_t m; sigemptyset(&m); sigaddset(&m, SIGUSR2); posix_spawnattr_setsigmask(&at, &m); fl |= POSIX_SPAWN_SETSIGMASK; }
                if (c & 2) { sigset_t d; sigemptyset(&d); sigaddset(&d, SIGTERM); posix_spawnattr_setsigdefault(&at, &d); fl |= POSIX_SPAWN_SETSIGDEF; }
                if (c & 4) { posix_spawnattr_setpgroup(&at, 0); fl |= POSIX_SPAWN_SETPGROUP; }
                if (c & 8) fl |= POSIX_SPAWN_SETSID;
                posix_spawnattr_setflags(&at, fl);
                posix_spawn_file_actions_adddup2(&fa, rp[1], 5);
                posix_spawn_file_actions_addclose(&fa, 6);
                posix_spawn_file_actions_addchdir_np(&fa, "/proc");
                posix_spawn_file_actions_addopen(&fa, 7, "/dev/null", O_RDONLY, 0);
                char *av[] = {"cfzproc", "spawncheck", NULL};
                pid_t pid = -1;
                int r = posix_spawn(&pid, self_abs, &fa, &at, av, environ);
                close(rp[1]);
                posix_spawnattr_destroy(&at);
                posix_spawn_file_actions_destroy(&fa);
                char got[512] = "", line[700], want[512];
                int st = -1;
                if (r) {
                    snprintf(line, sizeof line, "error %s", ename(r));
                } else {
                    if (read_all(rp[0], got, sizeof got, 30000) == -2) stuck("spawncheck", c);
                    wait_child(pid, &st, 0);
                    snprintf(line, sizeof line, "%s exit %d", got, WIFEXITED(st) ? WEXITSTATUS(st) : -1);
                }
                close(rp[0]);
                if ((c & 12) == 12) snprintf(want, sizeof want, "error EPERM");
                else snprintf(want, sizeof want, "usr2 %d winch %d term %s quit ign pgrp %d sid %d cwd /proc fd6 closed fd7 /dev/null exit 0", c & 1, !(c & 1),
                              c & 2 ? "dfl" : "ign", (c & 12) != 0, (c & 8) != 0);
                if (strcmp(line, want)) {
                    if (bad < 5) fprintf(stderr, "spawn attrs %d: got \"%s\" want \"%s\"\n", c, line, want);
                    bad++;
                }
                if (rep == 0) printf("spawn %c%c%c%c %s\n", c & 1 ? 'M' : '-', c & 2 ? 'D' : '-', c & 4 ? 'G' : '-', c & 8 ? 'S' : '-', line);
            }
        }
        close(6);
        pthread_sigmask(SIG_UNBLOCK, &w, NULL);
        OUT("spawnBad", "%ld", bad);
    }
    if (narrow & 4) {
        /* Children of a process whose SIGCHLD is ignored, or handled with SA_NOCLDWAIT, are
         * reaped by the kernel: wait() blocks until all have exited, then fails with ECHILD. */
        for (int mode = 0; mode < 2; mode++) {
            struct sigaction c = {0};
            c.sa_handler = mode ? SIG_IGN : sa_h;
            c.sa_flags = mode ? 0 : SA_NOCLDWAIT;
            sigaction(SIGCHLD, &c, NULL);
            pid_t pids[5];
            int spawned = 0;
            for (int k = 0; k < 5; k++) spawned += (pids[k] = spawn42()) > 0;
            int st, reaped = 0;
            pid_t r;
            while ((r = wait_child(-1, &st, 0)) > 0) reaped++;
            int e1 = errno;
            r = wait_child(pids[0], &st, WNOHANG);
            int e2 = r < 0 ? errno : 0;
            printf("%s spawned %d reaped %d wait %s waitpid %s\n", mode ? "sigign" : "nocldwait", spawned, reaped, ename(e1), e2 ? ename(e2) : "none");
            c.sa_handler = SIG_DFL;
            c.sa_flags = 0;
            sigaction(SIGCHLD, &c, NULL);
        }
    }
    if (narrow & 8) {
        /* With SA_NOCLDSTOP a stopped or continued child sends no SIGCHLD; its exit still does. */
        for (int mode = 0; mode < 2; mode++) {
            struct sigaction c = {0};
            c.sa_handler = sa_h;
            c.sa_flags = mode ? SA_NOCLDSTOP : 0;
            sigaction(SIGCHLD, &c, NULL);
            sigset_t stopmask, oldmask, pending; sigemptyset(&stopmask); sigaddset(&stopmask, SIGCHLD);
            if(mode) pthread_sigmask(SIG_BLOCK,&stopmask,&oldmask);
            sa_chld = 0;
            fflush(stdout);
            pid_t p = fork();
            if (p == 0) {
                syscall(SYS_kill, syscall(SYS_getpid), SIGSTOP);
                _exit(3);
            }
            int st = -1;
            wait_child(p, &st, WUNTRACED);
            int stopped = WIFSTOPPED(st) && WSTOPSIG(st) == SIGSTOP;
            if (!mode) while (!sa_chld) sched_yield();
            if(mode) { sigpending(&pending); if(sigismember(&pending,SIGCHLD)) sa_chld++; pthread_sigmask(SIG_SETMASK,&oldmask,NULL); }
            int at_stop = sa_chld;
            kill(p, SIGCONT);
            wait_child(p, &st, 0);
            int exited = WIFEXITED(st) ? WEXITSTATUS(st) : -1;
            for (int k = 0; k < 2000 && sa_chld <= at_stop; k++) sched_yield();
            printf("%s stopped %d chldAtStop %d exit %d chldAfterExit %d\n", mode ? "nocldstop" : "cldstop", stopped, at_stop > 0, exited, sa_chld > at_stop);
            c.sa_handler = SIG_DFL;
            c.sa_flags = 0;
            sigaction(SIGCHLD, &c, NULL);
        }
    }
    fl_finish(&f);
    OUT("flusherBad", "%ld", f.bad);
    return 1;
}

/* ==== 36. AtforkOrdering ==== */

static pthread_mutex_t af_m[3] = {PTHREAD_MUTEX_INITIALIZER, PTHREAD_MUTEX_INITIALIZER, PTHREAD_MUTEX_INITIALIZER};
static char af_tr[64];
static int af_n, af_stop;
static long af_led[3], af_led2[3];
static void af_rec(char c) {
    if (af_n < 63) af_tr[af_n++] = c;
    af_tr[af_n] = 0;
}
#define AF_H(i)                                                                    \
    static void af_p##i(void) { af_rec('a' + i); pthread_mutex_lock(&af_m[i]); }   \
    static void af_pa##i(void) { af_rec('A' + i); pthread_mutex_unlock(&af_m[i]); } \
    static void af_c##i(void) { af_rec('x' + i); pthread_mutex_unlock(&af_m[i]); }
AF_H(0)
AF_H(1)
AF_H(2)
static void *af_contend(void *arg) {
    uint64_t s = xseed(500 + (long)arg);
    while (!load(&af_stop)) {
        int k = xnext(&s) % 3;
        pthread_mutex_lock(&af_m[k]);
        af_led[k]++;
        if (xnext(&s) % 8 == 0) sched_yield(); /* the fork's prepare handler waits for it */
        pthread_mutex_unlock(&af_m[k]);
        __atomic_add_fetch(&af_led2[k], 1, __ATOMIC_RELAXED);
        sched_yield();
    }
    return NULL;
}
static void *af_lockall(void *arg) {
    (void)arg;
    for (int k = 0; k < 3; k++) {
        pthread_mutex_lock(&af_m[k]);
        pthread_mutex_unlock(&af_m[k]);
    }
    return (void *)1;
}
static int t_atfork(void) {
    pthread_atfork(af_p0, af_pa0, af_c0);
    pthread_atfork(af_p1, af_pa1, af_c1);
    pthread_atfork(af_p2, af_pa2, af_c2);
    struct fl f[2];
    fl_start(&f[0], 0, 300);
    fl_start(&f[1], 1, 300);
    pthread_t ct[2];
    for (long k = 0; k < 2; k++) pthread_create(&ct[k], NULL, af_contend, (void *)k);
    long rounds = 100 * scale, nfork = 0, n_fork = 0, bad = 0;
    char ptr[2][64] = {"", ""}, ctr[2][128] = {"", ""};
    for (long r = 0; r < rounds; r++) {
        int use_fork = r % 4 != 3;
        int p[2];
        if (high_pipe(p)) return 0;
        af_n = 0;
        af_tr[0] = 0;
        fflush(stdout);
        pid_t pid = use_fork ? fork() : _Fork();
        if (pid == 0) {
            close(p[0]);
            int slot = (int)(r % 64), ok = 1;
            ok &= fl_check(&f[0], slot, fl_call(&f[0], slot));
            ok &= fl_check(&f[1], slot, fl_call(&f[1], slot));
            if (use_fork) {
                /* The handlers released every mutex, so this process can take them all. */
                for (int k = 0; k < 3; k++) ok &= !pthread_mutex_lock(&af_m[k]) && !pthread_mutex_unlock(&af_m[k]);
                pthread_t t;
                void *res = NULL;
                ok &= !pthread_create(&t, NULL, af_lockall, NULL) && !pthread_join(t, &res) && res == (void *)1;
            }
            char msg[128];
            int k = snprintf(msg, sizeof msg, "%s ok %d", af_tr, ok);
            if (write(p[1], msg, k) != k) _exit(2);
            _exit(0);
        }
        close(p[1]);
        char parent[64];
        snprintf(parent, sizeof parent, "%s", af_tr);
        char child[128] = "";
        if (read_all(p[0], child, sizeof child, 30000) == -2) stuck("atfork child", r);
        close(p[0]);
        int st = -1;
        wait_child(pid, &st, 0);
        nfork += use_fork;
        n_fork += !use_fork;
        for (int k = 0; k < 3; k++) {
            pthread_mutex_lock(&af_m[k]);
            pthread_mutex_unlock(&af_m[k]);
        }
        const char *wp = use_fork ? "cbaABC" : "", *wc = use_fork ? "cbaxyz ok 1" : " ok 1";
        if (strcmp(parent, wp) || strcmp(child, wc) || !WIFEXITED(st) || WEXITSTATUS(st)) {
            if (bad < 5) fprintf(stderr, "round %ld %s: parent \"%s\" child \"%s\" status %#x\n", r, use_fork ? "fork" : "_Fork", parent, child, st);
            bad++;
        }
        if (!ptr[!use_fork][0] && !ctr[!use_fork][0]) {
            snprintf(ptr[!use_fork], 64, "%s", parent[0] ? parent : "-");
            snprintf(ctr[!use_fork], 128, "%s", child);
        }
    }
    __atomic_store_n(&af_stop, 1, __ATOMIC_RELEASE);
    for (int k = 0; k < 2; k++) pthread_join(ct[k], NULL);
    fl_finish(&f[0]);
    fl_finish(&f[1]);
    int consistent = 1;
    for (int k = 0; k < 3; k++) consistent &= af_led[k] == af_led2[k];
    OUT("rounds", "%ld fork %ld _Fork %ld", rounds, nfork, n_fork);
    OUT("forkTrace", "parent %s child %s", ptr[0], ctr[0]);
    OUT("_ForkTrace", "parent %s child %s", ptr[1], ctr[1]);
    OUT("bad", "%ld", bad);
    OUT("ledgerConsistent", "%d", consistent);
    OUT("flusherBad", "%ld", f[0].bad + f[1].bad);
    return 1;
}

/* ==== 38. CloseAllFdHygiene ==== */

static int fd_cmp(const void *a, const void *b) { return *(const int *)a - *(const int *)b; }
/* Prints descriptors 3 and up with normalized targets, and which of 0-2 are open. */
static void fd_show(const char *label) {
    DIR *d = opendir("/proc/self/fd");
    if (!d) { printf("%s unavailable %s\n", label, ename(errno)); return; }
    int dfd = dirfd(d), fds[512], n = 0;
    struct dirent *e;
    while ((e = readdir(d)))
        if (e->d_name[0] != '.' && atoi(e->d_name) != dfd && n < 512) fds[n++] = atoi(e->d_name);
    closedir(d);
    qsort(fds, n, sizeof fds[0], fd_cmp);
    char line[8192];
    int off = 0, low = 0;
    for (int i = 0; i < n; i++) {
        if (fds[i] < 3) { low |= 1 << fds[i]; continue; }
        char p[64], t[256];
        snprintf(p, sizeof p, "/proc/self/fd/%d", fds[i]);
        ssize_t k = readlink(p, t, sizeof t - 1);
        if (k < 0) continue; /* closed since the listing */
        t[k] = 0;
        if (!strncmp(t, "pipe:", 5)) strcpy(t, "pipe");
        else if (!strncmp(t, "socket:", 7)) strcpy(t, "socket");
        if (off < (int)sizeof line - 300) off += snprintf(line + off, sizeof line - off, " %d=%s", fds[i], t);
    }
    line[off] = 0;
    printf("%s low %d fds%s\n", label, low, off ? line : " none");
}
static int fd_never;
static volatile sig_atomic_t fd_sig;
static void fd_h(int s) { (void)s; fd_sig++; }
static void *fd_blocked(void *arg) { (void)arg; wait_until(&fd_never, 1); return NULL; }
static void *fd_work(void *arg) {
    uint64_t s = xseed((long)arg), acc = 0;
    for (int i = 0; i < 100000; i++) acc += xnext(&s);
    int p[2];
    if (!pipe(p)) { close(p[0]); close(p[1]); }
    return (void *)(uintptr_t)(acc & 0xffff);
}
static int fdlist_main(void) {
    struct rlimit rl;
    getrlimit(RLIMIT_NOFILE, &rl);
    fd_show("child");
    printf("childRlimit %lu %lu\n", (unsigned long)rl.rlim_cur, (unsigned long)rl.rlim_max);
    return 0;
}
/* close_range(first, ~0U, flags), or with NARROW bit 1 clear a loop of close/F_SETFD over
 * first..4095 (to get past a translator that cannot run close_range). */
static long close_from(unsigned first, unsigned flags) {
    if (narrow & 1) return syscall(SYS_close_range, first, ~0U, flags);
    for (unsigned fd = first; fd < 4096; fd++) {
        if (flags & CLOSE_RANGE_CLOEXEC) fcntl(fd, F_SETFD, FD_CLOEXEC);
        else close(fd);
    }
    return 0;
}
static int t_fdhygiene(void) {
    fd_show("start");
    if (close_from(3, 0)) { printf("UNSUPPORTED close_range %s\n", ename(errno)); return 0; }
    fd_show("closed");
    int n = open("/dev/null", O_RDWR);
    OUT("firstOpen", "%d", n);
    dup2(n, 0);
    dup2(n, 9);
    dup2(n, 4);
    close(n);
    fd_show("dup2");
    pthread_t b[2], w[4];
    for (int k = 0; k < 2; k++) pthread_create(&b[k], NULL, fd_blocked, NULL);
    uint64_t sum = 0;
    for (long k = 0; k < 4; k++) pthread_create(&w[k], NULL, fd_work, (void *)k);
    for (int k = 0; k < 4; k++) { void *r; pthread_join(w[k], &r); sum += (uintptr_t)r; }
    OUT("threadSum", "%lu", (unsigned long)sum);
    /* Again, with threads alive. */
    long r1 = close_from(5, 0);
    long r2 = syscall(SYS_close_range, 4, 4, CLOSE_RANGE_CLOEXEC);
    printf("closeRange %ld cloexec %ld fd4flags %d\n", r1, r2, fcntl(4, F_GETFD));
    fd_show("threads");
    struct rlimit rl = {16, 16};
    int rr = setrlimit(RLIMIT_NOFILE, &rl);
    getrlimit(RLIMIT_NOFILE, &rl);
    printf("rlimit set %d cur %lu max %lu\n", rr, (unsigned long)rl.rlim_cur, (unsigned long)rl.rlim_max);
    int opened[32], no = 0, e = 0;
    char ol[256] = "";
    int ooff = 0;
    for (;;) {
        int f = open("/dev/null", O_RDONLY | O_CLOEXEC);
        if (f < 0) { e = errno; break; }
        if (no < 32) opened[no++] = f;
        else { close(f); break; }
        ooff += snprintf(ol + ooff, sizeof ol - ooff, " %d", f);
    }
    printf("opens %d until %s:%s\n", no, ename(e), ol);
    for (int k = 0; k < no; k++) close(opened[k]);
    void *h = dlopen("libm.so.6", RTLD_NOW);
    double (*cosp)(double) = h ? (double (*)(double))dlsym(h, "cos") : NULL;
    printf("dlopen %s\n", cosp ? (cosp(0.0) == 1.0 ? "ok" : "wrong") : "unavailable");
    struct sigaction a = {0};
    a.sa_handler = fd_h;
    sigaction(SIGUSR1, &a, NULL);
    for (int k = 0; k < 3; k++) raise(SIGUSR1);
    OUT("signals", "%d", (int)fd_sig);
    fflush(stdout);
    pid_t pid = fork();
    if (pid == 0) {
        char *av[] = {"cfzproc", "fdlist", NULL};
        execve(self_abs, av, environ);
        printf("child exec %s\n", ename(errno));
        fflush(stdout);
        _exit(127);
    }
    int st = -1;
    wait_child(pid, &st, 0);
    printf("childExit %d\n", WIFEXITED(st) ? WEXITSTATUS(st) : -1);
    fd_show("end");
    set_wake(&fd_never, 1);
    for (int k = 0; k < 2; k++) pthread_join(b[k], NULL);
    return 1;
}

/* ==== 39. SetxidAcrossThreads ==== */

#define SX_T 4
static int sx_epoch, sx_ack, sx_stop, sx_pipe[2];
static uid_t sx_u[3];
static gid_t sx_g[3], sx_groups[4];
static int sx_ng;
static long sx_bad[SX_T + 2];
static int sx_parse(const char *st, const char *key, unsigned *v, int n) {
    const char *p = strstr(st, key);
    if (!p) return 0;
    p += strlen(key);
    for (int i = 0; i < n; i++) v[i] = (unsigned)strtoul(p, (char **)&p, 10);
    return 1;
}
static int sx_groups_ok(const char *st) {
    const char *p = strstr(st, "\nGroups:");
    if (!p) return 0;
    p += 8;
    for (int i = 0; i < sx_ng; i++) {
        unsigned v = (unsigned)strtoul(p, (char **)&p, 10);
        if (v != sx_groups[i]) return 0;
    }
    while (*p == ' ' || *p == '\t') p++;
    return *p == '\n';
}
static int sx_status_ok(const char *path) {
    char st[4096];
    unsigned u[4], g[4];
    if (read_small(path, st, sizeof st) < 0) return 0;
    if (!sx_parse(st, "\nUid:", u, 4) || !sx_parse(st, "\nGid:", g, 4)) return 0;
    return u[0] == sx_u[0] && u[1] == sx_u[1] && u[2] == sx_u[2] && u[3] == sx_u[1] && g[0] == sx_g[0] && g[1] == sx_g[1] && g[2] == sx_g[2] &&
           g[3] == sx_g[1] && sx_groups_ok(st);
}
/* What this thread's credentials are, per syscall and per /proc. */
static int sx_check(void) {
    uid_t r, e, s;
    gid_t gr, ge, gs;
    getresuid(&r, &e, &s);
    getresgid(&gr, &ge, &gs);
    gid_t gl[8];
    int ng = getgroups(8, gl), ok = ng == sx_ng;
    for (int i = 0; ok && i < ng; i++) ok &= gl[i] == sx_groups[i];
    return ok && r == sx_u[0] && e == sx_u[1] && s == sx_u[2] && gr == sx_g[0] && ge == sx_g[1] && gs == sx_g[2] && sx_status_ok("/proc/thread-self/status");
}
static void *sx_thread(void *arg) {
    long kind = (long)arg;
    for (int seen = 1;; seen++) {
        if (kind == 0) {
            wait_until(&sx_epoch, seen);
        } else if (kind == 1) {
            while (load(&sx_epoch) < seen) sched_yield();
        } else if (kind == 2) {
            char b;
            while (read(sx_pipe[0], &b, 1) < 0 && errno == EINTR) {}
        } else {
            for (long i = 0; load(&sx_epoch) < seen; i++)
                if ((i & 1023) == 1023) sched_yield();
        }
        if (load(&sx_stop)) break;
        sx_bad[kind] += !sx_check();
        __atomic_add_fetch(&sx_ack, 1, __ATOMIC_RELEASE);
        futex(&sx_ack, FUTEX_WAKE_PRIVATE, INT_MAX);
    }
    return NULL;
}
static int sx_round(int seen) {
    set_wake(&sx_epoch, seen);
    char b = 1;
    if (write(sx_pipe[1], &b, 1) != 1) return 0;
    double end = now() + 30;
    for (;;) {
        int v = load(&sx_ack);
        if (v >= SX_T * seen) break;
        if (now() > end) stuck("setxid acks", seen);
        struct timespec ts = {0, 10000000};
        syscall(SYS_futex, &sx_ack, FUTEX_WAIT_PRIVATE, v, &ts, NULL, 0);
    }
    sx_bad[SX_T] += !sx_check();
    DIR *d = opendir("/proc/self/task");
    struct dirent *e;
    int tasks = 0;
    while (d && (e = readdir(d))) {
        if (e->d_name[0] == '.') continue;
        char p[300];
        snprintf(p, sizeof p, "/proc/self/task/%s/status", e->d_name);
        sx_bad[SX_T + 1] += !sx_status_ok(p);
        tasks++;
    }
    if (d) closedir(d);
    return tasks;
}
static int sx_child(void) {
    if (setresgid(1049, 1049, 1049) || setresgid(0, 0, 0)) { printf("UNSUPPORTED setxid ids %s\n", ename(errno)); return 4; }
    sx_ng = 0;
    if (setgroups(0, NULL)) { printf("UNSUPPORTED setgroups %s\n", ename(errno)); return 4; }
    getresuid(&sx_u[0], &sx_u[1], &sx_u[2]);
    getresgid(&sx_g[0], &sx_g[1], &sx_g[2]);
    if (pipe(sx_pipe)) return 2;
    struct fl f;
    fl_start(&f, 1, 500);
    pthread_t t[SX_T];
    for (long k = 0; k < SX_T; k++) pthread_create(&t[k], NULL, sx_thread, (void *)k);
    long rounds = 40 * scale, fails = 0;
    int seen = 0, tasks = 0;
    for (long r = 0; r < rounds; r++) {
        gid_t g[3];
        for (int i = 0; i < 3; i++) g[i] = r % 3 == 0 ? 0 : 1000 + next() % 50;
        fails += setresgid(g[0], g[1], g[2]) != 0;
        memcpy(sx_g, g, sizeof g);
        if (next() & 1) {
            sx_ng = next() % 4;
            for (int i = 0; i < sx_ng; i++) sx_groups[i] = 2000 + i * 7 + next() % 5;
            fails += setgroups(sx_ng, sx_groups) != 0;
        }
        tasks = sx_round(++seen);
    }
    /* Drop: groups, then gid, then uid; regaining root must then fail in every thread. */
    sx_ng = 1;
    sx_groups[0] = 3000;
    fails += setgroups(1, sx_groups) != 0;
    gid_t dg[3] = {1100, 1101, 1102};
    uid_t du[3] = {2000, 2001, 2002};
    fails += setresgid(dg[0], dg[1], dg[2]) != 0;
    memcpy(sx_g, dg, sizeof dg);
    sx_round(++seen);
    fails += setresuid(du[0], du[1], du[2]) != 0;
    memcpy(sx_u, du, sizeof du);
    sx_round(++seen);
    int regain = setresuid(0, 0, 0);
    int regain_e = errno;
    sx_round(++seen);
    __atomic_store_n(&sx_stop, 1, __ATOMIC_RELEASE);
    set_wake(&sx_epoch, seen + 1);
    char b[SX_T] = {0};
    if (write(sx_pipe[1], b, 1) != 1) return 2;
    for (int k = 0; k < SX_T; k++) pthread_join(t[k], NULL);
    fl_finish(&f);
    OUT("rounds", "%ld", rounds + 3);
    OUT("tasks", "%d", tasks);
    OUT("setFails", "%ld", fails);
    OUT("threadBad", "futex %ld sleep %ld pipe %ld spin %ld", sx_bad[0], sx_bad[1], sx_bad[2], sx_bad[3]);
    OUT("mainBad", "%ld", sx_bad[SX_T]);
    OUT("taskStatusBad", "%ld", sx_bad[SX_T + 1]);
    OUT("regainRoot", "%d %s", regain, regain ? ename(regain_e) : "none");
    OUT("final", "uid %d %d %d gid %d %d %d", (int)sx_u[0], (int)sx_u[1], (int)sx_u[2], (int)sx_g[0], (int)sx_g[1], (int)sx_g[2]);
    OUT("flusherBad", "%ld", f.bad);
    return 0;
}
static int t_setxid(void) {
    if (geteuid() != 0) { puts("UNSUPPORTED setxid needs root"); return 0; }
    fflush(stdout);
    pid_t p = fork();
    if (p == 0) {
        int r = sx_child();
        fflush(stdout);
        _exit(r);
    }
    int st = -1;
    wait_child(p, &st, 0);
    if (WIFEXITED(st) && WEXITSTATUS(st) == 0) return 1;
    if (WIFEXITED(st) && WEXITSTATUS(st) == 4) return 0;
    printf("child %s %d\n", WIFEXITED(st) ? "status" : "signal", WIFEXITED(st) ? WEXITSTATUS(st) : WTERMSIG(st));
    return 0;
}

/* ==== 40. CondWaitCancellation ==== */

static pthread_mutex_t cc_m;
static pthread_cond_t cc_c = PTHREAD_COND_INITIALIZER;
static int cc_waiting, cc_release;
static long cc_ledger, cc_cleanups, cc_normal, cc_obad, cc_nbad;
static void cc_cleanup(void *arg) {
    (void)arg;
    /* Cancellation in a condition wait reacquires the mutex before the cleanup handlers run;
     * an error-checking mutex says so with EDEADLK. */
    int e = pthread_mutex_lock(&cc_m);
    if (e != EDEADLK) cc_obad++;
    cc_ledger += 1000;
    cc_cleanups++;
    cc_waiting--;
    pthread_mutex_unlock(&cc_m);
}
static void *cc_waiter(void *arg) {
    long timed = (long)arg;
    pthread_mutex_lock(&cc_m);
    cc_waiting++;
    pthread_cleanup_push(cc_cleanup, NULL);
    while (!cc_release) {
        if (timed) {
            struct timespec ts;
            clock_gettime(CLOCK_REALTIME, &ts);
            ts.tv_sec += 3600;
            pthread_cond_timedwait(&cc_c, &cc_m, &ts);
        } else {
            pthread_cond_wait(&cc_c, &cc_m);
        }
    }
    pthread_cleanup_pop(0);
    int e = pthread_mutex_lock(&cc_m);
    if (e != EDEADLK) cc_nbad++;
    cc_ledger += 1;
    cc_normal++;
    cc_waiting--;
    pthread_mutex_unlock(&cc_m);
    return NULL;
}
static int t_condcancel(void) {
    pthread_mutexattr_t ma;
    pthread_mutexattr_init(&ma);
    pthread_mutexattr_settype(&ma, PTHREAD_MUTEX_ERRORCHECK);
    pthread_mutex_init(&cc_m, &ma);
    struct fl f;
    fl_start(&f, 0, 200);
    long rounds = 300 * scale, threads = 0, canceled = 0, joined_normal = 0, expect = 0, bcast = 0, leftover = 0;
    for (long r = 0; r < rounds; r++) {
        int nw = 2 + next() % 3, nv = 1 + (next() % 4 == 0);
        pthread_t t[4];
        cc_release = 0;
        for (int k = 0; k < nw; k++) pthread_create(&t[k], NULL, cc_waiter, (void *)(long)(next() & 1));
        threads += nw;
        double end = now() + 30;
        for (;;) {
            pthread_mutex_lock(&cc_m);
            int w = cc_waiting;
            pthread_mutex_unlock(&cc_m);
            if (w == nw) break;
            if (now() > end) stuck("condcancel waiters", r);
            sched_yield();
        }
        for (int k = 0; k < nv; k++) pthread_cancel(t[k]);
        for (int k = 0; k < nv; k++) {
            void *res = NULL;
            pthread_join(t[k], &res);
            canceled += res == PTHREAD_CANCELED;
        }
        /* Reuse the condition and the mutex at once. */
        pthread_mutex_lock(&cc_m);
        leftover += cc_waiting != nw - nv;
        cc_release = 1;
        if (next() & 1) {
            pthread_cond_broadcast(&cc_c);
            bcast++;
        } else {
            for (int k = nv; k < nw; k++) pthread_cond_signal(&cc_c);
        }
        pthread_mutex_unlock(&cc_m);
        for (int k = nv; k < nw; k++) {
            void *res = (void *)1;
            pthread_join(t[k], &res);
            joined_normal += res == NULL;
        }
        expect += 1000L * nv + (nw - nv);
    }
    fl_finish(&f);
    OUT("rounds", "%ld broadcast %ld", rounds, bcast);
    OUT("threads", "%ld", threads);
    OUT("canceled", "%ld cleanups %ld", canceled, cc_cleanups);
    OUT("normal", "%ld joined %ld", cc_normal, joined_normal);
    OUT("ledger", "%ld expect %ld", cc_ledger, expect);
    OUT("ownerBad", "cleanup %ld normal %ld", cc_obad, cc_nbad);
    OUT("waitingBad", "%ld left %d", leftover, cc_waiting);
    OUT("flusherBad", "%ld", f.bad);
    return 1;
}

/* ==== 41. ProcExitStorm ==== */

#define PE_T 64
#define PE_R 4
static uint64_t pe_res[PE_T];
static int pe_batch, pe_stop, pe_scan_stop;
struct pe_rs { long dirs, reads, enoent, esrch, other, retained_ok, retained_err; };
static struct pe_rs pe_rs[PE_R + 1];
static uint64_t pe_work(long id) {
    uint64_t s = xseed(id), acc = 0;
    for (int i = 0; i < 3000; i++) acc += xnext(&s) >> (i & 7);
    return acc;
}
static void *pe_worker(void *arg) {
    long id = (long)arg;
    pe_res[id] = pe_work(pe_batch * PE_T + id);
    if (id % 3 == 1) pthread_exit(NULL);
    if (id % 3 == 2) syscall(SYS_exit, 0); /* the thread alone, without glibc's exit path */
    return NULL;
}
static void pe_err(struct pe_rs *s, int e) {
    if (e == ENOENT) s->enoent++;
    else if (e == ESRCH) s->esrch++;
    else s->other++;
}
static void *pe_reader(void *arg) {
    long me = (long)arg;
    struct pe_rs *s = &pe_rs[me];
    int ring[8], ri = 0;
    long n = 0;
    for (int k = 0; k < 8; k++) ring[k] = -1;
    while (!load(&pe_stop)) {
        DIR *d = opendir("/proc/self/task");
        if (!d) { pe_err(s, errno); continue; }
        struct dirent *e;
        while ((e = readdir(d))) {
            if (e->d_name[0] == '.') continue;
            char p[300], buf[2048];
            snprintf(p, sizeof p, "/proc/self/task/%s/stat", e->d_name);
            if (read_small(p, buf, sizeof buf) < 0) pe_err(s, errno); else s->reads++;
            snprintf(p, sizeof p, "/proc/self/task/%s/status", e->d_name);
            if (read_small(p, buf, sizeof buf) < 0) pe_err(s, errno); else s->reads++;
            if ((++n & 7) == 0) {
                /* Keep a directory descriptor of the task past its exit, then use it. */
                snprintf(p, sizeof p, "/proc/self/task/%s", e->d_name);
                int fd = open(p, O_RDONLY | O_DIRECTORY | O_CLOEXEC);
                if (fd < 0) { pe_err(s, errno); continue; }
                if (ring[ri] >= 0) {
                    int f = openat(ring[ri], "status", O_RDONLY | O_CLOEXEC);
                    if (f >= 0 && read(f, buf, sizeof buf) > 0) s->retained_ok++;
                    else s->retained_err++;
                    if (f >= 0) close(f);
                    close(ring[ri]);
                }
                ring[ri] = fd;
                ri = (ri + 1) % 8;
            }
        }
        closedir(d);
        s->dirs++;
    }
    for (int k = 0; k < 8; k++)
        if (ring[k] >= 0) close(ring[k]);
    return NULL;
}
static void *pe_scanner(void *arg) {
    (void)arg;
    struct pe_rs *s = &pe_rs[PE_R];
    while (!load(&pe_scan_stop)) {
        DIR *d = opendir("/proc");
        if (!d) { pe_err(s, errno); continue; }
        struct dirent *e;
        while ((e = readdir(d))) {
            if (e->d_name[0] < '0' || e->d_name[0] > '9') continue;
            char p[300], buf[2048];
            snprintf(p, sizeof p, "/proc/%s/status", e->d_name);
            if (read_small(p, buf, sizeof buf) < 0) pe_err(s, errno); else s->reads++;
        }
        closedir(d);
        s->dirs++;
    }
    return NULL;
}
static int procexit(int counts) {
    pthread_t rd[PE_R], w[PE_T];
    for (long k = 0; k < PE_R; k++) pthread_create(&rd[k], NULL, pe_reader, (void *)k);
    long batches = 20 * scale, bad = 0, created = 0;
    uint64_t hash = 1469598103934665603ULL;
    for (long b = 0; b < batches; b++) {
        pe_batch = (int)b;
        for (long k = 0; k < PE_T; k++) created += !pthread_create(&w[k], NULL, pe_worker, (void *)k);
        for (int k = 0; k < PE_T; k++) pthread_join(w[k], NULL);
        for (int k = 0; k < PE_T; k++) {
            uint64_t want = pe_work(b * PE_T + k);
            if (pe_res[k] != want) {
                if (bad < 5) fprintf(stderr, "batch %ld thread %d: %#lx want %#lx\n", b, k, (unsigned long)pe_res[k], (unsigned long)want);
                bad++;
            }
            hash = (hash ^ pe_res[k]) * 1099511628211ULL;
            pe_res[k] = 0;
        }
    }
    __atomic_store_n(&pe_stop, 1, __ATOMIC_RELEASE);
    for (int k = 0; k < PE_R; k++) pthread_join(rd[k], NULL);
    pthread_t sc;
    pthread_create(&sc, NULL, pe_scanner, NULL);
    long forks = 200 * scale, exitsum = 0, exitbad = 0;
    for (long i = 0; i < forks; i++) {
        char code[8];
        snprintf(code, sizeof code, "%ld", i % 100);
        fflush(stdout);
        pid_t p = fork();
        if (p == 0) {
            char *av[] = {"cfzproc", "exitwith", code, NULL};
            execve(self_abs, av, environ);
            _exit(127);
        }
        int st = -1;
        if (p > 0) wait_child(p, &st, 0);
        int ok = WIFEXITED(st) && WEXITSTATUS(st) == i % 100;
        if (!ok && exitbad < 5) fprintf(stderr, "fork %ld: status %#x\n", i, st);
        exitbad += !ok;
        exitsum += WIFEXITED(st) ? WEXITSTATUS(st) : 0;
    }
    __atomic_store_n(&pe_scan_stop, 1, __ATOMIC_RELEASE);
    pthread_join(sc, NULL);
    OUT("batches", "%ld threads %ld", batches, created);
    OUT("workBad", "%ld", bad);
    OUT("workHash", "%016lx", (unsigned long)hash);
    OUT("forks", "%ld exitSum %ld exitBad %ld", forks, exitsum, exitbad);
    if (counts) {
        for (int k = 0; k <= PE_R; k++)
            printf("%s %d dirs %ld reads %ld enoent %ld esrch %ld other %ld retainedOk %ld retainedErr %ld\n", k < PE_R ? "reader" : "scanner", k, pe_rs[k].dirs,
                   pe_rs[k].reads, pe_rs[k].enoent, pe_rs[k].esrch, pe_rs[k].other, pe_rs[k].retained_ok, pe_rs[k].retained_err);
    } else {
        long other = 0;
        for (int k = 0; k <= PE_R; k++) other += pe_rs[k].other;
        if (other) fprintf(stderr, "procfs errors other than ENOENT/ESRCH: %ld\n", other);
        DIAG("reader 0 dirs %ld reads %ld enoent %ld esrch %ld\n", pe_rs[0].dirs, pe_rs[0].reads, pe_rs[0].enoent, pe_rs[0].esrch);
    }
    return 1;
}
static int t_procexit(void) { return procexit(0); }
static int t_procexitcounts(void) { return procexit(1); }

static const struct { const char *name; int (*fn)(void); } TESTS[] = {
{"clone3fail",t_clone3},{"execnonleader",t_execnonleader},{"spawnattrs",t_spawnattrs},{"atfork",t_atfork},
{"fdhygiene",t_fdhygiene},{"setxid",t_setxid},{"condcancel",t_condcancel},{"procexit",t_procexit},{"procexitcounts",t_procexitcounts}};
int main(int argc, char **argv) {
    if (argc > 1 && !strcmp(argv[1], "child42")) return 42;
    if (argc > 2 && !strcmp(argv[1], "exitwith")) return atoi(argv[2]);
    static char exe[PATH_MAX];
    ssize_t n = readlink("/proc/self/exe", exe, sizeof exe - 1);
    if (n > 0) exe[n] = 0;
    self_path = strchr(argv[0], '/') ? argv[0] : exe;
    if (!realpath(self_path, self_abs)) snprintf(self_abs, sizeof self_abs, "%s", exe);
    if (argc > 1 && !strcmp(argv[1], "exechelper")) return exechelper_main(argc, argv);
    if (argc > 1 && !strcmp(argv[1], "execchain")) return execchain_main(argc, argv);
    if (argc > 1 && !strcmp(argv[1], "spawncheck")) return spawncheck_main();
    if (argc > 1 && !strcmp(argv[1], "fdlist")) return fdlist_main();
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
    pgsz=(size_t)sysconf(_SC_PAGESIZE);
    for (size_t i = 0; i < sizeof TESTS / sizeof TESTS[0]; i++) {
        if (strcmp(argv[1], TESTS[i].name)) continue;
        test_name = TESTS[i].name;
        double t0 = now();
        int ok = TESTS[i].fn();
        DIAG("%s took %.3f s\n", test_name, now() - t0);
        if (ok) printf("OK %s\n", argv[1]);
        return !ok;
    }
    fprintf(stderr, "unknown test %s\n", argv[1]);
    return 2;
}
