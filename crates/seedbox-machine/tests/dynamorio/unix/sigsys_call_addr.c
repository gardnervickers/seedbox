/* Source-derived from Seedbox PR317 f32472b7fe470367b7eddecde1ac53ad299f3a3e,
 * tools/jvm-fuzz/repros/sigsys_call_addr.c. No DR internal API or layout is used.
 * Adaptations are described in inventory.tsv and the integration handoff. */
/*
 * A seccomp SECCOMP_RET_TRAP filter turns getppid into SIGSYS. The kernel reports
 * the address after the trapping svc in both si_call_addr and the frame's pc; a
 * seccomp-based emulator uses si_call_addr to find the instruction.
 *
 *   sigsys_call_addr [ROUNDS]
 *
 * Natively prints OK.
 */
#define _GNU_SOURCE
#include "support.h"
#include <linux/filter.h>
#include <linux/seccomp.h>
#include <signal.h>
#include <stddef.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/prctl.h>
#include <sys/syscall.h>
#include <ucontext.h>
#include <unistd.h>

long raw_getppid(void);
extern const char after_svc[];
#if defined(__aarch64__)
asm(".text\n.p2align 2\n.globl raw_getppid\nraw_getppid:\nmov x8, #173\nsvc #0\n.globl after_svc\nafter_svc:\nret\n");
#else
asm(".text\n.p2align 2\n.globl raw_getppid\nraw_getppid:\nmov $110, %eax\nsyscall\n.globl after_svc\nafter_svc:\nret\n");
#endif

static volatile uintptr_t call_addr, pc;
static volatile int nr, code;
static void sigsys(int s, siginfo_t *si, void *ucv) {
    (void)s;
    ucontext_t *uc = ucv;
    call_addr = (uintptr_t)si->si_call_addr;
    nr = si->si_syscall;
    code = si->si_code;
#if defined(__aarch64__)
    pc = uc->uc_mcontext.pc;
    uc->uc_mcontext.regs[0] = 1234;
#else
    pc = uc->uc_mcontext.gregs[REG_RIP];
    uc->uc_mcontext.gregs[REG_RAX] = 1234;
#endif
}

int main(int argc, char **argv) {
    int rounds = argc > 1 ? atoi(argv[1]) : 5;
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
    if (prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) || prctl(PR_SET_SECCOMP, SECCOMP_MODE_FILTER, &prog)) {
        puts("UNSUPPORTED seccomp");
        return 2;
    }
    int bad = 0;
    for (int i = 0; i < rounds; i++) {
        long r = raw_getppid();
        int ok = r == 1234 && code == 1 /* SYS_SECCOMP */ && nr == SYS_getppid && pc == (uintptr_t)after_svc &&
                 call_addr == (uintptr_t)after_svc;
        printf("round %d: ret %ld code %d nr %d pc %s si_call_addr %s%s\n", i, r, code, nr,
               pc == (uintptr_t)after_svc ? "ok" : "WRONG", call_addr == (uintptr_t)after_svc ? "ok" : "WRONG",
               ok ? "" : "  <-- wrong");
        fprintf(stderr,"call address %#lx expected %#lx\n",(unsigned long)call_addr,(unsigned long)(uintptr_t)after_svc);
        bad += !ok;
    }
    puts(bad ? "FAIL" : "OK");
    return bad != 0;
}
