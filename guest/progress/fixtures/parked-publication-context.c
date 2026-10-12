/* Bounded ownership discriminator; no timeout releases a parked consumer. */
#define _GNU_SOURCE
#include "parked-publication-context.h"
#include <errno.h>
#include <linux/futex.h>
#include <pthread.h>
#include <signal.h>
#include <stdatomic.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <time.h>
#include <ucontext.h>
#include <unistd.h>

enum { PAGE = 4096, ITERATIONS = 20 };
static uint32_t *heap;
static void *poll_page;
static uint64_t load_value = 0x13579bdf2468ace0ULL;
static uint64_t fake_thread[512];
static struct parked_state expected;
static atomic_int req, ack, signal_seen, publication_done, finished, ready;
static unsigned faults, signals, bad;
static pid_t consumer;
extern uint64_t parked_enter(struct parked_state *state);

static void fail(const char *what)
{
    fprintf(stderr, "FAIL parked-publication-context: %s errno=%d\n", what, errno);
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

static void check_context(ucontext_t *uc, int poll)
{
    uint64_t pc = (uintptr_t)heap + (poll ? 8 : 0);
    if (uc->uc_mcontext.pc != pc || uc->uc_mcontext.sp != expected.sp ||
        (uc->uc_mcontext.pstate & 0xf0000000) != expected.nzcv ||
        (uintptr_t)__builtin_thread_pointer() != expected.tls)
        fail("PC/SP/NZCV/TLS");
    for (int i = 0; i < 31; ++i) {
        uint64_t want = poll && i == 8 ? (uintptr_t)poll_page :
                        poll && i == 9 ? load_value : expected.gpr[i];
        if (uc->uc_mcontext.regs[i] != want) {
            fprintf(stderr, "BAD GPR x%d got=%llx want=%llx pc=%llx\n", i,
                    (unsigned long long)uc->uc_mcontext.regs[i],
                    (unsigned long long)want, (unsigned long long)uc->uc_mcontext.pc);
            fail("GPR");
        }
    }
    const struct fpsimd_context *fp = (void *)uc->uc_mcontext.__reserved;
    if (fp->head.magic != FPSIMD_MAGIC || fp->head.size != sizeof(*fp) ||
        fp->fpsr != expected.fpsr || fp->fpcr != expected.fpcr ||
        memcmp(fp->vregs, expected.q, sizeof(expected.q)))
        fail("Q/FPSR/FPCR");
    /* SIGUSR2 is intentionally blocked by the application, not by the client. */
    if (!sigismember(&uc->uc_sigmask, SIGUSR2) ||
        sigismember(&uc->uc_sigmask, SIGUSR1) ||
        sigismember(&uc->uc_sigmask, SIGSEGV))
        fail("saved signal mask");
}

static void handler(int sig, siginfo_t *info, void *context)
{
    int iteration = atomic_load_explicit(&req, memory_order_acquire);
    if (sig == SIGUSR1) {
        if (iteration != 1 || atomic_load(&publication_done) != 0)
            fail("signal outside active wait");
        check_context(context, 0);
        ++signals;
        atomic_store_explicit(&signal_seen, signals, memory_order_release);
        wake(&signal_seen);
        return; /* Must recheck req before any application body instruction. */
    }
    if (sig != SIGSEGV || info->si_addr != poll_page || iteration != 0 ||
        atomic_load(&publication_done) != (int)faults + 1)
        fail("poll before publication/release or unexpected fault");
    check_context(context, 1);
    if (mprotect(poll_page, PAGE, PROT_READ)) fail("disarm");
    ++faults;
}

static void *publisher(void *unused)
{
    (void)unused;
    for (int i = 1; i <= ITERATIONS; ++i) {
        wait_equal(&ready, i);
        wait_equal(&ack, 1);
        if (atomic_load(&req) != 1) fail("ack without request");
        if (syscall(SYS_tgkill, getpid(), consumer, SIGUSR1)) fail("tgkill");
        wait_equal(&signal_seen, i);
        /* Signal return has an unchanged PC; publication must not need release. */
        heap[PAGE / 4 + 64 + i] = 0xd503201f;
        if (mprotect(poll_page, PAGE, PROT_NONE)) fail("arm");
        heap[0] = 0xc85f7d49; /* original LDR -> LDXR x9,[x10] */
        heap[3] = 0xd2800000 | ((0x600U + i) << 5); /* current observable body */
        uint64_t start = micros();
        __builtin___clear_cache((char *)heap, (char *)(heap + 4));
        uint64_t elapsed = micros() - start;
        if (atomic_load(&req) != 1 || faults != (unsigned)i - 1)
            fail("publication depended on early body/release");
        atomic_store_explicit(&publication_done, i, memory_order_release);
        printf("PUBLICATION iteration=%d req=1 ack=%d done=%d latency_us=%llu\n",
               i, atomic_load(&ack), i, (unsigned long long)elapsed);
        atomic_store_explicit(&req, 0, memory_order_release);
        wake(&req);
        wait_equal(&finished, i);
    }
    return NULL;
}

int main(int argc, char **argv)
{
    if (argc != 2) fail("usage: parked-publication-context TARGET_FILE");
    setvbuf(stdout, NULL, _IONBF, 0);
    heap = mmap(NULL, 2 * PAGE, PROT_READ | PROT_WRITE | PROT_EXEC,
                MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    poll_page = mmap(NULL, PAGE, PROT_READ, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (heap == MAP_FAILED || poll_page == MAP_FAILED) fail("mmap");
    fake_thread[1112 / 8] = (uintptr_t)poll_page;
    heap[0] = 0xf9400149;
    heap[1] = 0xf9422f88;
    heap[2] = 0xb940011f;
    heap[3] = 0xd2800000;
    heap[4] = 0xd65f03c0;
    heap[PAGE / 4] = 0xd65f03c0;
    __builtin___clear_cache((char *)heap, (char *)heap + 2 * PAGE);
    FILE *file = fopen(argv[1], "wx");
    if (!file) fail("exclusive target file");
    fprintf(file, "%lx %lx %lx\n", (uintptr_t)heap, (uintptr_t)&req, (uintptr_t)&ack);
    if (fclose(file)) fail("target close");
    consumer = syscall(SYS_gettid);
    struct sigaction sa = {.sa_sigaction = handler, .sa_flags = SA_SIGINFO};
    sigemptyset(&sa.sa_mask);
    if (sigaction(SIGSEGV, &sa, NULL) || sigaction(SIGUSR1, &sa, NULL)) fail("sigaction");
    sigset_t blocked;
    sigemptyset(&blocked);
    sigaddset(&blocked, SIGUSR2);
    if (pthread_sigmask(SIG_BLOCK, &blocked, NULL)) fail("mask");
    pthread_t writer;
    if (pthread_create(&writer, NULL, publisher, NULL)) fail("pthread_create");
    printf("BEGIN case=restartable iterations=%d target=%p poll=%p x28=%p\n",
           ITERATIONS, (void *)heap, (void *)(heap + 2), (void *)fake_thread);
    for (int i = 1; i <= ITERATIONS; ++i) {
        for (int r = 0; r < 31; ++r)
            expected.gpr[r] = 0x1020304050607080ULL ^ ((uint64_t)r << 40) ^ i;
        for (int r = 0; r < 32; ++r)
            for (int b = 0; b < 16; ++b) expected.q[r][b] = (r * 17 + b * 7 + i) & 255;
        expected.gpr[10] = (uintptr_t)&load_value;
        expected.gpr[16] = (uintptr_t)heap;
        expected.gpr[28] = (uintptr_t)fake_thread;
        expected.nzcv = ((uint64_t)i & 15) << 28;
        expected.fpsr = i & 0x1f;
        expected.fpcr = ((i & 3) << 22); /* independent rounding-mode patterns */
        heap[0] = 0xf9400149;
        heap[3] = 0xd2800000;
        __builtin___clear_cache((char *)heap, (char *)(heap + 4));
        atomic_store(&publication_done, 0);
        /* The publisher sees this iteration only after ack is reset. */
        atomic_store(&ack, 0);
        atomic_store_explicit(&req, 1, memory_order_release);
        atomic_store_explicit(&ready, i, memory_order_release);
        wake(&ready);
        uint64_t result = parked_enter(&expected);
        if (result != 0x600U + i || faults != (unsigned)i || signals != (unsigned)i)
            fail("stale body, missing fault or signal");
        printf("ITERATION case=restartable iteration=%d faults=1 signals=1 bad=0 "
               "body=%llu all_gpr=31 all_q=32 state=valid\n", i,
               (unsigned long long)result);
        atomic_store_explicit(&finished, i, memory_order_release);
        wake(&finished);
    }
    if (pthread_join(writer, NULL)) fail("pthread_join");
    printf("END case=restartable completed=%d faults=%u signals=%u bad=%u\n",
           ITERATIONS, faults, signals, bad);
    unlink(argv[1]);
    return 0;
}
