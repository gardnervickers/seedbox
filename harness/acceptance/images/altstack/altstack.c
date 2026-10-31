/* A signal handler blocked on an alternate stack across a checkpoint.
 *
 * A worker thread runs fixed blocks of floating-point and vector work and
 * checks each block against a table computed before any signal. On "park" the
 * control thread sends the worker SIGUSR1; its handler runs on a sigaltstack,
 * keeps scalar and vector sentinels in locals on that stack, sets FPCR to
 * round-toward-zero, and blocks in read(2) on a release pipe from assembly with
 * x19-x28 and q8-q31 holding sentinels. "parked" is answered only once
 * /proc shows the worker sleeping there. The oracle captures, then sends
 * "release TOKEN"; the handler checks every register, FPCR, its stack locals
 * and that it ran on the alternate stack, returns through rt_sigreturn, and the
 * interrupted worker must keep computing correct blocks. The result line is
 * deterministic continuation evidence.
 */
#define _GNU_SOURCE
#include <arpa/inet.h>
#include <errno.h>
#include <inttypes.h>
#include <netinet/in.h>
#include <pthread.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/socket.h>
#include <sys/syscall.h>
#include <ucontext.h>
#include <unistd.h>

#define PORT 7300
#define ALTSTACK (64 * 1024)
#define BLOCKS 64
#define AFTER_RELEASE 8
#define REGISTERS (10 + 2 * 24)

typedef double v2df __attribute__((vector_size(16)));

static double reference[BLOCKS];
static int release_pipe[2], parked_pipe[2];
static volatile pid_t worker_tid;
static char *altstack;
static volatile int released, finished;
static volatile uint64_t blocks, block_errors, blocks_at_release;

static struct {
    uint64_t token, read_result, onstack, on_altstack, locals_ok, fpcr_ok, registers_ok;
    uint64_t bad_register, interrupted_on_altstack;
} handler;

/* One block: deterministic scalar and vector arithmetic in registers. */
static double __attribute__((noinline)) block(unsigned index) {
    v2df a = {1.0 + index, 2.0 - index * 0.5}, b = {0.999, 1.001}, c = {0.0, 0.0};
    double s = index * 0.25;
    for (unsigned i = 0; i < 20000; ++i) {
        a = a * b + (v2df){1e-9 * i, -1e-9 * i};
        c += a;
        s = s * 0.9999 + c[0] * 1e-12 - c[1] * 1e-12;
    }
    return s + c[0] + c[1];
}

/* x0=fd x1=buffer x2=length x3=registers out. Loads sentinels into x19-x28 and
 * q8-q31, calls read(2) with them live, then stores them for the caller. */
extern long sentinel_read(int fd, void *buffer, size_t length, uint64_t *out);
__asm__(
    ".text\n.global sentinel_read\n.type sentinel_read, %function\nsentinel_read:\n"
    "  stp x29, x30, [sp, #-176]!\n  mov x29, sp\n"
    "  stp x19, x20, [sp, #16]\n  stp x21, x22, [sp, #32]\n  stp x23, x24, [sp, #48]\n"
    "  stp x25, x26, [sp, #64]\n  stp x27, x28, [sp, #80]\n"
    "  stp d8, d9, [sp, #96]\n  stp d10, d11, [sp, #112]\n  stp d12, d13, [sp, #128]\n"
    "  stp d14, d15, [sp, #144]\n  str x3, [sp, #160]\n"
    "  movz x9, #0x5ee1, lsl #48\n"
    "  add x19, x9, #19\n  add x20, x9, #20\n  add x21, x9, #21\n  add x22, x9, #22\n"
    "  add x23, x9, #23\n  add x24, x9, #24\n  add x25, x9, #25\n  add x26, x9, #26\n"
    "  add x27, x9, #27\n  add x28, x9, #28\n"
    "  movz x10, #0xa5a5, lsl #48\n  movz x11, #0x5a5a, lsl #48\n"
#define Q(n) "  add x12, x10, #" #n "\n  add x13, x11, #" #n "\n  mov v" #n ".d[0], x12\n  mov v" #n ".d[1], x13\n"
    Q(8) Q(9) Q(10) Q(11) Q(12) Q(13) Q(14) Q(15) Q(16) Q(17) Q(18) Q(19)
    Q(20) Q(21) Q(22) Q(23) Q(24) Q(25) Q(26) Q(27) Q(28) Q(29) Q(30) Q(31)
#undef Q
    "  mov x8, #63\n  svc #0\n"
    "  ldr x3, [sp, #160]\n"
    "  stp x19, x20, [x3, #0]\n  stp x21, x22, [x3, #16]\n  stp x23, x24, [x3, #32]\n"
    "  stp x25, x26, [x3, #48]\n  stp x27, x28, [x3, #64]\n  add x3, x3, #80\n"
    "  stp q8, q9, [x3, #0]\n  stp q10, q11, [x3, #32]\n  stp q12, q13, [x3, #64]\n"
    "  stp q14, q15, [x3, #96]\n  stp q16, q17, [x3, #128]\n  stp q18, q19, [x3, #160]\n"
    "  stp q20, q21, [x3, #192]\n  stp q22, q23, [x3, #224]\n  stp q24, q25, [x3, #256]\n"
    "  stp q26, q27, [x3, #288]\n  stp q28, q29, [x3, #320]\n  stp q30, q31, [x3, #352]\n"
    "  ldp x19, x20, [sp, #16]\n  ldp x21, x22, [sp, #32]\n  ldp x23, x24, [sp, #48]\n"
    "  ldp x25, x26, [sp, #64]\n  ldp x27, x28, [sp, #80]\n"
    "  ldp d8, d9, [sp, #96]\n  ldp d10, d11, [sp, #112]\n  ldp d12, d13, [sp, #128]\n"
    "  ldp d14, d15, [sp, #144]\n"
    "  ldp x29, x30, [sp], #176\n  ret\n"
    ".size sentinel_read, .-sentinel_read\n");

static int on_altstack(const void *address) {
    return (const char *)address >= altstack && (const char *)address < altstack + ALTSTACK;
}

static void on_signal(int number, siginfo_t *info, void *context) {
    (void)number; (void)info;
    volatile uint64_t scalars[8];
    volatile v2df vectors[4];
    for (int i = 0; i < 8; ++i) scalars[i] = 0x51C0000000000000ull | (uint64_t)i;
    for (int i = 0; i < 4; ++i) vectors[i] = (v2df){i + 0.125, -i - 0.375};
    stack_t current;
    handler.onstack = sigaltstack(NULL, &current) == 0 && (current.ss_flags & SS_ONSTACK);
    handler.on_altstack = on_altstack((const void *)scalars);
    ucontext_t *uc = context;
    handler.interrupted_on_altstack = on_altstack((const void *)uc->uc_mcontext.sp);
    uint64_t fpcr, zero_rounding = 3ull << 22, saved;
    __asm__ volatile("mrs %0, fpcr" : "=r"(saved));
    __asm__ volatile("msr fpcr, %0" :: "r"(saved | zero_rounding));
    char note = 'p';
    if (write(parked_pipe[1], &note, 1) != 1) _exit(3);
    uint64_t registers[REGISTERS];
    char token[32] = {0};
    long count = sentinel_read(release_pipe[0], token, sizeof(token) - 1, registers);
    handler.read_result = (uint64_t)count;
    handler.token = strtoull(token, NULL, 10);
    __asm__ volatile("mrs %0, fpcr" : "=r"(fpcr));
    handler.fpcr_ok = fpcr == (saved | zero_rounding);
    __asm__ volatile("msr fpcr, %0" :: "r"(saved));
    handler.registers_ok = 1;
    for (int i = 0; i < 10; ++i) {
        if (registers[i] != (0x5ee1ull << 48) + 19 + i) { handler.registers_ok = 0; handler.bad_register = 19 + i; }
    }
    for (int i = 0; i < 24; ++i) {
        if (registers[10 + 2 * i] != (0xa5a5ull << 48) + 8 + i
            || registers[11 + 2 * i] != (0x5a5aull << 48) + 8 + i) {
            handler.registers_ok = 0; handler.bad_register = 100 + 8 + i;
        }
    }
    handler.locals_ok = 1;
    for (int i = 0; i < 8; ++i) handler.locals_ok &= scalars[i] == (0x51C0000000000000ull | (uint64_t)i);
    for (int i = 0; i < 4; ++i) {
        v2df v = vectors[i];
        handler.locals_ok &= v[0] == i + 0.125 && v[1] == -i - 0.375;
    }
    blocks_at_release = blocks;
    released = 1;
}

static void *worker(void *unused) {
    (void)unused;
    worker_tid = (pid_t)syscall(SYS_gettid);
    stack_t stack = {.ss_sp = altstack, .ss_size = ALTSTACK, .ss_flags = 0};
    if (sigaltstack(&stack, NULL)) { perror("sigaltstack"); exit(2); }
    uint64_t after = 0;
    for (unsigned b = 0;; ++b) {
        if (block(b % BLOCKS) != reference[b % BLOCKS]) ++block_errors;
        ++blocks;
        if (released && ++after == AFTER_RELEASE) break;
    }
    finished = 1;
    return NULL;
}

static char state_of(pid_t tid) {
    char path[64], text[512];
    snprintf(path, sizeof(path), "/proc/self/task/%d/stat", (int)tid);
    FILE *f = fopen(path, "r");
    if (!f) return '?';
    size_t n = fread(text, 1, sizeof(text) - 1, f);
    fclose(f);
    text[n] = 0;
    char *end = strrchr(text, ')');
    return end && end[1] == ' ' ? end[2] : '?';
}

static void serve(FILE *in, FILE *out) {
    while (!worker_tid) usleep(1000);
    char line[128];
    while (fgets(line, sizeof(line), in)) {
        if (!strncmp(line, "park", 4)) {
            while (blocks < 4) usleep(1000);  /* Interrupt a computation under way. */
            if (syscall(SYS_tgkill, getpid(), worker_tid, SIGUSR1)) { perror("tgkill"); exit(2); }
            char note;
            if (read(parked_pipe[0], &note, 1) != 1) exit(2);
            while (state_of(worker_tid) != 'S') usleep(1000);
            fprintf(out, "parked blocks_before=%" PRIu64 "\n", blocks);
        } else if (!strncmp(line, "release ", 8)) {
            const char *token = line + 8;
            if (write(release_pipe[1], token, strcspn(token, "\n")) < 0) exit(2);
            while (!finished) usleep(1000);
            fprintf(out,
                    "{\"token\": %" PRIu64 ", \"read\": %" PRIu64 ", \"onstack\": %" PRIu64
                    ", \"on_altstack\": %" PRIu64 ", \"interrupted_on_altstack\": %" PRIu64
                    ", \"locals_ok\": %" PRIu64 ", \"fpcr_ok\": %" PRIu64 ", \"registers_ok\": %" PRIu64
                    ", \"bad_register\": %" PRIu64 ", \"blocks\": %" PRIu64 ", \"blocks_at_release\": %" PRIu64
                    ", \"block_errors\": %" PRIu64 "}\n",
                    handler.token, handler.read_result, handler.onstack, handler.on_altstack,
                    handler.interrupted_on_altstack, handler.locals_ok, handler.fpcr_ok,
                    handler.registers_ok, handler.bad_register, blocks, blocks_at_release, block_errors);
        } else {
            fprintf(out, "error unknown request\n");
        }
        fflush(out);
    }
}

int main(void) {
    for (unsigned b = 0; b < BLOCKS; ++b) reference[b] = block(b);
    if (pipe(release_pipe) || pipe(parked_pipe)) { perror("pipe"); return 2; }
    altstack = mmap(NULL, ALTSTACK, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (altstack == MAP_FAILED) { perror("mmap"); return 2; }
    struct sigaction action = {.sa_sigaction = on_signal, .sa_flags = SA_SIGINFO | SA_ONSTACK};
    sigemptyset(&action.sa_mask);
    if (sigaction(SIGUSR1, &action, NULL)) { perror("sigaction"); return 2; }
    pthread_t thread;
    if (pthread_create(&thread, NULL, worker, NULL)) { perror("pthread_create"); return 2; }
    int listener = socket(AF_INET, SOCK_STREAM, 0), one = 1;
    setsockopt(listener, SOL_SOCKET, SO_REUSEADDR, &one, sizeof(one));
    struct sockaddr_in address = {.sin_family = AF_INET, .sin_port = htons(PORT)};
    if (bind(listener, (struct sockaddr *)&address, sizeof(address)) || listen(listener, 4)) {
        perror("listen"); return 2;
    }
    printf("ALTSTACK_READY\n");
    fflush(stdout);
    for (;;) {
        int channel = accept(listener, NULL, NULL);
        if (channel < 0) continue;
        FILE *in = fdopen(channel, "r"), *out = fdopen(dup(channel), "w");
        serve(in, out);
        fclose(in); fclose(out);
    }
}
