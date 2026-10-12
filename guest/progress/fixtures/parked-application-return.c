/* Real translated blocking read, nested signal and saved middle-body return.
 * No client callback stands in for a syscall or application signal handler. */
#define _GNU_SOURCE
#include "parked-publication-context.h"
#include <errno.h>
#include <fcntl.h>
#include <linux/futex.h>
#include <pthread.h>
#include <signal.h>
#include <sched.h>
#include <stdatomic.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <time.h>
#include <ucontext.h>
#include <unistd.h>

enum { PAGE = 4096, ITERATIONS = 20, READ_WORD = 32, EDIT_WORD = 16 };
static uint32_t *code;
static void *poll_page;
static uint64_t fake_thread[512], load_value = 0x13579bdf2468ace0ULL;
static struct parked_state outer, read_state, read_return, outer_return;
static atomic_int ready, handler_ack, nested_seen, published, finished;
static int pipefd[2], iteration;
static pid_t consumer;
static unsigned faults, nested, reads, edits;
static uint64_t read_result, read_effects;
static unsigned char read_byte;
extern uint64_t parked_enter(struct parked_state *, struct parked_state *);

static void fail(const char *what)
{
    fprintf(stderr, "FAIL parked-application-return: %s iteration=%d errno=%d\n",
            what, iteration, errno);
    _exit(1);
}

static void wake(atomic_int *word)
{
    syscall(SYS_futex, word, FUTEX_WAKE_PRIVATE, 1, NULL, NULL, 0);
}

static void wait_equal(atomic_int *word, int value)
{
    int old;
    while ((old = atomic_load_explicit(word, memory_order_acquire)) != value)
        syscall(SYS_futex, word, FUTEX_WAIT_PRIVATE, old, NULL, NULL, 0);
}

static uint64_t micros(void)
{
    struct timespec t;
    if (clock_gettime(CLOCK_MONOTONIC, &t)) fail("clock");
    return t.tv_sec * 1000000ULL + t.tv_nsec / 1000;
}

static void pattern(struct parked_state *s, int salt)
{
    memset(s, 0, sizeof(*s));
    for (int r = 0; r < 31; ++r)
        s->gpr[r] = 0x8172635445362718ULL ^ ((uint64_t)r << 40) ^ salt;
    for (int r = 0; r < 32; ++r)
        for (int b = 0; b < 16; ++b) s->q[r][b] = (r * 19 + b * 11 + salt) & 255;
    s->nzcv = ((uint64_t)salt & 15) << 28;
    s->fpsr = salt & 0x1f;
    s->fpcr = (salt & 3) << 22;
}

static void check_frame(ucontext_t *uc, const struct parked_state *want, uint64_t pc,
                        int in_outer_handler)
{
    if (uc->uc_mcontext.pc != pc || uc->uc_mcontext.sp != want->sp ||
        (uc->uc_mcontext.pstate & 0xf0000000) != want->nzcv ||
        (uintptr_t)__builtin_thread_pointer() != want->tls)
        fail("frame PC/SP/NZCV/TLS");
    for (int r = 0; r < 31; ++r)
        if (uc->uc_mcontext.regs[r] != want->gpr[r]) {
            fprintf(stderr, "BAD x%d got=%llx want=%llx pc=%llx\n", r,
                    (unsigned long long)uc->uc_mcontext.regs[r],
                    (unsigned long long)want->gpr[r], (unsigned long long)uc->uc_mcontext.pc);
            fail("frame GPR");
        }
    const struct fpsimd_context *fp = (void *)uc->uc_mcontext.__reserved;
    if (fp->head.magic != FPSIMD_MAGIC || fp->head.size != sizeof(*fp) ||
        fp->fpsr != want->fpsr || fp->fpcr != want->fpcr ||
        memcmp(fp->vregs, want->q, sizeof(want->q))) fail("frame Q/FP");
    sigset_t current;
    if (sigprocmask(SIG_SETMASK, NULL, &current)) fail("read mask");
    for (int sig = 1; sig < NSIG; ++sig) {
        int saved = sig == SIGUSR2 || (in_outer_handler && sig == SIGSEGV);
        int active = sig == SIGUSR2 || sig == SIGSEGV || (in_outer_handler && sig == SIGUSR1);
        if (!!sigismember(&uc->uc_sigmask, sig) != saved ||
            !!sigismember(&current, sig) != active) fail("saved/current mask");
    }
}

static void signal_handler(int sig, siginfo_t *info, void *context)
{
    ucontext_t *uc = context;
    if (sig == SIGUSR1) {
        if (atomic_load(&handler_ack) != iteration || atomic_load(&published) != 0 ||
            info->si_code != SI_QUEUE || info->si_pid != getpid() || info->si_uid != getuid() ||
            info->si_value.sival_int != 0x4200 + iteration)
            fail("nested signal ordering/payload");
        /* SA_RESTART exposes the original SVC PC and its original arguments. */
        check_frame(uc, &read_state, (uintptr_t)(code + READ_WORD), 1);
        ++nested;
        atomic_store_explicit(&nested_seen, iteration, memory_order_release);
        wake(&nested_seen);
        return;
    }
    if (sig != SIGSEGV || info->si_addr != poll_page ||
        atomic_load(&published) != 0 || faults != (unsigned)iteration - 1)
        fail("unexpected/duplicate/late outer poll");
    struct parked_state at_poll = outer;
    at_poll.gpr[8] = (uintptr_t)poll_page;
    at_poll.gpr[9] = load_value;
    check_frame(uc, &at_poll, (uintptr_t)(code + 2), 0);
    ++faults;
    atomic_store_explicit(&handler_ack, iteration, memory_order_release);
    wake(&handler_ack);
    uint64_t value = parked_enter(&read_state, &read_return);
    if (atomic_load_explicit(&published, memory_order_acquire) != iteration ||
        read_result != 1 || read_effects != 1 || read_byte != (unsigned char)iteration ||
        value != 0x700U + iteration) fail("read result/effect/current post-read body");
    struct parked_state want_read = read_state;
    want_read.gpr[0] = 0x700U + iteration;
    want_read.gpr[9] = 1;
    if (memcmp(&read_return, &want_read, sizeof(read_return))) fail("read return full state");
    ++reads;
    /* Nested delivery and the blocked syscall must not mutate the outer frame. */
    check_frame(uc, &at_poll, (uintptr_t)(code + 2), 0);
    if (iteration % 2 == 0) {
        uc->uc_mcontext.pc = (uintptr_t)(code + EDIT_WORD);
        ++edits;
    }
    /* Unchanged odd return targets the middle of the newly published body. */
}

static void wait_for_kernel_read(void)
{
    char path[100], data[512];
    snprintf(path, sizeof(path), "/proc/self/task/%d/syscall", consumer);
    for (;;) {
        int fd = open(path, O_RDONLY | O_CLOEXEC);
        if (fd < 0) fail("open syscall observation");
        ssize_t n = read(fd, data, sizeof(data) - 1);
        if (close(fd) || n < 0) fail("read syscall observation");
        data[n] = 0;
        long number;
        unsigned long arg0, arg1, arg2, arg3, arg4, arg5, sp, pc;
        if (sscanf(data, "%ld %lx %lx %lx %lx %lx %lx %lx %lx", &number, &arg0, &arg1,
                   &arg2, &arg3, &arg4, &arg5, &sp, &pc) == 9 && number == SYS_read &&
            arg0 == (unsigned)pipefd[0] && arg1 == (uintptr_t)&read_byte && arg2 == 1) {
            printf("KERNEL_READ iteration=%d syscall=%ld pc=%#lx sp=%#lx fd=%lu\n",
                   iteration, number, pc, sp, arg0);
            return;
        }
        sched_yield(); /* Observation only; never releases the pipe. */
    }
}

static void *publisher(void *unused)
{
    (void)unused;
    for (int i = 1; i <= ITERATIONS; ++i) {
        wait_equal(&ready, i);
        wait_equal(&handler_ack, i);
        wait_for_kernel_read();
        siginfo_t si = {0};
        si.si_signo = SIGUSR1; si.si_code = SI_QUEUE;
        si.si_pid = getpid(); si.si_uid = getuid(); si.si_value.sival_int = 0x4200 + i;
        if (syscall(SYS_rt_tgsigqueueinfo, getpid(), consumer, SIGUSR1, &si)) fail("nested send");
        wait_equal(&nested_seen, i);
        wait_for_kernel_read(); /* The unchanged nested return really restarted read. */
        code[0] = 0xc85f7d49; /* An entry restart would alter the expected path. */
        code[2] = 0xd503201f; /* saved middle-body fault PC becomes NOP */
        code[3] = 0xd2800000 | ((0x600U + i) << 5);
        code[EDIT_WORD] = 0xd503201f;
        code[EDIT_WORD + 1] = 0xd2800000 | ((0x800U + i) << 5);
        code[READ_WORD + 5] = 0xd2800000 | ((0x700U + i) << 5);
        uint64_t start = micros();
        __builtin___clear_cache((char *)code, (char *)(code + READ_WORD + 7));
        uint64_t elapsed = micros() - start;
        if (read_effects != 0 || reads != (unsigned)i - 1) fail("read released before publication");
        atomic_store_explicit(&published, i, memory_order_release);
        printf("RETURN_PUBLICATION iteration=%d outer_pc=%p edited_pc=%p read_pc=%p "
               "read_effects=0 done=%d latency_us=%llu\n", i, (void *)(code + 2),
               (void *)(code + EDIT_WORD), (void *)(code + READ_WORD + 1), i,
               (unsigned long long)elapsed);
        unsigned char byte = i;
        if (write(pipefd[1], &byte, 1) != 1) fail("pipe release");
        wait_equal(&finished, i);
    }
    return NULL;
}

int main(void)
{
    setvbuf(stdout, NULL, _IONBF, 0);
    if (pipe(pipefd)) fail("pipe");
    consumer = syscall(SYS_gettid);
    code = mmap(NULL, PAGE, PROT_READ | PROT_WRITE | PROT_EXEC,
                MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    poll_page = mmap(NULL, PAGE, PROT_READ, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (code == MAP_FAILED || poll_page == MAP_FAILED) fail("mmap");
    fake_thread[1112 / 8] = (uintptr_t)poll_page;
    struct sigaction sa = {.sa_sigaction = signal_handler, .sa_flags = SA_SIGINFO | SA_RESTART};
    sigemptyset(&sa.sa_mask);
    if (sigaction(SIGSEGV, &sa, NULL) || sigaction(SIGUSR1, &sa, NULL)) fail("sigaction");
    sigset_t mask;
    sigemptyset(&mask); sigaddset(&mask, SIGUSR2);
    if (pthread_sigmask(SIG_BLOCK, &mask, NULL)) fail("mask");
    pthread_t thread;
    if (pthread_create(&thread, NULL, publisher, NULL)) fail("pthread_create");
    printf("BEGIN case=application-return iterations=20 outer=%p poll=%p read=%p edit=%p x28=%p\n",
           (void *)code, (void *)(code + 2), (void *)(code + READ_WORD),
           (void *)(code + EDIT_WORD), (void *)fake_thread);
    for (iteration = 1; iteration <= ITERATIONS; ++iteration) {
        pattern(&outer, iteration);
        outer.gpr[10] = (uintptr_t)&load_value;
        outer.gpr[16] = (uintptr_t)code;
        outer.gpr[28] = (uintptr_t)fake_thread;
        pattern(&read_state, iteration + 71);
        read_state.gpr[0] = pipefd[0]; read_state.gpr[1] = (uintptr_t)&read_byte;
        read_state.gpr[2] = 1; read_state.gpr[8] = SYS_read;
        read_state.gpr[10] = (uintptr_t)&read_result;
        read_state.gpr[11] = (uintptr_t)&read_effects;
        read_state.gpr[16] = (uintptr_t)(code + READ_WORD);
        read_state.gpr[28] = (uintptr_t)fake_thread;
        read_result = read_effects = read_byte = 0;
        code[0] = 0xf9400149; code[1] = 0xf9422f88; code[2] = 0xb940011f;
        code[3] = 0xd2800000; code[4] = 0xd65f03c0;
        code[EDIT_WORD] = 0xd503201f; code[EDIT_WORD + 1] = 0xd2800000;
        code[EDIT_WORD + 2] = 0xd65f03c0;
        uint32_t read_code[] = {0xd4000001, 0xf9000140, 0xf9400169, 0x91000529,
                                0xf9000169, 0xd2800000, 0xd65f03c0};
        memcpy(code + READ_WORD, read_code, sizeof(read_code));
        __builtin___clear_cache((char *)code, (char *)(code + READ_WORD + 7));
        if (mprotect(poll_page, PAGE, PROT_NONE)) fail("arm");
        atomic_store(&published, 0);
        atomic_store_explicit(&ready, iteration, memory_order_release); wake(&ready);
        uint64_t value = parked_enter(&outer, &outer_return);
        uint64_t want_value = (iteration % 2 ? 0x600U : 0x800U) + iteration;
        struct parked_state want = outer;
        want.gpr[0] = want_value; want.gpr[8] = (uintptr_t)poll_page; want.gpr[9] = load_value;
        if (value != want_value || memcmp(&outer_return, &want, sizeof(want)) ||
            faults != (unsigned)iteration || nested != (unsigned)iteration ||
            reads != (unsigned)iteration) fail("outer return full state/body/counts");
        if (mprotect(poll_page, PAGE, PROT_READ)) fail("disarm");
        printf("ITERATION case=application-return iteration=%d faults=1 nested=1 reads=1 "
               "effects=1 edited=%d outer_body=%llu read_body=%u all_gpr=31 all_q=32 bad=0\n",
               iteration, iteration % 2 == 0, (unsigned long long)value, 0x700U + iteration);
        atomic_store_explicit(&finished, iteration, memory_order_release); wake(&finished);
    }
    if (pthread_join(thread, NULL)) fail("join");
    printf("END case=application-return completed=20 faults=%u nested=%u reads=%u edits=%u bad=0\n",
           faults, nested, reads, edits);
    close(pipefd[0]); close(pipefd[1]);
    return 0;
}
