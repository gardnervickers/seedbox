/* Public PV application-state contract: every taken branch admits a progress
 * checkpoint, while the scratch register, arithmetic flags, stack and SIMD
 * values remain live. A synchronous fault also exposes the restored app state
 * to a signal handler before resuming the same checks. ARM64 also checks x0 and
 * x30, which a checkpoint that re-enters the code cache directly restores
 * through the fragment prefix and the clean call's link-register slot. No
 * pacing is involved.
 *
 * On ARM64 the client keeps a block's budget in a register the block
 * overwrites before reading, and its application value only in thread-local
 * storage until that overwrite. Three faults bound that window for x17:
 *   0. before the overwrite: the handler must see the old value;
 *   1. after an overwrite with a different value, in the block that begins
 *      with it: the handler must see the new value, so the window has closed;
 *   2. in the overwriting instruction itself (a load from an unmapped
 *      address): the handler must see the value from before the load.
 * Faults 0 and 1 are permanently undefined instructions (`.inst 0`, UDF).
 * The fixture relies on the translator decoding UDF as an instruction that
 * names no register, so the scan for a block's register continues past it. */
#define _GNU_SOURCE
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <ucontext.h>

static volatile sig_atomic_t faults;
static volatile sig_atomic_t context_failure;

static void fault_handler(int signal, siginfo_t *info, void *opaque) {
    (void)info;
    ucontext_t *context = opaque;
#if defined(__x86_64__)
    if (signal != SIGILL ||
        context->uc_mcontext.gregs[REG_RCX] != UINT64_C(0x12345678) ||
        (context->uc_mcontext.gregs[REG_EFL] & 0x8d5) != 0x894)
        context_failure = 1;
    context->uc_mcontext.gregs[REG_RIP] += 2; /* ud2 */
#elif defined(__aarch64__)
    /* x17 at each fault, and the signal and instruction that raise it. */
    static const struct {
        int signal;
        uint64_t x17;
    } expected[] = {
        { SIGILL, UINT64_C(0x12345678) },
        { SIGILL, UINT64_C(0x43210000) },
        { SIGSEGV, UINT64_C(0x43210000) },
    };
    if (faults >= (sig_atomic_t)(sizeof(expected) / sizeof(expected[0])) ||
        signal != expected[faults].signal ||
        context->uc_mcontext.regs[17] != expected[faults].x17 ||
        context->uc_mcontext.regs[0] != UINT64_C(0x12345678) ||
        context->uc_mcontext.regs[1] != UINT64_C(0x12345678) ||
        context->uc_mcontext.regs[16] != UINT64_C(0x12345678) ||
        context->uc_mcontext.regs[30] != UINT64_C(0x12345678) ||
        (context->uc_mcontext.pstate & UINT64_C(0xf0000000)) != UINT64_C(0x90000000))
        context_failure = 1;
    context->uc_mcontext.pc += 4; /* the faulting instruction */
#else
#error This fixture checks ARM64 or x86-64 PV instrumentation only
#endif
    ++faults;
}

extern int state_branches(void);
#if defined(__x86_64__)
__asm__(
    ".text\n"
    ".type state_branches,@function\n"
    "state_branches:\n"
    "sub $128, %rsp\n"
    "mov %rsp, %r8\n"
    "mov $0x12345678, %ecx\n"
    "mov %rcx, (%rsp)\n"
    "mov %rcx, 120(%rsp)\n"
    "mov %rcx, -16(%rsp)\n"
    "mov %rcx, -128(%rsp)\n"
    "movq %rcx, %xmm0\n"
    "punpcklqdq %xmm0, %xmm0\n"
    "mov $30000, %r9d\n"
    "mov $0x7fffffff, %eax\n"
    "add $1, %eax\n"
    "ud2\n"
    "jmp 2f\n"
    "1:\n"
    "mov $0x7fffffff, %eax\n"
    "add $1, %eax\n"
    ".rept 16\n"
    "jmp 2f\n"
    "2:\n"
    ".endr\n"
    "pushfq\n"
    "pop %rdx\n"
    "and $0x8d5, %edx\n"
    "cmp $0x894, %edx\n"
    "jne 3f\n"
    "cmp $0x12345678, %rcx\n"
    "jne 4f\n"
    "cmp %rsp, %r8\n"
    "jne 5f\n"
    "cmp %rcx, (%rsp)\n"
    "jne 5f\n"
    "cmp %rcx, 120(%rsp)\n"
    "jne 5f\n"
    "cmp %rcx, -16(%rsp)\n"
    "jne 5f\n"
    "cmp %rcx, -128(%rsp)\n"
    "jne 5f\n"
    "movq %xmm0, %rdx\n"
    "cmp %rcx, %rdx\n"
    "jne 6f\n"
    "movdqa %xmm0, %xmm1\n"
    "psrldq $8, %xmm1\n"
    "movq %xmm1, %rdx\n"
    "cmp %rcx, %rdx\n"
    "jne 6f\n"
    "dec %r9\n"
    "jnz 1b\n"
    "xor %eax, %eax\n"
    "jmp 7f\n"
    "3: mov $1, %eax\n jmp 7f\n"
    "4: mov $2, %eax\n jmp 7f\n"
    "5: mov $3, %eax\n jmp 7f\n"
    "6: mov $4, %eax\n"
    "7: add $128, %rsp\n ret\n"
    ".size state_branches,.-state_branches\n");
#elif defined(__aarch64__)
__asm__(
    ".text\n"
    ".type state_branches,%function\n"
    "state_branches:\n"
    "sub sp, sp, #32\n"
    "str x30, [sp, #8]\n"
    "mov x8, sp\n"
    "mov x1, #0x5678\n"
    "movk x1, #0x1234, lsl #16\n"
    "mov x0, x1\n"
    "mov x16, x1\n"
    "mov x17, x1\n"
    "mov x30, x1\n"
    "str x1, [sp]\n"
    "str x1, [sp, #24]\n"
    "dup v0.2d, x1\n"
    "mov x9, #30000\n"
    "mov w10, #0x7fffffff\n"
    "adds w10, w10, #1\n"
    /* A conditional branch always ends a block (an unconditional one to the
     * next instruction may be elided), so the fault starts the next block. */
    "cbnz x9, 0f\n"
    "0:\n"
    ".inst 0\n" /* fault 0: x17 still has its first value */
    /* The handler resumes here, in a new block that begins with the
     * overwrite, so the next fault comes after that block's window. */
    "movz x17, #0x4321, lsl #16\n"
    ".inst 0\n" /* fault 1: x17 has the new value */
    /* The block from here overwrites x17 with a load that faults. x17 comes
     * before x2 in the client's order of preference. */
    "mov x2, #16\n"
    "ldr x17, [x2]\n" /* fault 2: x17 keeps the value from before the load */
    "mov x17, x1\n"
    "b 2f\n"
    "1:\n"
    "mov w10, #0x7fffffff\n"
    "adds w10, w10, #1\n"
    ".rept 16\n"
    "b 2f\n"
    "2:\n"
    ".endr\n"
    "mrs x2, nzcv\n"
    "mov x3, #0x90000000\n"
    "cmp x2, x3\n"
    "b.ne 3f\n"
    "mov x3, #0x5678\n"
    "movk x3, #0x1234, lsl #16\n"
    "cmp x0, x3\n"
    "b.ne 4f\n"
    "cmp x1, x3\n"
    "b.ne 4f\n"
    "cmp x16, x3\n"
    "b.ne 4f\n"
    "cmp x17, x3\n"
    "b.ne 4f\n"
    "cmp x30, x3\n"
    "b.ne 4f\n"
    "mov x2, sp\n"
    "cmp x2, x8\n"
    "b.ne 5f\n"
    "ldr x2, [sp]\n"
    "cmp x2, x3\n"
    "b.ne 5f\n"
    "ldr x2, [sp, #24]\n"
    "cmp x2, x3\n"
    "b.ne 5f\n"
    "fmov x2, d0\n"
    "cmp x2, x3\n"
    "b.ne 6f\n"
    "mov x2, v0.d[1]\n"
    "cmp x2, x3\n"
    "b.ne 6f\n"
    "subs x9, x9, #1\n"
    "b.ne 1b\n"
    "mov w0, #0\n"
    "b 7f\n"
    "3: mov w0, #1\n b 7f\n"
    "4: mov w0, #2\n b 7f\n"
    "5: mov w0, #3\n b 7f\n"
    "6: mov w0, #4\n"
    "7: ldr x30, [sp, #8]\n add sp, sp, #32\n ret\n"
    ".size state_branches,.-state_branches\n");
#endif

int state_fixture(void) {
#if defined(__aarch64__)
    enum { FAULTS = 3 };
#else
    enum { FAULTS = 1 };
#endif
    struct sigaction action = { .sa_sigaction = fault_handler, .sa_flags = SA_SIGINFO };
    sigemptyset(&action.sa_mask);
    if (sigaction(SIGILL, &action, NULL) || sigaction(SIGSEGV, &action, NULL)) return 84;
    int result = state_branches();
    if (result || context_failure || faults != FAULTS) {
        fprintf(stderr, "STATE failure=%d context=%d faults=%d\n", result,
                (int)context_failure, (int)faults);
        return 85;
    }
    printf("STATE=OK FAULTS=%d\n", FAULTS);
    return 0;
}
