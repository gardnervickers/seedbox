#define _GNU_SOURCE
#include <errno.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <ucontext.h>
#include <unistd.h>

extern int register_probe(void), signal_probe(void);
extern char signal_fault[], signal_resume[];
#ifdef __aarch64__
extern int exclusive_probe(void);
#endif
static unsigned char alternate[128 * 1024] __attribute__((aligned(16)));
static volatile sig_atomic_t delivered, handler_offset, handler_stack_flags;

#define CHECK(test) do { if (!(test)) { \
    static const char message[] = "fixture check failed: " #test "\n"; \
    ssize_t ignored = write(STDERR_FILENO, message, sizeof(message) - 1); \
    (void)ignored; _exit(70); \
} } while (0)

static void handle(int number, siginfo_t *info, void *raw)
{
    ucontext_t *uc = raw;
    unsigned char local;
    stack_t stack;
    CHECK(number == SIGILL && info->si_signo == SIGILL);
    CHECK((uintptr_t)&local >= (uintptr_t)alternate &&
          (uintptr_t)&local < (uintptr_t)(alternate + sizeof(alternate)));
    handler_offset = (sig_atomic_t)((uintptr_t)&local - (uintptr_t)alternate);
    CHECK(sigaltstack(NULL, &stack) == 0);
    handler_stack_flags = stack.ss_flags;
    /* PR317 f32472b7, tools/jvm-fuzz/repros/sigaltstack_onstack.c:
     * keep its in-handler EPERM check beside the existing SS_ONSTACK oracle.
     * This public Linux API scenario is shared by x86-64 and AArch64.
     */
    stack_t again = { .ss_sp = alternate, .ss_size = sizeof(alternate) };
    CHECK(sigaltstack(&again, NULL) == -1 && errno == EPERM);
    CHECK(sigismember(&uc->uc_sigmask, SIGUSR1) == 1);
    CHECK(sigaddset(&uc->uc_sigmask, SIGUSR2) == 0);
#ifdef __x86_64__
    CHECK(uc->uc_mcontext.gregs[REG_RIP] == (greg_t)signal_fault);
    CHECK(uc->uc_mcontext.gregs[REG_RAX] == 17);
    uc->uc_mcontext.gregs[REG_RAX] = 42;
    uc->uc_mcontext.gregs[REG_RIP] = (greg_t)signal_resume;
    __asm__ volatile("pxor %%xmm0, %%xmm0" ::: "xmm0");
#else
    CHECK(uc->uc_mcontext.pc == (uintptr_t)signal_fault);
    CHECK(uc->uc_mcontext.regs[0] == 17);
    uc->uc_mcontext.regs[0] = 42;
    uc->uc_mcontext.pc = (uintptr_t)signal_resume;
    __asm__ volatile("movi v0.16b, #0" ::: "v0");
#endif
    ++delivered;
}

static void signals(void)
{
    stack_t stack = { .ss_sp = alternate, .ss_size = sizeof(alternate) };
    struct sigaction action = { .sa_sigaction = handle, .sa_flags = SA_SIGINFO | SA_ONSTACK };
    sigset_t mask;
    CHECK(sigemptyset(&action.sa_mask) == 0);
    CHECK(sigaltstack(&stack, NULL) == 0);
    CHECK(sigaction(SIGILL, &action, NULL) == 0);
    CHECK(sigemptyset(&mask) == 0 && sigaddset(&mask, SIGUSR1) == 0);
    CHECK(sigprocmask(SIG_SETMASK, &mask, NULL) == 0);
    for (int round = 1; round <= 2; ++round) {
        CHECK(signal_probe() == 0);
        CHECK(delivered == round);
        CHECK(sigprocmask(SIG_SETMASK, NULL, &mask) == 0);
        CHECK(sigismember(&mask, SIGUSR1) == 1 && sigismember(&mask, SIGUSR2) == 1);
        CHECK(sigismember(&mask, SIGILL) == 0);
        CHECK(sigaltstack(NULL, &stack) == 0 && stack.ss_flags == 0);
        CHECK(stack.ss_sp == alternate && stack.ss_size == sizeof(alternate));
        if (!(handler_stack_flags & SS_ONSTACK))
            fprintf(stderr, "alternate stack: checked handler offset=%d size=%zu reported flags=%d\n",
                    handler_offset, sizeof(alternate), handler_stack_flags);
        CHECK(handler_stack_flags & SS_ONSTACK);
    }
    puts("signal: context, vector, mask, alternate stack restored twice");
}

int main(int argc, char **argv)
{
    CHECK(argc == 2);
    if (strcmp(argv[1], "signal") == 0) {
        signals();
    } else if (strcmp(argv[1], "registers") == 0) {
        CHECK(register_probe() == 0);
        puts("registers: live values and flags preserved");
#ifdef __aarch64__
    } else if (strcmp(argv[1], "exclusive") == 0) {
        CHECK(exclusive_probe() == 0);
        puts("exclusive: stored value and load/base overlap preserved");
#endif
    } else {
        CHECK(0);
    }
    return 0;
}
