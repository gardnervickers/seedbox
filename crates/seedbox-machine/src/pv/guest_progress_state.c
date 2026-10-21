/* Public PV application-state contract: every taken branch admits a progress
 * checkpoint, while the scratch register, arithmetic flags, stack and SIMD
 * values remain live. A synchronous fault also exposes the restored app state
 * to a signal handler before resuming the same checks. No pacing is involved. */
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
    if (signal != SIGILL) context_failure = 1;
#if defined(__x86_64__)
    if (context->uc_mcontext.gregs[REG_RCX] != UINT64_C(0x12345678) ||
        (context->uc_mcontext.gregs[REG_EFL] & 0x8d5) != 0x894)
        context_failure = 1;
    context->uc_mcontext.gregs[REG_RIP] += 2; /* ud2 */
#elif defined(__aarch64__)
    if (context->uc_mcontext.regs[1] != UINT64_C(0x12345678) ||
        (context->uc_mcontext.pstate & UINT64_C(0xf0000000)) != UINT64_C(0x90000000))
        context_failure = 1;
    context->uc_mcontext.pc += 4; /* permanently undefined instruction */
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
    "mov x8, sp\n"
    "mov x1, #0x5678\n"
    "movk x1, #0x1234, lsl #16\n"
    "str x1, [sp]\n"
    "str x1, [sp, #24]\n"
    "dup v0.2d, x1\n"
    "mov x9, #30000\n"
    "mov w10, #0x7fffffff\n"
    "adds w10, w10, #1\n"
    ".inst 0\n"
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
    "cmp x1, x3\n"
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
    "7: add sp, sp, #32\n ret\n"
    ".size state_branches,.-state_branches\n");
#endif

int state_fixture(void) {
    struct sigaction action = { .sa_sigaction = fault_handler, .sa_flags = SA_SIGINFO };
    sigemptyset(&action.sa_mask);
    if (sigaction(SIGILL, &action, NULL)) return 84;
    int result = state_branches();
    if (result || context_failure || faults != 1) {
        fprintf(stderr, "STATE failure=%d context=%d faults=%d\n", result,
                (int)context_failure, (int)faults);
        return 85;
    }
    puts("STATE=OK FAULTS=1");
    return 0;
}
