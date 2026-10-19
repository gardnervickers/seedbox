#define _GNU_SOURCE
#include <signal.h>
#include <stdint.h>
#include <string.h>
#include <sys/mman.h>
#include <ucontext.h>
#include <unistd.h>

/* Finite application-owned code, never a third-party target. Each process
 * executes one NULL read and exits from its ordinary SA_SIGINFO handler. */
static unsigned char *code;
static void
handler(int sig, siginfo_t *info, void *context)
{
    ucontext_t *u = context;
#if defined(__x86_64__)
    uintptr_t pc = (uintptr_t)u->uc_mcontext.gregs[REG_RIP];
#elif defined(__aarch64__)
    uintptr_t pc = (uintptr_t)u->uc_mcontext.pc;
#else
# error Unsupported fixture architecture
#endif
    const int signal_ok = sig == SIGSEGV;
    const int address_ok = info->si_addr == NULL;
    const int pc_ok = pc == (uintptr_t)code;
    /* Only async-signal-safe interfaces, including on the failure path. */
    char result[] = "fault-context: signal-match=0 null-address=0 application-pc=0\n";
    result[28] = '0' + signal_ok;
    result[43] = '0' + address_ok;
    result[60] = '0' + pc_ok;
    if (write(STDOUT_FILENO, result, sizeof(result) - 1) != sizeof(result) - 1)
        _exit(4);
    _exit(signal_ok && address_ok && pc_ok ? 0 : 1);
}

int
main(int argc, char **argv)
{
    if (argc != 3 || (strcmp(argv[2], "shared") && strcmp(argv[2], "private")))
        return 2;
#if defined(__x86_64__)
    /* SysV argument RDI=NULL: CALL/JMP reads its indirect target at NULL. */
    unsigned char bytes[] = { 0xff, 0x27 };
    if (!strcmp(argv[1], "call"))
        bytes[1] = 0x17;
    else if (strcmp(argv[1], "jump"))
        return 2;
#elif defined(__aarch64__)
    /* A64 has no memory-indirect CTI. LDR X16,[X0]; RET tests the original
     * application fault-PC contract at the operand load, not x86 CTI mangling.
     * A BR/BLR suffix would never execute and would add no fault coverage. */
    const uint32_t bytes[] = { 0xf9400010, 0xd65f03c0 };
    if (strcmp(argv[1], "load"))
        return 2;
#endif
    struct sigaction action = { .sa_sigaction = handler, .sa_flags = SA_SIGINFO };
    if (sigemptyset(&action.sa_mask) || sigaction(SIGSEGV, &action, NULL))
        return 2;
    const long page = sysconf(_SC_PAGESIZE);
    if (page <= 0)
        return 2;
    unsigned char *rw;
    if (!strcmp(argv[2], "shared")) {
        int fd = memfd_create("fault-context-app", 0);
        if (fd < 0 || ftruncate(fd, page))
            return 2;
        rw = mmap(NULL, page, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
        code = mmap(NULL, page, PROT_READ | PROT_EXEC, MAP_SHARED, fd, 0);
        close(fd);
        if (rw == MAP_FAILED || code == MAP_FAILED)
            return 2;
    } else {
        code = mmap(NULL, page, PROT_READ | PROT_WRITE,
                    MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
        if (code == MAP_FAILED)
            return 2;
        rw = code;
    }
    memcpy(rw, bytes, sizeof(bytes));
    __builtin___clear_cache((char *)rw, (char *)rw + sizeof(bytes));
    if (rw == code && mprotect(code, page, PROT_READ | PROT_EXEC))
        return 2;
    __builtin___clear_cache((char *)code, (char *)code + sizeof(bytes));
    ((void (*)(void *))code)(NULL);
    return 3; /* Returning would mean the required fault was never delivered. */
}
