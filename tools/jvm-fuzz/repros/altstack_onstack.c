/*
 * sigaltstack(NULL, &old) from a handler running on the alternate stack. Linux
 * reports SS_ONSTACK whenever the stack pointer is inside the alternate stack;
 * natively it prints "OK". Found by harness/acceptance/workloads/checkpoint-altstack.yaml.
 *
 * Usage: altstack_onstack [ROUNDS]
 */
#define _GNU_SOURCE
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/mman.h>
#include <unistd.h>

#define SIZE (64 * 1024)
static char *stack;
static volatile int inside, onstack, flags;

static void handler(int number) {
    (void)number;
    volatile char local = 0;
    stack_t current;
    inside = (char *)&local >= stack && (char *)&local < stack + SIZE;
    onstack = sigaltstack(NULL, &current) == 0 && (current.ss_flags & SS_ONSTACK);
    flags = current.ss_flags;
}

int main(int argc, char **argv) {
    int rounds = argc > 1 ? atoi(argv[1]) : 10, failures = 0;
    stack = mmap(NULL, SIZE, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    stack_t alternate = {.ss_sp = stack, .ss_size = SIZE};
    struct sigaction action = {.sa_handler = handler, .sa_flags = SA_ONSTACK};
    if (stack == MAP_FAILED || sigaltstack(&alternate, NULL) || sigaction(SIGUSR1, &action, NULL)) {
        perror("setup");
        return 2;
    }
    for (int i = 0; i < rounds; ++i) {
        raise(SIGUSR1);
        if (!inside || !onstack) {
            if (!failures++) printf("round %d: handler on the alternate stack %d, ss_flags %#x\n", i, inside, flags);
        }
    }
    stack_t after;
    sigaltstack(NULL, &after);
    if (after.ss_flags & SS_ONSTACK) { printf("SS_ONSTACK outside the handler\n"); ++failures; }
    printf(failures ? "FAIL %d of %d rounds\n" : "OK\n", failures, rounds);
    return failures != 0;
}
