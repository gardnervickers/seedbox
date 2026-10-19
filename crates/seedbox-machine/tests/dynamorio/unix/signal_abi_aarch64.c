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

/* Fault sites: f(addr, nzcv << 28, &sp) records sp, sets NZCV, then faults at f_at. */
CFZ_HIDDEN void cfz_sf_store(void *, uint64_t, uint64_t *);
CFZ_HIDDEN void cfz_sf_ldar(void *, uint64_t, uint64_t *);
CFZ_HIDDEN void cfz_sf_brk(void *, uint64_t, uint64_t *);
CFZ_HIDDEN void cfz_sf_udf(void *, uint64_t, uint64_t *);
CFZ_HIDDEN void cfz_sf_spin(volatile int *, uint64_t, uint64_t *);
CFZ_HIDDEN extern const char cfz_sf_store_at[], cfz_sf_ldar_at[], cfz_sf_brk_at[], cfz_sf_udf_at[], cfz_sf_spin_loop[],
    cfz_sf_spin_end[];
asm(".text\n.p2align 2\n"
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
        sched_yield();
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
            sched_yield();
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
    unix_emit(slot,v);
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


static void fixed_work(long n) { volatile uint64_t x=1;for(long i=0;i<n;i++)x=x*1664525+1013904223; }
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


int main(int argc,char **argv) { setvbuf(stdout,NULL,_IOLBF,0); if(argc!=2)return 2;
if(!strcmp(argv[1],"sigshare"))return !t_sigshare();
if(!strcmp(argv[1],"sigframe"))return !t_sigframe();
if(!strcmp(argv[1],"ctxrewrite"))return !t_ctxrewrite();
if(!strcmp(argv[1],"nestsig"))return !t_nestsig();
return 2; }
