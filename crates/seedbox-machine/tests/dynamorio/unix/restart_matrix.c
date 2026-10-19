/* Source Seedbox f32472b7fe470367b7eddecde1ac53ad299f3a3e cfz.c t_restart:
 * original17-selector table,256KiB numbered write population,3signal modes,
 * raw ABI frames at syscall/return, firstEINTR-or3restart threshold. Public
 * Linux API only, positive blocked-entry replaces incidental paced sleeps.
 * Sleep subjects keep200ms request/remainder/success; elapsed/deadline clock
 * arithmetic remains an explicit unresolved source axis, not equivalent. */
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


#include "support.h"
static uint64_t mono_ns(clockid_t clock) {struct timespec t;if(clock_gettime(clock,&t))abort();return (uint64_t)t.tv_sec*1000000000+t.tv_nsec;}
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

long cfz_rawsys(long,long,long,long,long,long,long);
extern const char cfz_svc_at[],cfz_svc_after[];
#if defined(__aarch64__)
asm(".text\n.p2align 2\n.globl cfz_rawsys\n.type cfz_rawsys,%function\ncfz_rawsys:\nmov x8,x6\n.globl cfz_svc_at\ncfz_svc_at:\nsvc #0\n.globl cfz_svc_after\ncfz_svc_after:\nret\n");
#else
asm(".text\n.p2align 2\n.globl cfz_rawsys\n.type cfz_rawsys,@function\ncfz_rawsys:\nmov 8(%rsp),%rax\nmov %rcx,%r10\n.globl cfz_svc_at\ncfz_svc_at:\nsyscall\n.globl cfz_svc_after\ncfz_svc_after:\nret\n");
#endif
static volatile int rm_bad_frame,rm_blocked,rm_signal_code,rm_signal_pid,rm_partial;
static uint64_t kmask_get(void) {uint64_t m=0;if(syscall(SYS_rt_sigprocmask,SIG_SETMASK,NULL,&m,8))abort();return m;}
#define SIGBIT(sig) (UINT64_C(1)<<((sig)-1))
static void rm_handler(int s,siginfo_t *si,void *raw) {
    int saved=errno;ucontext_t *uc=raw;
#if defined(__aarch64__)
    uintptr_t pc=uc->uc_mcontext.pc;long ret=uc->uc_mcontext.regs[0];
#else
    uintptr_t pc=uc->uc_mcontext.gregs[REG_RIP];long ret=uc->uc_mcontext.gregs[REG_RAX];
#endif
    /* The original restart oracle classifies PC/result, not siginfo identity.
     * Keep identity diagnostic separate: a minimal native self-tgkill control
     * currently observes SI_USER/pid0 on this development host. Source signal
     * identity assertions in other fixtures remain intact and independent. */
    if(s!=SIGUSR1)rm_bad_frame=1;
    rm_signal_code=si->si_code;rm_signal_pid=si->si_pid;
    if(pc==(uintptr_t)cfz_svc_at)__atomic_add_fetch(&rm_robs,1,__ATOMIC_RELEASE);
    else if(pc==(uintptr_t)cfz_svc_after&&ret==-EINTR)__atomic_add_fetch(&rm_eobs,1,__ATOMIC_RELEASE);
    else if(rm_case==RM_WRITE&&pc==(uintptr_t)cfz_svc_after&&ret>0&&ret<=RM_WTOTAL)
        __atomic_add_fetch(&rm_partial,1,__ATOMIC_RELEASE); /* valid interrupted partial write */
    else rm_bad_frame|=2;
    if(rm_nested&&syscall(SYS_tgkill,getpid(),rm_tid,SIGUSR2))_exit(2);
    __atomic_add_fetch(&rm_usr1,1,__ATOMIC_RELEASE);errno=saved;
}
static void rm_handler2(int s) {if(s!=SIGUSR2)_exit(3);__atomic_add_fetch(&rm_usr2,1,__ATOMIC_RELEASE);}
static void rm_noop(int s) {(void)s;}
static void rm_kick(void) {
    int before=__atomic_load_n(&rm_usr1,__ATOMIC_ACQUIRE);
    if(syscall(SYS_tgkill,getpid(),rm_tid,SIGUSR1))abort();
    while(__atomic_load_n(&rm_usr1,__ATOMIC_ACQUIRE)==before)sched_yield();
}
static int rm_observe(void);
/* Keep the source every-four-drains interrupt opportunities, qualified by an
 * actual still-blocked writer. Completion is a positive event: never signal
 * unrelated application/join code after the whole stream was already written. */
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
        if(++n%4==0&&got<total&&rm_observe())rm_kick();
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

static long rm_nr(void) {
    static const long nrs[RM_N]={SYS_read,SYS_write,SYS_nanosleep,SYS_clock_nanosleep,
        SYS_futex,SYS_ppoll,SYS_epoll_pwait2,SYS_accept4,SYS_connect,SYS_recvmsg,
        SYS_wait4,SYS_flock,SYS_rt_sigtimedwait,SYS_semop,SYS_io_uring_enter,
        SYS_futex_waitv,SYS_rt_sigsuspend};
    return nrs[rm_case];
}
static int rm_observe(void) {
    char path[96],text[512];snprintf(path,sizeof path,"/proc/self/task/%d/syscall",rm_tid);
    for(;;) {
        if(__atomic_load_n(&rm_main_done,__ATOMIC_ACQUIRE))return 0;
        int fd=open(path,O_RDONLY|O_CLOEXEC);if(fd<0){perror(path);_exit(2);}
        ssize_t n=read(fd,text,sizeof text-1);close(fd);if(n<0){perror(path);_exit(2);}text[n]=0;
        long nr=-1;if(sscanf(text,"%ld",&nr)==1&&nr==rm_nr()) {
            __atomic_add_fetch(&rm_blocked,1,__ATOMIC_RELEASE);return 1;
        }
        sched_yield();
    }
}
static void *rm_helper(void *unused) {
    (void)unused;
    while(!__atomic_load_n(&rm_go,__ATOMIC_ACQUIRE))sched_yield();
    /* Original first-EINTR or3restart-frame threshold, every interruption
     * follows a fresh actual blocked-syscall observation. No timed pacing. */
    while(rm_eobs<1&&rm_robs<3&&!rm_main_done) {
        if(!rm_observe())break;
        rm_kick();
    }
    rm_complete(rm_case);__atomic_store_n(&rm_done,1,__ATOMIC_RELEASE);return NULL;
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
        if (syscall(SYS_epoll_pwait2, rmx.ep, &e, 1, &z, NULL, 8) < 0) {
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
    case RM_SEMOP: {
        const char *id=getenv("SEEDBOX_UNIX_SEMID");if(!id){errno=EINVAL;return 0;}
        char *end;long value=strtol(id,&end,10);if(*end||value<0||value>INT_MAX){errno=EINVAL;return 0;}
        rmx.semid=(int)value;return semctl(rmx.semid,0,GETVAL)==0;
    }
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
        return syscall(SYS_futex_waitv, &w, 1, 0, NULL, CLOCK_MONOTONIC)==-1&&errno==EAGAIN;
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
    case RM_SEMOP: break; /* native owner reaps child then IPC_RMID */
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
        *ok = r == 1 && b=='z';
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
        /* seedbox-syscall-subject: nanosleep; bound-ms=200; invariant=actual blocked-entry and signal frame classify EINTR, source separate remainder has positive value at most original request. */
        RM_CALL((long)&req, (long)&rem, 0, 0, 0, 0, SYS_nanosleep);
        int remok = 1;
        if (r == -EINTR) {
            remok = rem.tv_sec == 0 && rem.tv_nsec > 0 && rem.tv_nsec <= RM_SLEEP_NS;
            /* seedbox-syscall-subject: nanosleep; bound-ms=200; invariant=source EINTR remainder is resumed to actual completion, no elapsed-time assertion. */
            while (nanosleep(&rem, &rem) && errno == EINTR) {}
        }
        *ok = remok && (r == 0 || r == -EINTR);
        break;
    }
    case RM_CLOCKABS: {
        uint64_t end = mono_ns(CLOCK_MONOTONIC) + RM_SLEEP_NS;
        struct timespec dl = {(time_t)(end / 1000000000ULL), (long)(end % 1000000000ULL)};
        /* seedbox-syscall-subject: clock_nanosleep; bound-ms=200; invariant=source absolute monotonic deadline with actual blocked-entry/EINTR frame and retried success, no elapsed deadline assertion. */
        do RM_CALL(CLOCK_MONOTONIC, TIMER_ABSTIME, (long)&dl, 0, 0, 0, SYS_clock_nanosleep); while (r == -EINTR);
        *ok = r == 0;
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
        *ok = r == 1 && b=='z';
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

int main(int argc,char **argv) {
    if(argc!=3)return 2;int c=-1;
    for(int i=0;i<RM_N;i++)if(!strcmp(argv[1],rm_names[i]))c=i;
    char *end;long mode=strtol(argv[2],&end,10);if(c<0||*end||mode<0||mode>2)return 2;
    rm_stream=malloc((1<<20)+RM_WTOTAL);if(!rm_stream)return 2;
    for(size_t i=0;i<(1<<20)+RM_WTOTAL;i++)rm_stream[i]=(unsigned char)(i%251);
    rm_tid=gettid();rm_case=c;rm_nested=mode==2;rm_stream_ok=1;
    struct sigaction nested={0};nested.sa_handler=rm_handler2;nested.sa_flags=SA_RESTART;sigemptyset(&nested.sa_mask);
    struct sigaction action={0};action.sa_sigaction=rm_handler;action.sa_flags=SA_SIGINFO|(mode?SA_RESTART:0);sigemptyset(&action.sa_mask);
    if(sigaction(SIGUSR1,&action,NULL)||sigaction(SIGUSR2,&nested,NULL))return 2;
    if(!rm_setup(c)){fprintf(stderr,"restart setup %s: %s (errno%d)\n",argv[1],strerror(errno),errno);return 2;}
    pthread_t helper;if(pthread_create(&helper,NULL,rm_helper,NULL))return 2;
    __atomic_store_n(&rm_go,1,__ATOMIC_RELEASE);
    int ok=0;long first=rm_run(c,&ok);__atomic_store_n(&rm_main_done,1,__ATOMIC_RELEASE);
    if(pthread_join(helper,NULL))return 2;rm_teardown(c);free(rm_stream);
    if(!rm_blocked){fprintf(stderr,"restart %s returned before positive blocked-entry: first%ld\n",argv[1],first);return first<0&&first!=-EINTR?2:1;}
    if(c==RM_WRITE)ok&=rm_stream_ok;
    ok&=rm_done&&rm_usr1>0&&!rm_bad_frame&&(rm_eobs>=1||rm_robs>=3);
    if(mode==2)ok&=rm_usr2==rm_usr1;else ok&=rm_usr2==0;
    fprintf(stderr,"restart %s mode%ld first%ld frames restart%d/eintr%d signals%d nested%d blocked%d badframe%d complete%d\n",argv[1],mode,first,rm_robs,rm_eobs,rm_usr1,rm_usr2,rm_blocked,rm_bad_frame,ok);
    fprintf(stderr,"restart signal identity diagnostic: code%d pid%d partial-writes%d\n",rm_signal_code,rm_signal_pid,rm_partial);
    if(!ok)return 1;
    const char *classification=first==-EINTR?"eintr":first>=0?(rm_robs>=3&&!rm_eobs?"restarted":"done"):"error";
    printf("OK restart %s mode%ld: completed payload frame nested blocked-entry first=%s\n",argv[1],mode,classification);return 0;
}
