/* unaligned_ldxr: a misaligned ldxr (crossing a 16-byte boundary, and crossing into
 * a PROT_NONE page) raises SIGBUS BUS_ADRALN at the ldxr with si_addr = the access.
 * Natively prints both cases "SIGBUS code 1 addrOk 1 at ldxr" and "OK"; under the
 * translator the first faults at the stxr and the second is SIGSEGV SEGV_ACCERR
 * at the page boundary. */
#define _GNU_SOURCE
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/mman.h>
#include <ucontext.h>

uint64_t xinc(void *p);
extern const char xinc_ld[], xinc_st[];
asm(".text\n.globl xinc\nxinc:\n1:\n.globl xinc_ld\nxinc_ld: ldxr x2, [x0]\nadd x2, x2, #1\n"
    ".globl xinc_st\nxinc_st: stxr w3, x2, [x0]\ncbnz w3, 1b\nmov x0, x2\nret\n");
static volatile int sig, code;
static volatile uintptr_t addr, pc;
static void h(int s, siginfo_t *si, void *ucv) {
    ucontext_t *uc = ucv;
    sig = s;
    code = si->si_code;
    addr = (uintptr_t)si->si_addr;
    pc = uc->uc_mcontext.pc;
    uc->uc_mcontext.pc = uc->uc_mcontext.regs[30];
}
static int one(const char *what, char *p) {
    sig = 0;
    xinc(p);
    const char *at = pc == (uintptr_t)xinc_ld ? "ldxr" : pc == (uintptr_t)xinc_st ? "stxr" : "elsewhere";
    printf("%s: %s code %d addrOk %d at %s\n", what, sig == SIGBUS ? "SIGBUS" : sig == SIGSEGV ? "SIGSEGV" : sig ? "other" : "none",
           code, addr == (uintptr_t)p, sig ? at : "-");
    return sig == SIGBUS && code == BUS_ADRALN && addr == (uintptr_t)p && pc == (uintptr_t)xinc_ld;
}
int main(void) {
    char *m = mmap(0, 2 * 4096, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    mprotect(m + 4096, 4096, PROT_NONE);
    struct sigaction a;
    memset(&a, 0, sizeof a);
    a.sa_sigaction = h;
    a.sa_flags = SA_SIGINFO | SA_NODEFER;
    sigaction(SIGBUS, &a, NULL);
    sigaction(SIGSEGV, &a, NULL);
    int ok = one("cross16", m + 0x10c) & one("intoProtNone", m + 4096 - 4);
    printf("%s\n", ok ? "OK" : "BAD");
    return !ok;
}
