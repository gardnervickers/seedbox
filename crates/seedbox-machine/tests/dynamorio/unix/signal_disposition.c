/* Seedbox R20 public signal-disposition regression. A cooperating sender waits
 * for a handler/client receipt before supplying each read's payload. No libc
 * read retry may conceal a synthetic EINTR or a syscall that never executed. */
#define _GNU_SOURCE
#include <pthread.h>
#include <sched.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/syscall.h>
#include <unistd.h>

#define ROUNDS 256
/* Exported application mailbox, accessed through the public module/symbol API. */
volatile int disposition_receipts;
static volatile sig_atomic_t handled;
static int requests[2], payloads[2], signal_number;
static pid_t main_tid;

static void handler(int sig) {
    (void)sig;
    handled++;
    __atomic_add_fetch(&disposition_receipts, 1, __ATOMIC_RELEASE);
}

static long raw_read(int fd, void *buffer, unsigned long size) {
#ifdef __aarch64__
    register long x0 __asm__("x0") = fd;
    register void *x1 __asm__("x1") = buffer;
    register unsigned long x2 __asm__("x2") = size;
    register long x8 __asm__("x8") = SYS_read;
    __asm__ volatile("svc #0" : "+r"(x0) : "r"(x1), "r"(x2), "r"(x8) : "memory", "cc");
    return x0;
#else
    long result;
    __asm__ volatile("syscall" : "=a"(result) : "a"((long)SYS_read), "D"((long)fd),
                     "S"(buffer), "d"(size) : "rcx", "r11", "memory", "cc");
    return result;
#endif
}

static void *sender(void *unused) {
    (void)unused;
    for (int i = 0; i < ROUNDS; i++) {
        unsigned char request, payload = (unsigned char)(i * 37 + 11);
        if (read(requests[0], &request, 1) != 1 || request != (unsigned char)i)
            _exit(2);
        if (syscall(SYS_tgkill, getpid(), main_tid, signal_number)) _exit(2);
        while (__atomic_load_n(&disposition_receipts, __ATOMIC_ACQUIRE) < i + 1)
            sched_yield();
        if (write(payloads[1], &payload, 1) != 1) _exit(2);
    }
    return NULL;
}

int main(int argc, char **argv) {
    if (argc != 2) return 2;
    int native = !strcmp(argv[1], "native");
    int ignored = !strcmp(argv[1], "ignore");
    int defaulted = !strcmp(argv[1], "default");
    int bypass = !strcmp(argv[1], "bypass");
    if (!native && !ignored && !defaulted && !bypass && strcmp(argv[1], "suppress")) return 2;
    /* SIG_IGN for ordinary signals is retained in the kernel and produces no
     * client event. Explicitly send a fault-class signal, which the public API
     * continues to intercept even when ignored; no faulting instruction is used. */
    signal_number = ignored ? SIGSEGV : defaulted ? SIGWINCH : bypass ? SIGURG : SIGUSR1;
    main_tid = (pid_t)syscall(SYS_gettid);
    struct sigaction action = {0};
    action.sa_handler = ignored ? SIG_IGN : defaulted ? SIG_DFL : handler;
    action.sa_flags = SA_RESTART;
    sigemptyset(&action.sa_mask);
    if (sigaction(signal_number, &action, NULL) || pipe(requests) || pipe(payloads)) return 2;
    sigset_t mask;
    sigemptyset(&mask);
    sigaddset(&mask, signal_number);
    if (pthread_sigmask(SIG_BLOCK, &mask, NULL)) return 2;
    pthread_t worker;
    if (pthread_create(&worker, NULL, sender, NULL)) return 2;
    if (pthread_sigmask(SIG_UNBLOCK, &mask, NULL)) return 2;
    for (int i = 0; i < ROUNDS; i++) {
        unsigned char request = (unsigned char)i, payload = 0;
        if (write(requests[1], &request, 1) != 1) return 2;
        long result = raw_read(payloads[0], &payload, 1);
        if (result != 1 || payload != (unsigned char)(i * 37 + 11)) {
            fprintf(stderr, "round %d raw read %ld payload %u\n", i, result, payload);
            return 1;
        }
    }
    if (pthread_join(worker, NULL)) return 2;
    if (__atomic_load_n(&disposition_receipts, __ATOMIC_ACQUIRE) != ROUNDS ||
        handled != (native ? ROUNDS : 0)) return 1;
    close(requests[0]); close(requests[1]); close(payloads[0]); close(payloads[1]);
    puts("reads 256 payloads 256 receipts 256 rawEintr 0\nOK");
    return 0;
}
