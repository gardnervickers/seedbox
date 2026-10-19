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
/* x86-64 adaptation of cfz.c t_nestsig: native syscall clobbers RCX/R11,
 * so check the other live callee/scratch GPRs, XMM8-XMM15 and EFLAGS. */
CFZ_HIDDEN long cfz_canary_kill(const uint64_t *,long,long,long);
CFZ_HIDDEN extern const char cfz_ck_after[];
asm(".text\n.p2align 2\n.globl cfz_canary_kill\n.hidden cfz_canary_kill\n.type cfz_canary_kill,@function\ncfz_canary_kill:\n"
    "push %rbx\npush %rbp\npush %r12\npush %r13\npush %r14\npush %r15\nsub $16,%rsp\nmov %rdi,(%rsp)\n"
    "mov %rdi,%r11\nmov %rcx,%r10\nmov %rsi,%rdi\nmov %rdx,%rsi\nmov %r10,%rdx\n"
    "mov 72(%r11),%rbx\n"
    "mov 80(%r11),%rbp\n"
    "mov 88(%r11),%r12\n"
    "mov 96(%r11),%r13\n"
    "mov 104(%r11),%r14\n"
    "mov 112(%r11),%r15\n"
    "mov 120(%r11),%r8\n"
    "mov 128(%r11),%r9\n"
    "mov 136(%r11),%r10\n"
    "movdqu 256(%r11),%xmm8\n"
    "movdqu 272(%r11),%xmm9\n"
    "movdqu 288(%r11),%xmm10\n"
    "movdqu 304(%r11),%xmm11\n"
    "movdqu 320(%r11),%xmm12\n"
    "movdqu 336(%r11),%xmm13\n"
    "movdqu 352(%r11),%xmm14\n"
    "movdqu 368(%r11),%xmm15\n"
    "pushq 504(%r11)\npopfq\nmov $234,%eax\nsyscall\n.globl cfz_ck_after\n.hidden cfz_ck_after\ncfz_ck_after:\npushfq\npop %rax\nmov %rax,8(%rsp)\nmov (%rsp),%rdx\nxor %edi,%edi\n"
    "cmp 72(%rdx),%rbx\nsetne %al\nmovzbl %al,%eax\nadd %rax,%rdi\n"
    "cmp 80(%rdx),%rbp\nsetne %al\nmovzbl %al,%eax\nadd %rax,%rdi\n"
    "cmp 88(%rdx),%r12\nsetne %al\nmovzbl %al,%eax\nadd %rax,%rdi\n"
    "cmp 96(%rdx),%r13\nsetne %al\nmovzbl %al,%eax\nadd %rax,%rdi\n"
    "cmp 104(%rdx),%r14\nsetne %al\nmovzbl %al,%eax\nadd %rax,%rdi\n"
    "cmp 112(%rdx),%r15\nsetne %al\nmovzbl %al,%eax\nadd %rax,%rdi\n"
    "cmp 120(%rdx),%r8\nsetne %al\nmovzbl %al,%eax\nadd %rax,%rdi\n"
    "cmp 128(%rdx),%r9\nsetne %al\nmovzbl %al,%eax\nadd %rax,%rdi\n"
    "cmp 136(%rdx),%r10\nsetne %al\nmovzbl %al,%eax\nadd %rax,%rdi\n"
    "movq %xmm8,%rax\ncmp 256(%rdx),%rax\nsetne %al\nmovzbl %al,%eax\nadd %rax,%rdi\n"
    "movhlps %xmm8,%xmm0\nmovq %xmm0,%rax\ncmp 264(%rdx),%rax\nsetne %al\nmovzbl %al,%eax\nadd %rax,%rdi\n"
    "movq %xmm9,%rax\ncmp 272(%rdx),%rax\nsetne %al\nmovzbl %al,%eax\nadd %rax,%rdi\n"
    "movhlps %xmm9,%xmm0\nmovq %xmm0,%rax\ncmp 280(%rdx),%rax\nsetne %al\nmovzbl %al,%eax\nadd %rax,%rdi\n"
    "movq %xmm10,%rax\ncmp 288(%rdx),%rax\nsetne %al\nmovzbl %al,%eax\nadd %rax,%rdi\n"
    "movhlps %xmm10,%xmm0\nmovq %xmm0,%rax\ncmp 296(%rdx),%rax\nsetne %al\nmovzbl %al,%eax\nadd %rax,%rdi\n"
    "movq %xmm11,%rax\ncmp 304(%rdx),%rax\nsetne %al\nmovzbl %al,%eax\nadd %rax,%rdi\n"
    "movhlps %xmm11,%xmm0\nmovq %xmm0,%rax\ncmp 312(%rdx),%rax\nsetne %al\nmovzbl %al,%eax\nadd %rax,%rdi\n"
    "movq %xmm12,%rax\ncmp 320(%rdx),%rax\nsetne %al\nmovzbl %al,%eax\nadd %rax,%rdi\n"
    "movhlps %xmm12,%xmm0\nmovq %xmm0,%rax\ncmp 328(%rdx),%rax\nsetne %al\nmovzbl %al,%eax\nadd %rax,%rdi\n"
    "movq %xmm13,%rax\ncmp 336(%rdx),%rax\nsetne %al\nmovzbl %al,%eax\nadd %rax,%rdi\n"
    "movhlps %xmm13,%xmm0\nmovq %xmm0,%rax\ncmp 344(%rdx),%rax\nsetne %al\nmovzbl %al,%eax\nadd %rax,%rdi\n"
    "movq %xmm14,%rax\ncmp 352(%rdx),%rax\nsetne %al\nmovzbl %al,%eax\nadd %rax,%rdi\n"
    "movhlps %xmm14,%xmm0\nmovq %xmm0,%rax\ncmp 360(%rdx),%rax\nsetne %al\nmovzbl %al,%eax\nadd %rax,%rdi\n"
    "movq %xmm15,%rax\ncmp 368(%rdx),%rax\nsetne %al\nmovzbl %al,%eax\nadd %rax,%rdi\n"
    "movhlps %xmm15,%xmm0\nmovq %xmm0,%rax\ncmp 376(%rdx),%rax\nsetne %al\nmovzbl %al,%eax\nadd %rax,%rdi\n"
    "mov 8(%rsp),%rax\nxor 504(%rdx),%rax\nand $0x8d5,%eax\nsetne %al\nmovzbl %al,%eax\nadd %rdi,%rax\nadd $16,%rsp\npop %r15\npop %r14\npop %r13\npop %r12\npop %rbp\npop %rbx\nret\n");
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
    can[63] = 0x202 | (((level * 5 + sig) & 1) ? 0x41 : 0);
    long bad = cfz_canary_kill(can, getpid(), ns_tid, sig);
    ns_canbad += bad;
}
static int ns_onalt(uintptr_t sp) { return sp >= (uintptr_t)ns_alt.ss_sp && sp < (uintptr_t)ns_alt.ss_sp + ns_alt.ss_size; }
static void ns_frame(int s, const siginfo_t *si, const ucontext_t *uc) {
    if (uc->uc_mcontext.gregs[REG_RIP] != (uintptr_t)cfz_ck_after) ns_pcbad++;
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


int main(void) { return !t_nestsig(); }
